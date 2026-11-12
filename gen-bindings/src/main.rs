//! Regenerates `servo-quickjs/src/bindings.rs` and `servo-quickjs/src/static_wrappers.c`.
//! Run from the workspace root: `LIBCLANG_PATH=/usr/lib/llvm-18/lib cargo run -p gen-bindings`.

fn main() {
    let out = std::path::Path::new("servo-quickjs/src");
    bindgen::Builder::default()
        .header("servo-quickjs/quickjs/quickjs.h")
        // The debugger API stays declared; mozjs only calls it when QuickJS has it.
        .clang_arg("-DCONFIG_DEBUGGER")
        .allowlist_function("(__)?JS_.*|js_.*")
        .allowlist_type("JS.*")
        .allowlist_var("JS_.*")
        // Reference counting is inlined in Rust (servo-quickjs/src/lib.rs): an FFI call per count
        // change showed up in profiles of DOM access.
        .blocklist_function("JS_(Dup|Free)Value(RT)?|JS_(Dup|Free)Context")
        .wrap_static_fns(true)
        .wrap_static_fns_path(out.join("static_wrappers"))
        .wrap_static_fns_suffix("__sys")
        .layout_tests(false)
        .size_t_is_usize(true)
        .prepend_enum_name(false)
        .derive_default(true)
        .generate()
        .expect("bindgen failed")
        .write_to_file(out.join("bindings.rs"))
        .expect("write failed");
    let wrappers = out.join("static_wrappers.c");
    let c = std::fs::read_to_string(&wrappers).unwrap();
    std::fs::write(
        &wrappers,
        c.replace("\"servo-quickjs/quickjs/quickjs.h\"", "\"quickjs.h\""),
    )
    .unwrap();
}
