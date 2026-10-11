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
    println!("cargo:rerun-if-changed=stub_bindings.py");
    println!("cargo:rerun-if-changed=upstream-bindgen");
    println!("cargo:rerun-if-changed=src/engine");
    icu();
}

/// Sets `qjs_intl` if the Intl subset is built, and `ICU_SUFFIX` to the suffix of ICU's symbols:
/// none for the NDK's `libicu`, the major version for a system ICU (`ucol_open_74`).
fn icu() {
    println!("cargo:rustc-check-cfg=cfg(qjs_intl)");
    let intl = std::env::var_os("CARGO_FEATURE_INTL").is_some()
        && std::env::var("CARGO_CFG_TARGET_OS").unwrap() == "linux";
    let mut suffix = String::new();
    match std::env::var("CARGO_CFG_TARGET_ENV").unwrap().as_str() {
        "ohos" if intl => println!("cargo:rustc-cfg=qjs_intl"),
        "gnu" if intl => {
            let icu = pkg_config::Config::new()
                .probe("icu-i18n")
                .unwrap_or_else(|e| {
                    panic!(
                        "the `intl` feature needs the system ICU (libicu-dev, or libicu-devel), \
                         or build without the feature: {e}"
                    )
                });
            suffix = format!("_{}", icu.version.split('.').next().unwrap());
            println!("cargo:rustc-cfg=qjs_intl");
        }
        _ => {}
    }
    println!("cargo:rustc-env=ICU_SUFFIX={suffix}");
}
