/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Engine-independent contract tests for the `mozjs` API served by SpiderMonkey or QuickJS.
//!
//! Every case declares the C entry points it exercises in `apis`. `servo-mozjs-api-usage.toml` records the
//! entries Servo actually uses, and the coverage gate requires a case for every scoped entry:
//! `implemented` entries count as normal coverage, while `empty`/`todo` entries also need a case
//! whose QuickJS divergence must be registered in `quickjs-difference-allowlist.toml`.
#![allow(unsafe_op_in_unsafe_fn)]

// `js::quickjs` only exists in the servo-quickjs shim, never in crates.io `mozjs`. Cargo ignores a
// `[patch.crates-io]` whose version does not match the pinned requirement, so without this the
// QuickJS run would quietly build SpiderMonkey and compare it against itself. Keep the check
// compile-time so a mis-patched run cannot even start.
#[cfg(feature = "js-quickjs")]
const _: () = {
    #[allow(dead_code)]
    fn assert_quickjs_shim_is_linked() {
        let _ = js::quickjs::runtime_alive;
    }
};

#[cfg(test)]
mod buffer_test;
#[cfg(test)]
mod class_test;
#[cfg(test)]
mod clone_test;
#[cfg(test)]
mod contract_test;
#[cfg(test)]
mod debugger_test;
#[cfg(test)]
mod errors_test;
#[cfg(test)]
mod eval_test;
#[cfg(test)]
mod event_loop_test;
#[cfg(test)]
mod misc_test;
#[cfg(test)]
mod module_test;
#[cfg(test)]
mod objects_test;
#[cfg(test)]
mod promises_test;
#[cfg(test)]
mod property_test;
#[cfg(test)]
mod proxy_test;
#[cfg(test)]
mod realm_test;
#[cfg(test)]
mod runtime_test;
#[cfg(test)]
mod stencil_test;
#[cfg(test)]
mod streaming_test;
#[cfg(test)]
mod strings_test;
#[cfg(test)]
mod support;

#[cfg(test)]
mod tests {
    #[test]
    fn engine_contract() {
        // With CONTRACT_ISOLATE_CASES=1 this re-executes the binary once per case and exits; a
        // crash in one case then cannot hide the cases after it.
        super::support::run_isolated();
        let engine = js::rust::JSEngine::init().expect("initializing the JS engine");
        super::support::run_module("buffer", || super::buffer_test::run(&engine));
        super::support::run_module("class", || super::class_test::run(&engine));
        super::support::run_module("clone", || super::clone_test::run(&engine));
        super::support::run_module("contract", || super::contract_test::run(&engine));
        super::support::run_module("errors", || super::errors_test::run(&engine));
        super::support::run_module("eval", || super::eval_test::run(&engine));
        super::support::run_module("debugger", || super::debugger_test::run(&engine));
        super::support::run_module("event_loop", || super::event_loop_test::run(&engine));
        super::support::run_module("streaming", || super::streaming_test::run(&engine));
        super::support::run_module("misc", || super::misc_test::run(&engine));
        super::support::run_module("module", || super::module_test::run(&engine));
        super::support::run_module("objects", || super::objects_test::run(&engine));
        super::support::run_module("property", || super::property_test::run(&engine));
        super::support::run_module("proxy", || super::proxy_test::run(&engine));
        super::support::run_module("promises", || super::promises_test::run(&engine));
        super::support::run_module("realm", || super::realm_test::run(&engine));
        super::support::run_module("runtime", || super::runtime_test::run(&engine));
        super::support::run_module("strings", || super::strings_test::run(&engine));
        super::support::run_module("stencil", || super::stencil_test::run(&engine));
        super::support::assert_no_case_failures();
    }
}
