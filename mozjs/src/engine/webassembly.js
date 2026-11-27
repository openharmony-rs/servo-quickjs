// QuickJS has no WebAssembly. Servo exposes the namespace anyway, with APIs that fail
// explicitly, so that feature detection (`typeof WebAssembly`, `WebAssembly.Module`) does not
// throw a ReferenceError and callers get a clear CompileError instead. This mirrors engines
// where WebAssembly is disabled by policy: the binding is present, compiling is rejected.
(() => {
  const g = globalThis;
  if (typeof g.WebAssembly !== "undefined")
    return;

  class CompileError extends Error {
    constructor(message) {
      super(message);
      this.name = "CompileError";
    }
  }
  class LinkError extends Error {
    constructor(message) {
      super(message);
      this.name = "LinkError";
    }
  }
  class RuntimeError extends Error {
    constructor(message) {
      super(message);
      this.name = "RuntimeError";
    }
  }

  const UNSUPPORTED = "WebAssembly is not supported in this build";
  const reject = () => Promise.reject(new CompileError(UNSUPPORTED));
  const fail = () => { throw new CompileError(UNSUPPORTED); };

  class Module { constructor() { fail(); } }
  class Instance { constructor() { fail(); } }
  class Memory { constructor() { fail(); } }
  class Table { constructor() { fail(); } }
  class Global { constructor() { fail(); } }

  g.WebAssembly = {
    CompileError,
    LinkError,
    RuntimeError,
    Module,
    Instance,
    Memory,
    Table,
    Global,
    compile: reject,
    instantiate: reject,
    validate: () => false,
    compileStreaming: reject,
    instantiateStreaming: reject,
  };
  Object.defineProperty(g.WebAssembly, Symbol.toStringTag, { value: "WebAssembly" });
})();
