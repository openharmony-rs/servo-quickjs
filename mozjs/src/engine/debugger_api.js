/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

// SpiderMonkey's Debugger API (https://firefox-source-docs.mozilla.org/js/Debugger/), as far as
// Servo's devtools use it, on QuickJS. `debugger.rs` evaluates this file in a debugger global and
// calls the function with its natives; it defines the `Debugger` constructor this returns on the
// global, and calls `host` when a script is compiled.
//
// Debuggee values are QuickJS values of other realms of the same runtime: there are no
// compartments, so debugger code reaches them directly. This file only uses intrinsics captured
// when it runs. It calls debuggee functions and evaluates debuggee code only through `native.call`
// and `native.evalInGlobal`, which also report terminations; as in SpiderMonkey, the traps of JS
// Proxies still run when it inspects them (their properties, prototype or extensibility).
//
// QuickJS's WeakMaps are not ephemerons: an entry whose value references its key keeps the key
// alive. So the maps from debuggee values to the Debugger.* objects that reference them hold
// those objects weakly.
(function (native) {
    "use strict";

    const { defineProperty, getOwnPropertyDescriptor, getOwnPropertyNames, getOwnPropertySymbols,
        getPrototypeOf, isExtensible, isFrozen, isSealed } = Object;
    const { apply } = Reflect;
    const ReflectGet = Reflect.get;
    const WeakRefCtor = WeakRef;
    const WeakRefDeref = WeakRef.prototype.deref;
    // The debugger's own global, which can't be a debuggee.
    const ownGlobal = globalThis;
    const ArrayFrom = Array.from;
    const StringCtor = String;

    // The kinds of `[kind, value]` completions of the natives.
    const RETURN = 0;
    const THROW = 1;

    // `native.functionKind` results.
    const NOT_A_FUNCTION = -1;
    const NATIVE = 0;
    const GENERATOR = 2;
    const ASYNC = 3;
    const ASYNC_GENERATOR = 4;
    const BOUND = 5;

    // Only this file creates instances of the Debugger.* classes.
    const TOKEN = {};

    function isObject(v) {
        return (typeof v === "object" && v !== null) || typeof v === "function";
    }

    // The value of a WeakRef, or undefined.
    function deref(ref) {
        return ref === undefined ? undefined : apply(WeakRefDeref, ref, []);
    }

    // The global a debuggee argument designates: a global, a WindowProxy, or a Debugger.Object
    // of one.
    function debuggeeGlobal(v, what) {
        const g = native.toGlobal(DebuggerObject.is(v) ? DebuggerObject.referent(v) : v);
        if (g === undefined) {
            throw new TypeError(`${what}: argument is not a global object`);
        }
        return g;
    }

    function checkHook(name, hook) {
        if (hook !== undefined && typeof hook !== "function") {
            throw new TypeError(`Debugger.${name} must be a function or undefined`);
        }
        return hook;
    }

    // The private state of a Debugger.
    class State {
        constructor(dbg) {
            this.dbg = dbg;
            // Debuggee global -> its Debugger.Object.
            this.debuggees = new Map();
            // Referent -> Debugger.Object, script handle -> Debugger.Script, source id ->
            // Debugger.Source, held weakly: each has one instance per Debugger while it lives, as in
            // SpiderMonkey.
            this.objects = new WeakMap();
            this.scripts = new WeakMap();
            this.sources = new Map();
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
                    throw new TypeError("Debugger.Object belongs to a different Debugger");
                }
                return DebuggerObject.referent(v);
            }
            if (isObject(v)) {
                throw new TypeError("expected a Debugger.Object or a primitive value");
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
            if (source === undefined) {
                source = new DebuggerSource(TOKEN, info);
                this.sources.set(id, new WeakRefCtor(source));
            }
            return source;
        }

        // The Debugger.Script of a script handle; `info` describes its source, if new.
        script(handle, info) {
            let script = deref(this.scripts.get(handle));
            if (script === undefined) {
                const id = native.scriptSource(handle);
                script = new DebuggerScript(TOKEN, this, handle, this.source(id, info));
                this.scripts.set(handle, new WeakRefCtor(script));
            }
            return script;
        }
    }

    // The states of the Debuggers with at least one debuggee, which the host calls about. The shim
    // only does so while there are any.
    const active = new Set();
    // Debuggers running debuggee code with `exclusiveDebuggerOnEval`, innermost last.
    const exclusiveStack = [];

    function activate(st) {
        if (!active.has(st)) {
            active.add(st);
            if (active.size === 1) {
                native.setActive(true);
            }
        }
    }

    function deactivate(st) {
        if (active.delete(st) && active.size === 0) {
            native.setActive(false);
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

    class Debugger {
        #st;

        static {
            stateOf = dbg => {
                if (!isObject(dbg) || !(#st in dbg)) {
                    throw new TypeError("not a Debugger");
                }
                return dbg.#st;
            };
        }

        constructor(...debuggees) {
            if (new.target === undefined) {
                throw new TypeError("Debugger must be called with new");
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
                throw new TypeError("Debugger.addDebuggee: a debugger can't debug its own global");
            }
            let obj = st.debuggees.get(g);
            if (obj === undefined) {
                obj = st.wrap(g);
                st.debuggees.set(g, obj);
                activate(st);
            }
            return obj;
        }

        removeDebuggee(global) {
            const st = stateOf(this);
            st.debuggees.delete(debuggeeGlobal(global, "Debugger.removeDebuggee"));
            if (st.debuggees.size === 0) {
                deactivate(st);
            }
        }

        removeAllDebuggees() {
            const st = stateOf(this);
            st.debuggees.clear();
            deactivate(st);
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
            stateOf(this);
            return null;
        }

        adoptDebuggeeValue(value) {
            const st = stateOf(this);
            if (DebuggerObject.is(value)) {
                return st.wrap(DebuggerObject.referent(value));
            }
            if (isObject(value)) {
                throw new TypeError("Debugger.adoptDebuggeeValue: expected a Debugger.Object");
            }
            return value;
        }

        adoptFrame(frame) {
            stateOf(this);
            throw new TypeError("Debugger.adoptFrame: not a Debugger.Frame");
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
            stateOf(this);
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
                throw new TypeError("Debugger.uncaughtExceptionHook must be a function or null");
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
                stateOf(this)[name] = checkHook(name, hook);
            },
            configurable: true,
        });
    }

    class DebuggerObject {
        #st;
        #referent;

        constructor(token, st, referent) {
            if (token !== TOKEN) {
                throw new TypeError("Debugger.Object is not a constructor");
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

        // The own `name` data property of a function that is not a JS Proxy, so no debuggee code
        // runs.
        get name() {
            if (typeof this.#referent !== "function" || native.isProxy(this.#referent)) {
                return undefined;
            }
            const desc = getOwnPropertyDescriptor(this.#referent, "name");
            const name = desc?.value;
            return typeof name === "string" && name !== "" ? name : undefined;
        }

        get displayName() {
            return this.name;
        }

        get parameterNames() {
            const kind = native.functionKind(this.#referent);
            if (kind === NOT_A_FUNCTION) {
                return undefined;
            }
            return [];
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

        // The Debugger.Script of a function written in JavaScript, and undefined for native
        // functions. Scripts of functions aren't reachable yet, so they are null.
        get script() {
            const kind = native.functionKind(this.#referent);
            return kind === NOT_A_FUNCTION || kind === NATIVE || kind === BOUND ? undefined : null;
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
            if (st.shouldAvoidSideEffects) {
                return null;
            }
            return st.evaluate(() => native.call(ReflectGet, undefined, [this.#referent, key, r]));
        }

        call(thisArg, ...args) {
            return this.apply(thisArg, args);
        }

        apply(thisArg, args) {
            const st = this.#st;
            if (typeof this.#referent !== "function") {
                throw new TypeError("Debugger.Object.prototype.apply: the referent is not callable");
            }
            const t = st.unwrap(thisArg);
            const a = args === undefined || args === null ? [] : ArrayFrom(args, v => st.unwrap(v));
            if (st.shouldAvoidSideEffects) {
                return null;
            }
            return st.evaluate(() => native.call(this.#referent, t, a));
        }

        executeInGlobal(code, options) {
            const st = this.#st;
            const global = native.toGlobal(this.#referent);
            if (global === undefined) {
                throw new TypeError("Debugger.Object.prototype.executeInGlobal: the referent is not a global");
            }
            code = StringCtor(code);
            // Side effects can't be detected yet, so code that must have none isn't run.
            if (st.shouldAvoidSideEffects) {
                return null;
            }
            const url = options?.url !== undefined ? StringCtor(options.url) : "debugger eval code";
            return st.evaluate(() => native.evalInGlobal(global, code, url));
        }

        asEnvironment() {
            const global = native.toGlobal(this.#referent);
            if (global === undefined) {
                return undefined;
            }
            return new DebuggerEnvironment(TOKEN, this.#st, "object", global, null);
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

        constructor(token, st, handle, source) {
            if (token !== TOKEN) {
                throw new TypeError("Debugger.Script is not a constructor");
            }
            this.#st = st;
            this.#handle = handle;
            this.#source = source;
        }

        get source() {
            return this.#source;
        }

        get url() {
            return native.scriptUrl(this.#handle) ?? this.#source.url;
        }

        get global() {
            return this.#st.wrap(native.scriptGlobal(this.#handle));
        }

        get displayName() {
            return undefined;
        }

        get format() {
            return "js";
        }

        get sourceStart() {
            return 0;
        }

        get sourceLength() {
            return this.#source.text.length;
        }

        get startLine() {
            return 1;
        }

        get startColumn() {
            return 1;
        }

        getChildScripts() {
            return [];
        }

        getPossibleBreakpoints(query) {
            return [];
        }

        getPossibleBreakpointOffsets(query) {
            return [];
        }

        getEffectfulOffsets() {
            return [];
        }

        getOffsetMetadata(offset) {
            throw new Error("Debugger.Script.prototype.getOffsetMetadata: invalid offset");
        }

        setBreakpoint(offset, handler) {
            throw new Error("Debugger.Script.prototype.setBreakpoint: invalid offset");
        }

        getBreakpoints(offset) {
            return [];
        }

        clearBreakpoint(handler, offset) { }

        clearAllBreakpoints(offset) { }

        isInCatchScope(offset) {
            return false;
        }
    }

    class DebuggerSource {
        #info;

        constructor(token, info) {
            if (token !== TOKEN) {
                throw new TypeError("Debugger.Source is not a constructor");
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
            return null;
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
            return 1;
        }

        get startColumn() {
            return 1;
        }
    }

    // Environments: so far only the object environment of a debuggee global.
    class DebuggerEnvironment {
        #st;
        #type;
        #object;
        #parent;

        constructor(token, st, type, object, parent) {
            if (token !== TOKEN) {
                throw new TypeError("Debugger.Environment is not a constructor");
            }
            this.#st = st;
            this.#type = type;
            this.#object = object;
            this.#parent = parent;
        }

        get type() {
            return this.#type;
        }

        get parent() {
            return this.#parent;
        }

        get object() {
            return this.#st.wrap(this.#object);
        }

        get calleeScript() {
            return null;
        }

        get scopeKind() {
            return null;
        }

        get optimizedOut() {
            return false;
        }

        get inspectable() {
            return true;
        }

        names() {
            return getOwnPropertyNames(this.#object);
        }

        getVariable(name) {
            const desc = getOwnPropertyDescriptor(this.#object, name);
            return desc !== undefined && "value" in desc ? this.#st.wrap(desc.value) : undefined;
        }
    }

    Debugger.Object = DebuggerObject;
    Debugger.Script = DebuggerScript;
    Debugger.Source = DebuggerSource;
    Debugger.Environment = DebuggerEnvironment;

    const host = {
        // The shim compiled the code of a script or module of a global, which hasn't run yet.
        newScript(handle, info) {
            if (active.size === 0) {
                return;
            }
            const global = native.scriptGlobal(handle);
            for (const st of interested(global)) {
                const hook = st.onNewScript;
                if (hook !== undefined) {
                    st.callHook(hook, st.dbg, [st.script(handle, info)]);
                }
            }
        },
    };

    return { Debugger, host };
})
