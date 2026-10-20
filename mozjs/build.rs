/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Generates the SpiderMonkey API (bindgen types, stubbed functions) from `upstream-bindgen/`,
//! forwarding every function implemented in `src/engine` (see `stub_bindings.py`).

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    for name in ["jsapi.rs", "gluebindings.rs"] {
        let status = Command::new("python3")
            .arg("stub_bindings.py")
            .arg(format!("upstream-bindgen/{name}"))
            .arg(out.join(name))
            .arg("src/engine")
            .status()
            .expect("failed to run python3");
        assert!(status.success(), "stub_bindings.py failed for {name}");
    }
    println!("cargo:rustc-check-cfg=cfg(qjs_pool_stats)");
    if std::env::var("QJS_POOL_STATS").is_ok_and(|v| !v.is_empty() && v != "0") {
        println!("cargo:rustc-cfg=qjs_pool_stats");
    }
    println!("cargo:rerun-if-env-changed=QJS_POOL_STATS");
    println!("cargo:rerun-if-changed=stub_bindings.py");
    println!("cargo:rerun-if-changed=upstream-bindgen");
    println!("cargo:rerun-if-changed=src/engine");
    icu();
}

/// Sets `qjs_intl` if the Intl subset is built, and `ICU_SUFFIX` to the suffix of ICU's symbols:
/// none for the NDK's `libicu`, and the one a system ICU's headers give them (`ucol_open_74`).
fn icu() {
    println!("cargo:rustc-check-cfg=cfg(qjs_intl)");
    let intl = std::env::var_os("CARGO_FEATURE_INTL").is_some()
        && std::env::var("CARGO_CFG_TARGET_OS").unwrap() == "linux";
    let mut suffix = String::new();
    match std::env::var("CARGO_CFG_TARGET_ENV").unwrap().as_str() {
        "ohos" if intl => println!("cargo:rustc-cfg=qjs_intl"),
        "gnu" if intl => {
            let probe = |name| {
                pkg_config::Config::new()
                    .atleast_version("65")
                    .probe(name)
                    .unwrap_or_else(|e| {
                        let hint = if matches!(e, pkg_config::Error::CrossCompilation) {
                            "point PKG_CONFIG_SYSROOT_DIR at the target's sysroot"
                        } else {
                            "install libicu-dev (or libicu-devel)"
                        };
                        panic!(
                            "the `intl` feature needs ICU 65 or later: {hint}, or build without \
                             the feature: {e}"
                        )
                    })
            };
            let icu = probe("icu-i18n");
            // Since ICU 76, a shared ICU's icu-i18n lists icu-uc only in `Requires.private`.
            probe("icu-uc");
            if let Ok(dir) = pkg_config::get_variable("icu-i18n", "pcfiledir") {
                println!("cargo:rerun-if-changed={dir}/icu-i18n.pc");
            }
            suffix = symbol_suffix(&icu);
            println!("cargo:rustc-cfg=qjs_intl");
        }
        _ => {}
    }
    println!("cargo:rustc-env=ICU_SUFFIX={suffix}");
}

/// The suffix ICU's headers append to its function names (U_ICU_ENTRY_POINT_RENAME), which is
/// empty if ICU was built without renaming.
fn symbol_suffix(icu: &pkg_config::Library) -> String {
    let source = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("icu_symbol.c");
    std::fs::write(&source, "#include <unicode/ucol.h>\nICU_SYMBOL ucol_open\n").unwrap();
    let expanded = cc::Build::new()
        .file(&source)
        .includes(&icu.include_paths)
        .cargo_warnings(false)
        .expand();
    let expanded = String::from_utf8_lossy(&expanded);
    // GCC can put the expansion on a later line, after a line marker.
    let (_, tail) = expanded.rsplit_once("ICU_SYMBOL").unwrap();
    tail.lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .flat_map(str::split_whitespace)
        .next()
        .and_then(|symbol| symbol.strip_prefix("ucol_open"))
        .expect("ICU's headers don't declare ucol_open")
        .to_owned()
}
