Tests of mozjs 0.26.14 (servo/mozjs 138c947015, `mozjs/tests/`), unmodified; 0.26.7's and `offthread.rs`. `../tests/` holds the
copies that run against the shim, with these changes:

- `jsvalue.rs`: imports `JSVal` from `mozjs` (the shim has no `mozjs_sys` crate).
- `callback.rs`: throws with `throw_internal_error` instead of the variadic `JS_ReportErrorASCII`.

Engine differences covered by the shim's tests:

- `capture_stack.rs`: QuickJS numbers lines from 1 when the compile options say 0, and places a
  call at its `(`; the shim's copy asserts that output.
