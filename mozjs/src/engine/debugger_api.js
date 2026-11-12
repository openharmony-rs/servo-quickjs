/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

// SpiderMonkey's Debugger API (https://firefox-source-docs.mozilla.org/js/Debugger/), as far as
// Servo's devtools use it, on QuickJS. `debugger.rs` evaluates this file in a debugger global and
// calls the function with its natives; it defines the `Debugger` constructor this returns on the
// global, and calls `host` when a script is compiled and from QuickJS's debugger hooks.
//
// Debuggee values are QuickJS values of other realms of the same runtime: there are no
// compartments, so debugger code reaches them directly. This file only uses intrinsics captured
// when it runs. It calls debuggee functions and evaluates debuggee code only through natives that
// return completions and report terminations; as in SpiderMonkey, the traps of JS Proxies still run
// when it inspects them (their properties, prototype or extensibility).
//
// QuickJS's WeakMaps are not ephemerons: an entry whose value references its key keeps the key
// alive. So the maps from debuggee values and handles to the Debugger.* objects that reference them
// hold those objects weakly.
//
// Positions: QuickJS's are where errors can be thrown, in code points. With the step positions the
// parser records while a debugger is active (patch Q49: statements, initializers, conditions, the
// closing braces of functions and the end of scripts), the breakpoint positions of a script are
// SpiderMonkey's: those, and calls at SpiderMonkey's coordinates (the property name of `o.f()`; the
// start of `f` in `f()`, unless `f` starts the statement and there are arguments, then the `(`; the
// `new` of `new F()`), in UTF-16 code units.
(function (native) {
    "use strict";

    const { create, defineProperty, freeze, getOwnPropertyDescriptor, getOwnPropertyNames,
        getOwnPropertySymbols, getPrototypeOf, isExtensible, isFrozen, isSealed } = Object;
    const { apply } = Reflect;
    const ReflectGet = Reflect.get;
    const ProxyCtor = Proxy;
    const MapCtor = Map;
    const SetCtor = Set;
    const WeakMapCtor = WeakMap;
    const WeakSetCtor = WeakSet;
    const WeakRefCtor = WeakRef;
    const WeakRefDeref = WeakRef.prototype.deref;
    const ArrayFrom = Array.from;
    const StringCtor = String;
    const ReferenceErrorCtor = ReferenceError;
    const TypeErrorCtor = TypeError;
    const identifierRegExp = /^[\p{ID_Start}$_][\p{ID_Continue}$\u200c\u200d]*$/u;
    const RegExpTest = RegExp.prototype.test;
    const surrogateRegExp = /[\ud800-\udbff]/;
    // The debugger's own global, which can't be a debuggee.
    const ownGlobal = globalThis;

    // The kinds of `[kind, value]` completions of the natives.
    const RETURN = 0;
    const THROW = 1;

    // What hook callbacks return to the shim.
    const CONTINUE = 0;
    const TERMINATE = 1;

    // `[status, value]` of variables.
    const VALUE = 0;
    const UNINITIALIZED = 1;
    const MISSING = 2;

    // How a frame stopped (`JSFrameExitKind`).
    const EXIT_RETURN = 0;
    const EXIT_THROW = 1;
    const EXIT_SUSPEND = 3;

    // Variable kinds (`JSVariableKind`) and what closure variables bind to (`JSBindingKind`).
    const VARIABLE_ARG = 0;
    const VARIABLE_LOCAL = 1;
    const VARIABLE_CLOSURE = 2;
    const BINDING_FUNCTION = 0;
    const BINDING_MODULE = 3;

    // Calls (`JSCallKind`).
    const CALL_NONE = 0;
    const CALL_FUNCTION = 1;
    const CALL_METHOD = 2;
    const CALL_CONSTRUCTOR = 3;

    // `native.functionKind` results.
    const NOT_A_FUNCTION = -1;
    const NATIVE = 0;
    const GENERATOR = 2;
    const ASYNC = 3;
    const ASYNC_GENERATOR = 4;
    const BOUND = 5;

    // The values of variables that have none.
    const uninitialized = freeze({ uninitialized: true });
    const optimizedOut = freeze({ optimizedOut: true });

    // Only this file creates instances of the Debugger.* classes.
    const TOKEN = {};

    function isObject(v) {
        return (typeof v === "object" && v !== null) || typeof v === "function";
    }

    // The value of a WeakRef, or undefined.
    function deref(ref) {
        return ref === undefined ? undefined : apply(WeakRefDeref, ref, []);
    }

    function isIdentifier(name) {
        return typeof name === "string" && name !== "this" && apply(RegExpTest, identifierRegExp, [name]);
    }

    // The global a debuggee argument designates: a global, a WindowProxy, or a Debugger.Object
    // of one.
    function debuggeeGlobal(v, what) {
        const g = native.toGlobal(DebuggerObject.is(v) ? DebuggerObject.referent(v) : v);
        if (g === undefined) {
            throw new TypeErrorCtor(`${what}: argument is not a global object`);
        }
        return g;
    }

    function checkHook(name, hook) {
        if (hook !== undefined && typeof hook !== "function") {
            throw new TypeErrorCtor(`Debugger.${name} must be a function or undefined`);
        }
        return hook;
    }

    // Sources, scripts and positions, shared by the debuggers of this emulation.

    // Source id -> what the shim said about a source of a debuggee (`newScript`), and debuggee
    // global -> the handles of its scripts and their source ids, while it is a debuggee: the shim
    // knows the source of a function while a handle of its script lives.
    const sourceInfos = new MapCtor();
    const debuggeeScripts = new MapCtor();
    // Script handle -> its breakpoint positions.
    const positionCache = new WeakMapCtor();

    function sourceInfo(id) {
        return sourceInfos.get(id);
    }

    function isDebuggee(global) {
        for (const st of active) {
            if (st.debuggees.has(global)) {
                return true;
            }
        }
        return false;
    }

    // Drops the scripts of a global that no longer is a debuggee.
    function releaseScripts(global) {
        const scripts = debuggeeScripts.get(global);
        if (scripts !== undefined && !isDebuggee(global)) {
            for (const id of scripts.keys()) {
                sourceInfos.delete(id);
            }
            debuggeeScripts.delete(global);
        }
    }

    // The lines of a source's text.
    function lines(info) {
        if (info.lines === undefined) {
            info.lines = info.text.split("\n");
        }
        return info.lines;
    }

    // The text of a line of a source (QuickJS's line numbers count from the document's start), or
    // undefined.
    function lineText(info, line) {
        return lines(info)[line - info.startLine];
    }

    // SpiderMonkey's column (in UTF-16 code units, 1-based) of a QuickJS position (in code points).
    // A line with surrogates has a table of the UTF-16 index of each code point.
    function utf16Column(info, line, column) {
        if (!info) {
            return column;
        }
        info.utf16 ??= new MapCtor();
        let table = info.utf16.get(line);
        if (table === undefined) {
            const text = lineText(info, line);
            table = null;
            if (text !== undefined && apply(RegExpTest, surrogateRegExp, [text])) {
                table = [0];
                for (let index = 0; index < text.length;) {
                    const c = text.charCodeAt(index);
                    index += c >= 0xd800 && c < 0xdc00 ? 2 : 1;
                    table.push(index);
                }
            }
            info.utf16.set(line, table);
        }
        if (table === null) {
            return column;
        }
        const n = column - 1, last = table.length - 1;
        return (n <= last ? table[n] : table[last] + n - last) + 1;
    }

    // The statement positions of a source, as `line:column` keys, or undefined.
    function statementSet(info) {
        if (!info?.statements) {
            return undefined;
        }
        if (info.statementSet === undefined) {
            info.statementSet = new SetCtor();
            const s = info.statements;
            for (let i = 0; i < s.length; i += 2) {
                info.statementSet.add(`${s[i]}:${utf16Column(info, s[i], s[i + 1])}`);
            }
        }
        return info.statementSet;
    }

    function isIdentifierChar(c) {
        return c !== undefined && (apply(RegExpTest, /[\p{ID_Continue}$\u200c\u200d]/u, [c]) || c >= "\ud800");
    }

    function isSpace(c) {
        return c === " " || c === "\t" || c === "\v" || c === "\f" || c === "\u00a0";
    }

    function skipSpacesBack(text, i) {
        while (i >= 0 && isSpace(text[i])) {
            i--;
        }
        return i;
    }

    // SpiderMonkey's column of a call whose QuickJS position (its `(`) is at `index` of `text`.
    function callColumn(text, index, kind, argc, line, statements) {
        if (text[index] !== "(") {
            return index + 1;
        }
        const end = skipSpacesBack(text, index - 1);
        let start = end + 1;
        while (start > 0 && isIdentifierChar(text[start - 1])) {
            start--;
        }
        const isName = start <= end;
        const before = skipSpacesBack(text, start - 1);
        if (kind === CALL_METHOD) {
            // The property name of `o.f()`, the `(` of `o[k]()`.
            return isName && text[before] === "." ? start + 1 : index + 1;
        }
        if (kind === CALL_CONSTRUCTOR) {
            // The `new` of `new F()` or `new a.b.F()`.
            let name = start;
            for (;;) {
                let j = skipSpacesBack(text, name - 1);
                if (text[j] !== ".") {
                    break;
                }
                j = skipSpacesBack(text, j - 1);
                let k = j + 1;
                while (k > 0 && isIdentifierChar(text[k - 1])) {
                    k--;
                }
                if (k > j) {
                    break;
                }
                name = k;
            }
            const j = skipSpacesBack(text, name - 1);
            const isNew = isName && j >= 2 && text.slice(j - 2, j + 1) === "new" && !isIdentifierChar(text[j - 3]);
            return isNew ? j - 1 : index + 1;
        }
        if (isName && text[before] !== ".") {
            // The callee's name, unless it starts the statement and the call has arguments.
            const column = start + 1;
            return argc === 0 || !statements.has(`${line}:${column}`) ? column : index + 1;
        }
        return index + 1;
    }

    // The breakpoint positions of a script: `{offset, lineNumber, columnNumber, isStepStart}`, in
    // offset order (that of QuickJS's positions).
    function breakpointPositions(handle, info) {
        let positions = positionCache.get(handle);
        if (positions !== undefined) {
            return positions;
        }
        const raw = native.scriptPositions(handle);
        positions = [];
        const seen = new SetCtor();
        const add = (offset, lineNumber, columnNumber, isStepStart) => {
            const key = `${lineNumber}:${columnNumber}`;
            if (!seen.has(key)) {
                seen.add(key);
                positions.push({ offset, lineNumber, columnNumber, isStepStart });
            }
        };
        const statements = statementSet(info);
        let lastLine = -1;
        for (let i = 0; i < raw.length; i += 3) {
            const pc = raw[i], line = raw[i + 1], column = utf16Column(info, raw[i + 1], raw[i + 2]);
            if (info && line < info.startLine) {
                // The shim's wrapper of an event handler.
                continue;
            }
            if (!statements) {
                // Compiled before a debugger was active: each position, the first of a line a step.
                add(pc, line, column, line !== lastLine);
                lastLine = line;
            } else if (statements.has(`${line}:${column}`)) {
                // Closing braces and the end of a script are breakpoints, not steps.
                const c = info && lineText(info, line)?.[column - 1];
                add(pc, line, column, c !== "}" && c !== undefined);
            } else {
                const [kind, argc] = native.scriptCall(handle, pc);
                const text = info && lineText(info, line);
                if (kind !== CALL_NONE && text !== undefined) {
                    add(pc, line, callColumn(text, column - 1, kind, argc, line, statements), false);
                }
            }
        }
        positionCache.set(handle, positions);
        return positions;
    }

    // The private state of a Debugger.
    class State {
        constructor(dbg) {
            this.dbg = dbg;
            // Debuggee global -> its Debugger.Object.
            this.debuggees = new MapCtor();
            // Referent -> Debugger.Object, script handle -> Debugger.Script, source id ->
            // Debugger.Source, frame handle -> Debugger.Frame, global -> its environments, held
            // weakly: each has one instance per Debugger while it lives, as in SpiderMonkey.
            this.objects = new WeakMapCtor();
            this.scripts = new WeakMapCtor();
            this.sources = new MapCtor();
            this.sourceSweep = 64;
            this.frames = new WeakMapCtor();
            this.globalEnvironments = new WeakMapCtor();
            // Script handle -> offset -> breakpoint handlers. Breakpoints keep their scripts.
            this.breakpoints = new MapCtor();
            // Frames with an onStep or onPop hook.
            this.hookedFrames = new SetCtor();
            this.onNewScript = undefined;
            this.onEnterFrame = undefined;
            this.onNativeCall = undefined;
            this.onDebuggerStatement = undefined;
            this.onExceptionUnwind = undefined;
            this.onNewGlobalObject = undefined;
            this.onNewPromise = undefined;
            this.onPromiseSettled = undefined;
            this.uncaughtExceptionHook = null;
            this.shouldAvoidSideEffects = false;
            this.exclusiveDebuggerOnEval = false;
        }

        wrap(v) {
            if (!isObject(v)) {
                return v;
            }
            let obj = deref(this.objects.get(v));
            if (obj === undefined) {
                obj = new DebuggerObject(TOKEN, this, v);
                this.objects.set(v, new WeakRefCtor(obj));
            }
            return obj;
        }

        // The debuggee value of a Debugger.Object of this debugger, or a primitive.
        unwrap(v) {
            if (DebuggerObject.is(v)) {
                if (DebuggerObject.owner(v) !== this) {
                    throw new TypeErrorCtor("Debugger.Object belongs to a different Debugger");
                }
                return DebuggerObject.referent(v);
            }
            if (isObject(v)) {
                throw new TypeErrorCtor("expected a Debugger.Object or a primitive value");
            }
            return v;
        }

        completion(result) {
            const [kind, value] = result;
            if (kind === RETURN) {
                return { return: this.wrap(value) };
            }
            if (kind === THROW) {
                return { throw: this.wrap(value) };
            }
            return null;
        }

        // Runs debuggee code for this debugger (`run` returns a native completion), and
        // returns the completion. With `exclusiveDebuggerOnEval`, other debuggers don't see it.
        evaluate(run) {
            const exclusive = this.exclusiveDebuggerOnEval;
            if (exclusive) {
                exclusiveStack.push(this);
            }
            try {
                return this.completion(run());
            } finally {
                if (exclusive) {
                    exclusiveStack.pop();
                }
            }
        }

        // Calls a hook. A hook that throws is reported to `uncaughtExceptionHook`, whose result
        // replaces the hook's.
        callHook(hook, thisArg, args) {
            try {
                return apply(hook, thisArg, args);
            } catch (e) {
                const report = this.uncaughtExceptionHook;
                if (typeof report === "function") {
                    try {
                        return apply(report, this.dbg, [e]);
                    } catch (_) { }
                }
                return undefined;
            }
        }

        source(id, info) {
            let source = deref(this.sources.get(id));
            if (source === undefined && info !== undefined) {
                if (this.sources.size >= this.sourceSweep) {
                    for (const [key, ref] of ArrayFrom(this.sources)) {
                        if (deref(ref) === undefined) {
                            this.sources.delete(key);
                        }
                    }
                    this.sourceSweep = 2 * this.sources.size + 64;
                }
                source = new DebuggerSource(TOKEN, info);
                this.sources.set(id, new WeakRefCtor(source));
            }
            return source ?? null;
        }

        // The Debugger.Script of a script handle; `info` describes its source, if new.
        script(handle, info) {
            if (handle === undefined) {
                return undefined;
            }
            let script = deref(this.scripts.get(handle));
            if (script === undefined) {
                const id = native.scriptSource(handle);
                info ??= sourceInfo(id);
                script = new DebuggerScript(TOKEN, this, handle, this.source(id, info), info);
                this.scripts.set(handle, new WeakRefCtor(script));
            }
            return script;
        }

        // The Debugger.Frame of a frame handle, made if `make`.
        frame(handle, make = true) {
            if (handle === null || handle === undefined) {
                return null;
            }
            let frame = deref(this.frames.get(handle));
            if (frame === undefined && make) {
                frame = new DebuggerFrame(TOKEN, this, handle);
                this.frames.set(handle, new WeakRefCtor(frame));
            }
            return frame ?? null;
        }

        // The global environments of a global: its lexical declarations, then the global object.
        globalEnvironment(global) {
            let env = deref(this.globalEnvironments.get(global));
            if (env === undefined) {
                const object = new DebuggerEnvironment(TOKEN, this, new ObjectScope(global), null);
                env = new DebuggerEnvironment(TOKEN, this, new GlobalLexicalScope(global), object);
                this.globalEnvironments.set(global, new WeakRefCtor(env));
            }
            return env;
        }

        // Removes the breakpoints of this debugger in the code of `global`, or all.
        clearBreakpoints(global) {
            for (const [handle, offsets] of ArrayFrom(this.breakpoints)) {
                if (global !== undefined && native.scriptGlobal(handle) !== global) {
                    continue;
                }
                for (const [offset, handlers] of offsets) {
                    for (let i = 0; i < handlers.length; i++) {
                        native.clearBreakpoint(handle, offset);
                    }
                }
                this.breakpoints.delete(handle);
            }
        }

        // Removes the breakpoints and frame hooks of this debugger in the code of `global`, or all.
        clearHooks(global) {
            this.clearBreakpoints(global);
            for (const frame of ArrayFrom(this.hookedFrames)) {
                if (global === undefined || DebuggerFrame.global(frame) === global) {
                    frame.onStep = undefined;
                    frame.onPop = undefined;
                }
            }
        }
    }

    // The states of the Debuggers with at least one debuggee, which the host calls about. The shim
    // only does so while there are any.
    const active = new SetCtor();
    // Debuggers running debuggee code with `exclusiveDebuggerOnEval`, innermost last.
    const exclusiveStack = [];
    // How many debuggers have an onEnterFrame or onNativeCall hook.
    let enterFrameHooks = 0;
    let nativeCallHooks = 0;

    function activate(st) {
        if (!active.has(st)) {
            active.add(st);
            if (active.size === 1) {
                native.setActive(true);
            }
            if (st.onEnterFrame !== undefined) {
                countHook("onEnterFrame", 1);
            }
            if (st.onNativeCall !== undefined) {
                countHook("onNativeCall", 1);
            }
        }
    }

    function deactivate(st) {
        if (active.delete(st)) {
            if (st.onEnterFrame !== undefined) {
                countHook("onEnterFrame", -1);
            }
            if (st.onNativeCall !== undefined) {
                countHook("onNativeCall", -1);
            }
            if (active.size === 0) {
                native.setActive(false);
            }
        }
    }

    // Counts the active debuggers with a frame entry or native call hook, so that the shim only
    // installs QuickJS's hooks while one does.
    function countHook(name, delta) {
        if (name === "onEnterFrame") {
            const before = enterFrameHooks;
            enterFrameHooks += delta;
            if ((before === 0) !== (enterFrameHooks === 0)) {
                native.wantFrameEntries(enterFrameHooks > 0);
            }
        } else if (name === "onNativeCall") {
            const before = nativeCallHooks;
            nativeCallHooks += delta;
            if ((before === 0) !== (nativeCallHooks === 0)) {
                native.wantNativeCalls(nativeCallHooks > 0);
            }
        }
    }

    // The State of a Debugger, in a private field.
    let stateOf;

    // The debuggers to tell about debuggee code of `global`.
    function interested(global) {
        if (exclusiveStack.length > 0) {
            const st = exclusiveStack[exclusiveStack.length - 1];
            return st.debuggees.has(global) ? [st] : [];
        }
        return ArrayFrom(active).filter(st => st.debuggees.has(global));
    }

    // The global of a frame handle.
    function frameGlobal(handle) {
        const script = native.frameScript(handle);
        return script === undefined ? undefined : native.scriptGlobal(script);
    }

    class Debugger {
        #st;

        static {
            stateOf = dbg => {
                if (!isObject(dbg) || !(#st in dbg)) {
                    throw new TypeErrorCtor("not a Debugger");
                }
                return dbg.#st;
            };
        }

        constructor(...debuggees) {
            if (new.target === undefined) {
                throw new TypeErrorCtor("Debugger must be called with new");
            }
            this.#st = new State(this);
            for (const g of debuggees) {
                this.addDebuggee(g);
            }
        }

        addDebuggee(global) {
            const st = stateOf(this);
            const g = debuggeeGlobal(global, "Debugger.addDebuggee");
            if (g === ownGlobal) {
                throw new TypeErrorCtor("Debugger.addDebuggee: a debugger can't debug its own global");
            }
            let obj = st.debuggees.get(g);
            if (obj === undefined) {
                obj = st.wrap(g);
                st.debuggees.set(g, obj);
                native.setDebuggee(g, true);
                activate(st);
            }
            return obj;
        }

        removeDebuggee(global) {
            const st = stateOf(this);
            const g = debuggeeGlobal(global, "Debugger.removeDebuggee");
            if (st.debuggees.delete(g)) {
                native.setDebuggee(g, false);
                st.clearHooks(g);
                if (st.debuggees.size === 0) {
                    deactivate(st);
                }
                releaseScripts(g);
            }
        }

        removeAllDebuggees() {
            const st = stateOf(this);
            const globals = ArrayFrom(st.debuggees.keys());
            for (const g of globals) {
                native.setDebuggee(g, false);
            }
            st.debuggees.clear();
            st.clearHooks();
            deactivate(st);
            globals.forEach(releaseScripts);
        }

        hasDebuggee(global) {
            const st = stateOf(this);
            const g = native.toGlobal(DebuggerObject.is(global) ? DebuggerObject.referent(global) : global);
            return g !== undefined && st.debuggees.has(g);
        }

        getDebuggees() {
            return ArrayFrom(stateOf(this).debuggees.values());
        }

        getNewestFrame() {
            const st = stateOf(this);
            for (const handle of native.frames()) {
                if (st.debuggees.has(frameGlobal(handle))) {
                    return st.frame(handle);
                }
            }
            return null;
        }

        adoptDebuggeeValue(value) {
            const st = stateOf(this);
            if (DebuggerObject.is(value)) {
                return st.wrap(DebuggerObject.referent(value));
            }
            if (isObject(value)) {
                throw new TypeErrorCtor("Debugger.adoptDebuggeeValue: expected a Debugger.Object");
            }
            return value;
        }

        adoptFrame(frame) {
            const st = stateOf(this);
            return st.frame(DebuggerFrame.handle(frame));
        }

        findScripts() {
            stateOf(this);
            return [];
        }

        findSources() {
            stateOf(this);
            return [];
        }

        clearAllBreakpoints() {
            stateOf(this).clearBreakpoints();
        }

        get enabled() {
            stateOf(this);
            return true;
        }

        get uncaughtExceptionHook() {
            return stateOf(this).uncaughtExceptionHook;
        }

        set uncaughtExceptionHook(hook) {
            if (hook !== null && typeof hook !== "function") {
                throw new TypeErrorCtor("Debugger.uncaughtExceptionHook must be a function or null");
            }
            stateOf(this).uncaughtExceptionHook = hook;
        }

        get shouldAvoidSideEffects() {
            return stateOf(this).shouldAvoidSideEffects;
        }

        set shouldAvoidSideEffects(value) {
            stateOf(this).shouldAvoidSideEffects = !!value;
        }

        get exclusiveDebuggerOnEval() {
            return stateOf(this).exclusiveDebuggerOnEval;
        }

        set exclusiveDebuggerOnEval(value) {
            stateOf(this).exclusiveDebuggerOnEval = !!value;
        }
    }

    for (const name of ["onNewScript", "onEnterFrame", "onNativeCall", "onDebuggerStatement",
        "onExceptionUnwind", "onNewGlobalObject", "onNewPromise", "onPromiseSettled"]) {
        defineProperty(Debugger.prototype, name, {
            get() {
                return stateOf(this)[name];
            },
            set(hook) {
                const st = stateOf(this);
                const before = st[name] !== undefined;
                st[name] = checkHook(name, hook);
                const after = st[name] !== undefined;
                if (active.has(st) && before !== after) {
                    countHook(name, after ? 1 : -1);
                }
            },
            configurable: true,
        });
    }

    class DebuggerObject {
        #st;
        #referent;

        constructor(token, st, referent) {
            if (token !== TOKEN) {
                throw new TypeErrorCtor("Debugger.Object is not a constructor");
            }
            this.#st = st;
            this.#referent = referent;
        }

        static is(v) {
            return isObject(v) && #referent in v;
        }

        static referent(obj) {
            return obj.#referent;
        }

        static owner(obj) {
            return obj.#st;
        }

        get class() {
            return native.className(this.#referent);
        }

        get callable() {
            return typeof this.#referent === "function";
        }

        get proto() {
            return this.#st.wrap(getPrototypeOf(this.#referent));
        }

        get global() {
            return this.#st.wrap(native.globalOf(this.#referent));
        }

        // The function's own name: that of its code for functions written in JavaScript, its own
        // `name` data property for other functions that are not JS Proxies. No debuggee code runs.
        get name() {
            const f = this.#referent;
            if (typeof f !== "function" || native.isProxy(f)) {
                return undefined;
            }
            const script = native.functionScript(f);
            if (script !== undefined) {
                const name = native.scriptInfo(script).name;
                if (typeof name === "string" && name !== "") {
                    return name;
                }
            }
            const desc = getOwnPropertyDescriptor(f, "name");
            const name = desc?.value;
            return typeof name === "string" && name !== "" ? name : undefined;
        }

        get displayName() {
            return this.name;
        }

        get parameterNames() {
            const f = this.#referent;
            if (native.functionKind(f) === NOT_A_FUNCTION) {
                return undefined;
            }
            const script = native.functionScript(f);
            if (script === undefined) {
                return [];
            }
            return native.scriptVariables(script).args.map(([name]) => isIdentifier(name) ? name : undefined);
        }

        get isAsyncFunction() {
            const kind = native.functionKind(this.#referent);
            return kind === NOT_A_FUNCTION ? undefined : kind === ASYNC || kind === ASYNC_GENERATOR;
        }

        get isGeneratorFunction() {
            const kind = native.functionKind(this.#referent);
            return kind === NOT_A_FUNCTION ? undefined : kind === GENERATOR || kind === ASYNC_GENERATOR;
        }

        get isBoundFunction() {
            const kind = native.functionKind(this.#referent);
            return kind === NOT_A_FUNCTION ? undefined : kind === BOUND;
        }

        // The Debugger.Script of a function written in JavaScript, and undefined for others.
        get script() {
            const kind = native.functionKind(this.#referent);
            if (kind === NOT_A_FUNCTION || kind === NATIVE || kind === BOUND) {
                return undefined;
            }
            return this.#st.script(native.functionScript(this.#referent));
        }

        get environment() {
            return undefined;
        }

        get isError() {
            return native.className(this.#referent) === "Error";
        }

        get isProxy() {
            return native.isProxy(this.#referent);
        }

        isExtensible() {
            return isExtensible(this.#referent);
        }

        isSealed() {
            return isSealed(this.#referent);
        }

        isFrozen() {
            return isFrozen(this.#referent);
        }

        getOwnPropertyNames() {
            return getOwnPropertyNames(this.#referent);
        }

        getOwnPropertyNamesLength() {
            return getOwnPropertyNames(this.#referent).length;
        }

        getOwnPropertySymbols() {
            return getOwnPropertySymbols(this.#referent);
        }

        getOwnPropertyDescriptor(key) {
            const desc = getOwnPropertyDescriptor(this.#referent, key);
            if (desc === undefined) {
                return undefined;
            }
            const st = this.#st;
            const result = { configurable: desc.configurable, enumerable: desc.enumerable };
            if ("value" in desc) {
                result.value = st.wrap(desc.value);
                result.writable = desc.writable;
            } else {
                result.get = st.wrap(desc.get);
                result.set = st.wrap(desc.set);
            }
            return result;
        }

        getProperty(key, receiver) {
            const st = this.#st;
            const r = receiver === undefined ? this.#referent : st.unwrap(receiver);
            return st.evaluate(() => native.call(ReflectGet, undefined, [this.#referent, key, r]));
        }

        call(thisArg, ...args) {
            return this.apply(thisArg, args);
        }

        apply(thisArg, args) {
            const st = this.#st;
            if (typeof this.#referent !== "function") {
                throw new TypeErrorCtor("Debugger.Object.prototype.apply: the referent is not callable");
            }
            const t = st.unwrap(thisArg);
            const a = args === undefined || args === null ? [] : ArrayFrom(args, v => st.unwrap(v));
            return st.evaluate(() => native.call(this.#referent, t, a));
        }

        // Code that must have no side effects runs with the debugger's hooks, which abort it at
        // effects; code that declares global bindings, which happens before its first instruction,
        // doesn't run.
        executeInGlobal(code, options) {
            const st = this.#st;
            const global = native.toGlobal(this.#referent);
            if (global === undefined) {
                throw new TypeErrorCtor("Debugger.Object.prototype.executeInGlobal: the referent is not a global");
            }
            code = StringCtor(code);
            const url = options?.url !== undefined ? StringCtor(options.url) : "debugger eval code";
            const pure = st.shouldAvoidSideEffects;
            return st.evaluate(() => native.evalInGlobal(global, code, url, pure));
        }

        asEnvironment() {
            const global = native.toGlobal(this.#referent);
            if (global === undefined) {
                return undefined;
            }
            return this.#st.globalEnvironment(global);
        }

        unsafeDereference() {
            return this.#referent;
        }

        makeDebuggeeValue(value) {
            if (DebuggerObject.is(value)) {
                return this.#st.wrap(DebuggerObject.referent(value));
            }
            return this.#st.wrap(value);
        }
    }

    class DebuggerScript {
        #st;
        #handle;
        #source;
        #info;

        constructor(token, st, handle, source, info) {
            if (token !== TOKEN) {
                throw new TypeErrorCtor("Debugger.Script is not a constructor");
            }
            this.#st = st;
            this.#handle = handle;
            this.#source = source;
            this.#info = info;
        }

        static handle(script) {
            return script.#handle;
        }

        #scriptInfo() {
            return native.scriptInfo(this.#handle);
        }

        #positions() {
            return breakpointPositions(this.#handle, this.#info);
        }

        get source() {
            return this.#source;
        }

        get url() {
            return native.scriptUrl(this.#handle) ?? this.#source?.url;
        }

        get global() {
            return this.#st.wrap(native.scriptGlobal(this.#handle));
        }

        get displayName() {
            const info = this.#scriptInfo();
            return info.isToplevel || info.name === null || info.name === "" ? undefined : info.name;
        }

        get format() {
            return "js";
        }

        get isFunction() {
            return !this.#scriptInfo().isToplevel;
        }

        get isModule() {
            return this.#scriptInfo().isModule;
        }

        get parameterNames() {
            if (this.#scriptInfo().isToplevel) {
                return undefined;
            }
            return native.scriptVariables(this.#handle).args.map(([name]) => isIdentifier(name) ? name : undefined);
        }

        get startLine() {
            const info = this.#scriptInfo();
            return info.isToplevel ? (this.#info?.startLine ?? info.line) : info.line;
        }

        get startColumn() {
            const info = this.#scriptInfo();
            if (info.isToplevel) {
                return 1;
            }
            return utf16Column(this.#info, info.line, info.column);
        }

        // The offset of the function in the source text (0 for the script's code), which identifies
        // the script in its source.
        get sourceStart() {
            const info = this.#scriptInfo();
            const source = this.#info;
            if (info.isToplevel || !source) {
                return 0;
            }
            const all = lines(source);
            let offset = 0;
            for (let i = 0; i < info.line - source.startLine && i < all.length; i++) {
                offset += all[i].length + 1;
            }
            return offset + utf16Column(source, info.line, info.column) - 1;
        }

        get sourceLength() {
            return this.#scriptInfo().isToplevel ? (this.#info?.text.length ?? 0) : 0;
        }

        getChildScripts() {
            return native.scriptChildren(this.#handle).map(h => this.#st.script(h, this.#info));
        }

        getPossibleBreakpoints(query) {
            let positions = this.#positions();
            if (query?.line !== undefined) {
                positions = positions.filter(p => p.lineNumber === query.line);
            }
            return positions.map(p => ({ ...p }));
        }

        getPossibleBreakpointOffsets(query) {
            return this.getPossibleBreakpoints(query).map(p => p.offset);
        }

        getEffectfulOffsets() {
            return native.scriptEffectfulOffsets(this.#handle);
        }

        // The location of an offset: that of the breakpoint position at it or before it.
        getOffsetMetadata(offset) {
            const positions = this.#positions();
            let lo = 0, hi = positions.length;
            while (lo < hi) {
                const mid = (lo + hi) >> 1;
                if (positions[mid].offset <= offset) {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            let best = positions[lo - 1];
            if (best?.offset === offset) {
                return {
                    lineNumber: best.lineNumber,
                    columnNumber: best.columnNumber,
                    isBreakpoint: true,
                    isStepStart: best.isStepStart,
                };
            }
            if (best === undefined) {
                const [line, column] = native.scriptLocation(this.#handle, offset);
                best = { lineNumber: line, columnNumber: utf16Column(this.#info, line, column) };
            }
            return {
                lineNumber: best.lineNumber,
                columnNumber: best.columnNumber,
                isBreakpoint: false,
                isStepStart: false,
            };
        }

        getOffsetLocation(offset) {
            const { lineNumber, columnNumber, isBreakpoint } = this.getOffsetMetadata(offset);
            return { lineNumber, columnNumber, isEntryPoint: isBreakpoint };
        }

        // The offsets where stepping stops: the breakpoint positions, and the backward jumps, so
        // that a loop without any still steps.
        stepOffsets() {
            const offsets = new SetCtor(this.#positions().map(p => p.offset));
            for (const offset of native.scriptLoopOffsets(this.#handle)) {
                offsets.add(offset);
            }
            return ArrayFrom(offsets);
        }

        setBreakpoint(offset, handler) {
            if (!isObject(handler)) {
                throw new TypeErrorCtor("Debugger.Script.prototype.setBreakpoint: the handler is not an object");
            }
            if (typeof offset !== "number" || !native.setBreakpoint(this.#handle, offset)) {
                throw new Error("Debugger.Script.prototype.setBreakpoint: invalid offset");
            }
            const st = this.#st;
            let offsets = st.breakpoints.get(this.#handle);
            if (offsets === undefined) {
                offsets = new MapCtor();
                st.breakpoints.set(this.#handle, offsets);
            }
            let handlers = offsets.get(offset);
            if (handlers === undefined) {
                handlers = [];
                offsets.set(offset, handlers);
            }
            handlers.push(handler);
        }

        getBreakpoints(offset) {
            const offsets = this.#st.breakpoints.get(this.#handle);
            if (offsets === undefined) {
                return [];
            }
            if (offset !== undefined) {
                return [...(offsets.get(offset) ?? [])];
            }
            return ArrayFrom(offsets.values()).flat();
        }

        clearBreakpoint(handler, offset) {
            const offsets = this.#st.breakpoints.get(this.#handle);
            if (offsets === undefined) {
                return;
            }
            for (const [at, handlers] of ArrayFrom(offsets)) {
                if (offset !== undefined && at !== offset) {
                    continue;
                }
                const kept = handlers.filter(h => h !== handler);
                for (let i = kept.length; i < handlers.length; i++) {
                    native.clearBreakpoint(this.#handle, at);
                }
                if (kept.length === 0) {
                    offsets.delete(at);
                } else {
                    offsets.set(at, kept);
                }
            }
        }

        clearAllBreakpoints(offset) {
            const offsets = this.#st.breakpoints.get(this.#handle);
            if (offsets === undefined) {
                return;
            }
            for (const [at, handlers] of ArrayFrom(offsets)) {
                if (offset !== undefined && at !== offset) {
                    continue;
                }
                for (let i = 0; i < handlers.length; i++) {
                    native.clearBreakpoint(this.#handle, at);
                }
                offsets.delete(at);
            }
            if (offsets.size === 0) {
                this.#st.breakpoints.delete(this.#handle);
            }
        }

        isInCatchScope(offset) {
            return false;
        }
    }

    class DebuggerSource {
        #info;

        constructor(token, info) {
            if (token !== TOKEN) {
                throw new TypeErrorCtor("Debugger.Source is not a constructor");
            }
            this.#info = info;
        }

        get id() {
            return this.#info.id;
        }

        get url() {
            return this.#info.url;
        }

        get text() {
            return this.#info.text;
        }

        get introductionType() {
            return this.#info.introductionType;
        }

        get displayURL() {
            return this.#info.displayURL ?? null;
        }

        get introductionScript() {
            return undefined;
        }

        get introductionOffset() {
            return undefined;
        }

        get elementAttributeName() {
            return undefined;
        }

        get sourceMapURL() {
            return null;
        }

        get startLine() {
            return this.#info.startLine;
        }

        get startColumn() {
            return 1;
        }
    }

    // Frames.

    // Frame handle -> the frames (of all debuggers) with an onStep or an onPop hook. The shim calls
    // `popFrame` when the frame of such a handle stops, and has breakpoints at the step positions
    // of its script while one of them has an onStep hook.
    const hooked = new WeakMapCtor();
    // The frame handles with breakpoints at their step positions, and those whose frame's stop is
    // reported, for this emulation (the shim counts emulations).
    const stepping = new WeakSetCtor();
    const popping = new WeakSetCtor();

    function updateFrameHooks(handle, frame) {
        let frames = hooked.get(handle);
        const [onStep, onPop] = DebuggerFrame.hooks(frame);
        if (onStep !== undefined || onPop !== undefined) {
            if (frames === undefined) {
                frames = new SetCtor();
                hooked.set(handle, frames);
            }
            frames.add(frame);
        } else if (frames !== undefined) {
            frames.delete(frame);
            if (frames.size === 0) {
                hooked.delete(handle);
                frames = undefined;
            }
        }
        const step = frames !== undefined && ArrayFrom(frames).some(f => DebuggerFrame.hooks(f)[0] !== undefined);
        if (step !== stepping.has(handle)) {
            if (step) {
                stepping.add(handle);
            } else {
                stepping.delete(handle);
            }
            native.setStepping(handle, step ? DebuggerFrame.script(frame).stepOffsets() : null);
        }
        const pop = frames !== undefined;
        if (pop !== popping.has(handle)) {
            if (pop) {
                popping.add(handle);
            } else {
                popping.delete(handle);
            }
            native.setOnPop(handle, pop);
        }
    }

    class DebuggerFrame {
        #st;
        #handle;
        #onStep = undefined;
        #onPop = undefined;
        #environment = undefined;

        constructor(token, st, handle) {
            if (token !== TOKEN) {
                throw new TypeErrorCtor("Debugger.Frame is not a constructor");
            }
            this.#st = st;
            this.#handle = handle;
        }

        static handle(frame) {
            if (!isObject(frame) || !(#handle in frame)) {
                throw new TypeErrorCtor("not a Debugger.Frame");
            }
            return frame.#handle;
        }

        static hooks(frame) {
            return [frame.#onStep, frame.#onPop];
        }

        static owner(frame) {
            return frame.#st;
        }

        static script(frame) {
            return frame.#st.script(native.frameScript(frame.#handle));
        }

        static global(frame) {
            return frameGlobal(frame.#handle);
        }

        #live() {
            if (!native.frameAlive(this.#handle)) {
                throw new Error("Debugger.Frame is not live");
            }
        }

        #info() {
            return native.scriptInfo(native.frameScript(this.#handle));
        }

        get onStack() {
            return native.frameAlive(this.#handle);
        }

        get live() {
            return this.onStack;
        }

        get terminated() {
            return !this.onStack;
        }

        get script() {
            return this.#st.script(native.frameScript(this.#handle));
        }

        get offset() {
            this.#live();
            return native.frameOffset(this.#handle);
        }

        get older() {
            this.#live();
            const st = this.#st;
            let handle = native.frameOlder(this.#handle);
            while (handle !== null && !st.debuggees.has(frameGlobal(handle))) {
                handle = native.frameOlder(handle);
            }
            return st.frame(handle);
        }

        get type() {
            const info = this.#info();
            if (info.isEval) {
                return "eval";
            }
            if (info.isToplevel) {
                return info.isModule ? "module" : "global";
            }
            return "call";
        }

        get implementation() {
            return "interpreter";
        }

        get this() {
            this.#live();
            return this.#st.wrap(frameThis(this.#handle));
        }

        get callee() {
            if (this.type !== "call") {
                return null;
            }
            return this.#st.wrap(native.frameCallee(this.#handle));
        }

        get arguments() {
            return null;
        }

        get environment() {
            this.#live();
            if (this.#environment === undefined) {
                const st = this.#st;
                const global = frameGlobal(this.#handle);
                const scope = new FrameScope(this.#handle, false);
                let parent = st.globalEnvironment(global);
                if (scope.closures.length > 0) {
                    parent = new DebuggerEnvironment(TOKEN, st, new ClosureScope(this.#handle, scope.closures), parent);
                }
                // A script's own frame has no variables of its own to show.
                this.#environment = scope.names().length === 0 && this.type !== "call"
                    ? parent
                    : new DebuggerEnvironment(TOKEN, st, scope, parent, this.type === "call" ? this.script : null);
            }
            return this.#environment;
        }

        get onStep() {
            return this.#onStep;
        }

        set onStep(hook) {
            checkHook("Frame.onStep", hook);
            if (hook !== undefined) {
                this.#live();
            }
            if (hook !== this.#onStep) {
                this.#onStep = hook;
                this.#hooksChanged();
            }
        }

        get onPop() {
            return this.#onPop;
        }

        set onPop(hook) {
            checkHook("Frame.onPop", hook);
            if (hook !== undefined) {
                this.#live();
            }
            if (hook !== this.#onPop) {
                this.#onPop = hook;
                this.#hooksChanged();
            }
        }

        #hooksChanged() {
            if (this.#onStep !== undefined || this.#onPop !== undefined) {
                this.#st.hookedFrames.add(this);
            } else {
                this.#st.hookedFrames.delete(this);
            }
            updateFrameHooks(this.#handle, this);
        }

        // Evaluates code in the frame's scope: direct eval inside a `with` statement over a proxy
        // of the frame's variables, so that assignments reach the frame.
        eval(code, options) {
            this.#live();
            const st = this.#st;
            const handle = this.#handle;
            const global = frameGlobal(handle);
            const scope = new FrameScope(handle, true);
            const evaluator = native.frameEvaluator(global);
            const handler = scopeHandler(scope, StringCtor(code), native.intrinsicEval(global), global);
            const proxy = new ProxyCtor(create(null), handler);
            const thisValue = frameThis(handle);
            return st.evaluate(() => native.evalInFrame(evaluator, thisValue, proxy));
        }
    }

    // The `this` of a frame: the lexical one of an arrow function, the one of a function that uses
    // it (coerced if sloppy, initialized in derived constructors), else its this argument, coerced
    // for sloppy functions.
    function frameThis(handle) {
        const script = native.frameScript(handle);
        const vars = native.scriptVariables(script);
        for (const [kind, list] of [[VARIABLE_LOCAL, vars.locals], [VARIABLE_CLOSURE, vars.closures]]) {
            const i = list.findIndex(([name]) => name === "this");
            if (i >= 0) {
                const [status, value] = native.frameVariable(handle, kind, i);
                if (status === VALUE) {
                    return value;
                }
            }
        }
        const info = native.scriptInfo(script);
        const value = native.frameThis(handle);
        if (!info.isStrict && !info.isToplevel && (value === undefined || value === null)) {
            return native.globalThisOf(frameGlobal(handle));
        }
        return value;
    }

    // Scopes: the variables of environments.

    // A frame's arguments and locals (and, for `frame.eval`, the variables it closes over): name ->
    // [kind, index, const], the innermost declaration of a name last.
    class FrameScope {
        constructor(handle, withClosures) {
            this.handle = handle;
            const script = native.frameScript(handle);
            const vars = script === undefined ? { args: [], locals: [], closures: [] } : native.scriptVariables(script);
            this.slots = new MapCtor();
            this.closures = [];
            const add = (slots, name, kind, index, isConst) => {
                if (!isIdentifier(name)) {
                    return;
                }
                let list = slots.get(name);
                if (list === undefined) {
                    list = [];
                    slots.set(name, list);
                }
                list.push([kind, index, isConst]);
            };
            vars.args.forEach(([name], i) => add(this.slots, name, VARIABLE_ARG, i, false));
            vars.locals.forEach(([name, , isConst], i) => add(this.slots, name, VARIABLE_LOCAL, i, isConst));
            vars.closures.forEach(([name, , isConst, binding], i) => {
                // The variables of a module are those of its code's frame.
                if (binding === BINDING_MODULE) {
                    add(this.slots, name, VARIABLE_CLOSURE, i, isConst);
                } else if (binding === BINDING_FUNCTION && isIdentifier(name)) {
                    this.closures.push([name, i, isConst]);
                    if (withClosures && !this.slots.has(name)) {
                        add(this.slots, name, VARIABLE_CLOSURE, i, isConst);
                    }
                }
            });
        }

        names() {
            return native.frameAlive(this.handle) ? ArrayFrom(this.slots.keys()) : [];
        }

        // The slot of a name that holds a value, else the last one (uninitialized).
        slot(name) {
            const list = this.slots.get(name);
            if (list === undefined) {
                return undefined;
            }
            for (let i = list.length - 1; i >= 0; i--) {
                const [status] = native.frameVariable(this.handle, list[i][0], list[i][1]);
                if (status === VALUE) {
                    return list[i];
                }
            }
            return list[list.length - 1];
        }

        get(name) {
            const slot = this.slot(name);
            if (slot === undefined || !native.frameAlive(this.handle)) {
                return [MISSING, undefined];
            }
            return native.frameVariable(this.handle, slot[0], slot[1]);
        }

        set(name, value) {
            const slot = this.slot(name);
            if (slot === undefined || slot[2]) {
                return false;
            }
            return native.setFrameVariable(this.handle, slot[0], slot[1], value);
        }

        type() {
            return "declarative";
        }
    }

    // The variables a frame's function closes over.
    class ClosureScope {
        constructor(handle, closures) {
            this.handle = handle;
            this.slots = new MapCtor();
            for (const [name, index, isConst] of closures) {
                this.slots.set(name, [VARIABLE_CLOSURE, index, isConst]);
            }
        }

        names() {
            return native.frameAlive(this.handle) ? ArrayFrom(this.slots.keys()) : [];
        }

        get(name) {
            const slot = this.slots.get(name);
            if (slot === undefined) {
                return [MISSING, undefined];
            }
            return native.frameVariable(this.handle, slot[0], slot[1]);
        }

        set(name, value) {
            const slot = this.slots.get(name);
            return slot !== undefined && !slot[2] && native.setFrameVariable(this.handle, slot[0], slot[1], value);
        }

        type() {
            return "declarative";
        }
    }

    // The global lexical declarations of a global.
    class GlobalLexicalScope {
        constructor(global) {
            this.global = global;
        }

        names() {
            return native.globalLexicalNames(this.global);
        }

        get(name) {
            return native.globalLexicalVariable(this.global, name);
        }

        set(name, value) {
            return false;
        }

        type() {
            return "declarative";
        }

        scopeKind() {
            return "global";
        }
    }

    // The global object.
    class ObjectScope {
        constructor(global) {
            this.global = global;
        }

        names() {
            return getOwnPropertyNames(this.global);
        }

        get(name) {
            const desc = getOwnPropertyDescriptor(this.global, name);
            return desc !== undefined && "value" in desc ? [VALUE, desc.value] : [MISSING, undefined];
        }

        set(name, value) {
            return false;
        }

        type() {
            return "object";
        }
    }

    // The proxy handler of `frame.eval`'s scope (see FRAME_EVALUATOR in debugger.rs): it serves the
    // evaluator's first lookups of `eval` and `debuggerCode` with the realm's eval and the code, then
    // the frame's variables and `arguments`; other names are looked up outside, in the global
    // environment. Its errors are the debuggee's.
    function scopeHandler(scope, code, realmEval, global) {
        let evalTaken = false;
        let codeTaken = false;
        const fail = (kind, message) => {
            throw native.newError(global, kind, message);
        };
        return {
            has(target, name) {
                if (typeof name !== "string") {
                    return false;
                }
                return (name === "eval" && !evalTaken) || (name === "debuggerCode" && !codeTaken) ||
                    name === "arguments" || scope.slots.has(name);
            },
            get(target, name) {
                if (typeof name !== "string") {
                    return undefined;
                }
                if (name === "eval" && !evalTaken) {
                    evalTaken = true;
                    return realmEval;
                }
                if (name === "debuggerCode" && !codeTaken) {
                    codeTaken = true;
                    return code;
                }
                const [status, value] = scope.get(name);
                if (status === UNINITIALIZED) {
                    fail(1, `can't access lexical declaration '${name}' before initialization`);
                }
                return value;
            },
            set(target, name, value) {
                if (!scope.set(name, value)) {
                    fail(0, `invalid assignment to '${name}'`);
                }
                return true;
            },
        };
    }

    class DebuggerEnvironment {
        #st;
        #scope;
        #parent;
        #calleeScript;

        constructor(token, st, scope, parent, calleeScript = null) {
            if (token !== TOKEN) {
                throw new TypeErrorCtor("Debugger.Environment is not a constructor");
            }
            this.#st = st;
            this.#scope = scope;
            this.#parent = parent;
            this.#calleeScript = calleeScript;
        }

        get type() {
            return this.#scope.type();
        }

        get parent() {
            return this.#parent;
        }

        get object() {
            return this.#scope instanceof ObjectScope ? this.#st.wrap(this.#scope.global) : undefined;
        }

        get calleeScript() {
            return this.#calleeScript;
        }

        get scopeKind() {
            if (this.#calleeScript !== null) {
                return "function";
            }
            return this.#scope.scopeKind?.() ?? null;
        }

        get optimizedOut() {
            return false;
        }

        get inspectable() {
            return true;
        }

        names() {
            return this.#scope.names();
        }

        getVariable(name) {
            const [status, value] = this.#scope.get(StringCtor(name));
            if (status === VALUE) {
                return this.#st.wrap(value);
            }
            return status === UNINITIALIZED ? uninitialized : optimizedOut;
        }

        setVariable(name, value) {
            if (!this.#scope.set(StringCtor(name), this.#st.unwrap(value))) {
                throw new TypeErrorCtor(`Debugger.Environment.prototype.setVariable: can't set ${name}`);
            }
        }

        find(name) {
            for (let env = this; env !== null; env = env.parent) {
                if (env.names().includes(name)) {
                    return env;
                }
            }
            return null;
        }
    }

    Debugger.Object = DebuggerObject;
    Debugger.Script = DebuggerScript;
    Debugger.Source = DebuggerSource;
    Debugger.Frame = DebuggerFrame;
    Debugger.Environment = DebuggerEnvironment;

    // A hook's resumption value: `null` terminates; anything else goes on (QuickJS can't force a
    // return or a throw).
    function resumption(value) {
        return value === null ? TERMINATE : CONTINUE;
    }

    const host = {
        // The shim compiled the code of a script, module, event handler or eval code of a global,
        // which hasn't run yet.
        newScript(handle, info) {
            const global = native.scriptGlobal(handle);
            if (!isDebuggee(global)) {
                return;
            }
            // Not those of evaluations that other debuggers don't see (eager evaluations).
            if (exclusiveStack.length === 0) {
                let scripts = debuggeeScripts.get(global);
                if (scripts === undefined) {
                    scripts = new MapCtor();
                    debuggeeScripts.set(global, scripts);
                }
                scripts.set(info.id, handle);
                sourceInfos.set(info.id, info);
            }
            for (const st of interested(global)) {
                const hook = st.onNewScript;
                if (hook !== undefined) {
                    st.callHook(hook, st.dbg, [st.script(handle, info)]);
                }
            }
        },

        // A breakpoint or step position: the debuggers' handlers at it, and the onStep hooks of the
        // frame.
        breakpoint(frameHandle, scriptHandle, offset, global) {
            for (const st of interested(global)) {
                const frame = st.frame(frameHandle, false);
                const onStep = frame === null ? undefined : DebuggerFrame.hooks(frame)[0];
                if (onStep !== undefined && resumption(st.callHook(onStep, frame, [])) === TERMINATE) {
                    return TERMINATE;
                }
                const handlers = st.breakpoints.get(scriptHandle)?.get(offset);
                if (handlers === undefined) {
                    continue;
                }
                for (const handler of [...handlers]) {
                    const hit = handler.hit;
                    if (typeof hit === "function" &&
                        resumption(st.callHook(hit, handler, [st.frame(frameHandle)])) === TERMINATE) {
                        return TERMINATE;
                    }
                }
            }
            return CONTINUE;
        },

        enterFrame(frameHandle, global) {
            for (const st of interested(global)) {
                const hook = st.onEnterFrame;
                if (hook !== undefined &&
                    resumption(st.callHook(hook, st.dbg, [st.frame(frameHandle)])) === TERMINATE) {
                    return TERMINATE;
                }
            }
            return CONTINUE;
        },

        // A frame with onStep or onPop hooks stops: its onPop hooks are called, and its hooks
        // removed.
        popFrame(frameHandle, kind, value, global) {
            const frames = ArrayFrom(hooked.get(frameHandle) ?? []);
            let result = CONTINUE;
            for (const frame of frames) {
                const onPop = DebuggerFrame.hooks(frame)[1];
                if (onPop !== undefined) {
                    const st = DebuggerFrame.owner(frame);
                    const completion = frameCompletion(st, kind, value);
                    if (resumption(st.callHook(onPop, frame, [completion])) === TERMINATE) {
                        result = TERMINATE;
                    }
                }
            }
            for (const frame of frames) {
                frame.onPop = undefined;
                frame.onStep = undefined;
            }
            return result;
        },

        nativeCall(func, global) {
            for (const st of interested(global)) {
                const hook = st.onNativeCall;
                if (hook !== undefined &&
                    resumption(st.callHook(hook, st.dbg, [st.wrap(func), "call"])) === TERMINATE) {
                    return TERMINATE;
                }
            }
            return CONTINUE;
        },
    };

    function frameCompletion(st, kind, value) {
        switch (kind) {
            case EXIT_RETURN:
                return { return: st.wrap(value) };
            case EXIT_THROW:
                return { throw: st.wrap(value) };
            case EXIT_SUSPEND:
                return { return: st.wrap(value), yield: true };
            default:
                return null;
        }
    }

    return { Debugger, host };
})
