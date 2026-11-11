/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! A build without the debugger (see `debugger.rs`): QuickJS has no patches for debuggers, and
//! no debugger is ever active, so what the shim does for debuggers compiles away.

use std::ffi::{c_char, c_int, CStr};

use servo_quickjs as q;

use super::Cx;
use crate::sys::jsapi as root;

#[derive(Default)]
pub(crate) struct State;

impl State {
    pub(crate) fn is_active(&self) -> bool {
        false
    }

    pub(crate) fn take_frame_eval_check(&self) -> bool {
        false
    }
}

pub(crate) unsafe fn JS_DefineDebuggerObject(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
) -> bool {
    let _ = (cx, obj);
    panic!(
        "JS_DefineDebuggerObject: servo-quickjs was built without the debugger (the `debugger` \
         feature, or SERVO_QUICKJS_DEBUGGER=1)"
    )
}

pub(crate) unsafe fn free_debuggers(_cx: &Cx) {}

pub(crate) unsafe fn reinterrupt(_cx: &Cx, _qctx: *mut q::JSContext) {}

#[must_use]
pub(crate) unsafe fn new_script(
    _cx: &Cx,
    _code: q::JSValue,
    _url: &CStr,
    _text: &[u8],
    _introduction_type: *const c_char,
    _start_line: u32,
) -> c_int {
    0
}
