/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

#![allow(unsafe_op_in_unsafe_fn)]

//! Streaming-compilation host callbacks.
//!
//! The current SpiderMonkey build has no JIT backend, so it does not expose `WebAssembly` and
//! cannot create an engine-owned `StreamConsumer`. This case still exercises the complete glue
//! ABI with a real C++-layout consumer: the glue calls dispatch through the vtable methods that
//! `JS::StreamConsumer` declares. The QuickJS shim deliberately leaves those glue entry points
//! unimplemented, so QuickJS records this case as a known defect instead of pretending to stream.

use std::ffi::{CStr, c_char};
#[cfg(not(feature = "js-quickjs"))]
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};

use js::jsapi::{self, MimeType};
use js::rust::JSEngine;

use crate::support::{Observation, Value as Observed, run_case, with_global};

const CHUNK: &[u8] = b"stream chunk";
const STREAM_ERROR_CODE: usize = 1;

static NOTE_URLS: AtomicUsize = AtomicUsize::new(0);
static CONSUME_CHUNKS: AtomicUsize = AtomicUsize::new(0);
static STREAM_ENDS: AtomicUsize = AtomicUsize::new(0);
static STREAM_ERRORS: AtomicUsize = AtomicUsize::new(0);
static SOURCE_MAP_MISSING: AtomicUsize = AtomicUsize::new(0);

/// The prefix of an Itanium C++ vtable that begins at the object's vptr: the two destructor
/// entries followed by the five virtual methods in declaration order.
#[repr(C)]
struct StreamConsumerVtable {
    complete_destructor: usize,
    deleting_destructor: usize,
    consume_chunk: unsafe extern "C" fn(*mut TestConsumer, *const u8, usize) -> bool,
    stream_end: unsafe extern "C" fn(*mut TestConsumer, *mut std::ffi::c_void),
    stream_error: unsafe extern "C" fn(*mut TestConsumer, usize),
    consume_optimized_encoding: unsafe extern "C" fn(*mut TestConsumer, *const u8, usize),
    note_response_urls: unsafe extern "C" fn(*mut TestConsumer, *const c_char, *const c_char),
}

/// The first field has the same layout as `JS::StreamConsumer`; the remaining fields are private
/// test state. No engine-owned code deletes this object, so it does not need a C++ destructor.
#[repr(C)]
struct TestConsumer {
    vtable: *const StreamConsumerVtable,
    success: bool,
}

impl TestConsumer {
    fn new(success: bool) -> Self {
        TestConsumer {
            vtable: &TEST_CONSUMER_VTABLE,
            success,
        }
    }

    fn erase(&mut self) -> *mut jsapi::StreamConsumer {
        self as *mut TestConsumer as *mut jsapi::StreamConsumer
    }
}

static TEST_CONSUMER_VTABLE: StreamConsumerVtable = StreamConsumerVtable {
    complete_destructor: 0,
    deleting_destructor: 0,
    consume_chunk,
    stream_end,
    stream_error,
    consume_optimized_encoding,
    note_response_urls,
};

#[allow(dead_code)]
unsafe extern "C" fn consume_chunk(
    consumer: *mut TestConsumer,
    bytes: *const u8,
    length: usize,
) -> bool {
    assert!(!bytes.is_null());
    assert_eq!(std::slice::from_raw_parts(bytes, length), CHUNK);
    assert!((*consumer).success);
    CONSUME_CHUNKS.fetch_add(1, Ordering::SeqCst);
    true
}

#[allow(dead_code)]
unsafe extern "C" fn stream_end(_consumer: *mut TestConsumer, listener: *mut std::ffi::c_void) {
    assert!(listener.is_null());
    STREAM_ENDS.fetch_add(1, Ordering::SeqCst);
}

#[allow(dead_code)]
unsafe extern "C" fn stream_error(consumer: *mut TestConsumer, error_code: usize) {
    assert!(!(*consumer).success);
    assert_eq!(error_code, STREAM_ERROR_CODE);
    STREAM_ERRORS.fetch_add(1, Ordering::SeqCst);
}

#[allow(dead_code)]
unsafe extern "C" fn consume_optimized_encoding(
    _consumer: *mut TestConsumer,
    _bytes: *const u8,
    _length: usize,
) {
    panic!("consumeOptimizedEncoding must not be used by this case");
}

#[allow(dead_code)]
unsafe extern "C" fn note_response_urls(
    _consumer: *mut TestConsumer,
    url: *const c_char,
    source_map_url: *const c_char,
) {
    assert_eq!(
        CStr::from_ptr(url).to_str().expect("UTF-8 URL"),
        "stream.wasm"
    );
    if source_map_url.is_null() {
        SOURCE_MAP_MISSING.fetch_add(1, Ordering::SeqCst);
    } else {
        assert_eq!(
            CStr::from_ptr(source_map_url)
                .to_str()
                .expect("UTF-8 source map URL"),
            "stream.wasm.map"
        );
    }
    NOTE_URLS.fetch_add(1, Ordering::SeqCst);
}

unsafe extern "C" fn consume_callback(
    _cx: *mut jsapi::JSContext,
    _response: jsapi::HandleObject,
    _mime: MimeType,
    _consumer: *mut jsapi::StreamConsumer,
) -> bool {
    true
}

unsafe extern "C" fn report_stream_error(_cx: *mut jsapi::JSContext, _error_code: usize) {}

fn stream_observation(engine: &JSEngine) -> Observation {
    NOTE_URLS.store(0, Ordering::SeqCst);
    CONSUME_CHUNKS.store(0, Ordering::SeqCst);
    STREAM_ENDS.store(0, Ordering::SeqCst);
    STREAM_ERRORS.store(0, Ordering::SeqCst);
    SOURCE_MAP_MISSING.store(0, Ordering::SeqCst);

    with_global(engine, |cx, _global| unsafe {
        jsapi::InitConsumeStreamCallback(
            cx.raw_cx(),
            Some(consume_callback),
            Some(report_stream_error),
        );

        #[cfg(feature = "js-quickjs")]
        {
            let _ = cx;
            return Observation::new(vec![
                ("consumer_abi", Observed::String("unimplemented".to_owned())),
                (
                    "streaming_compilation",
                    Observed::String("unavailable".to_owned()),
                ),
            ]);
        }

        #[cfg(not(feature = "js-quickjs"))]
        {
            // The hand-written vtable below follows the Itanium C++ ABI. There is no portable way to
            // derive an MSVC vtable layout from Rust, so do not claim coverage on that target.
            #[cfg(not(target_env = "msvc"))]
            {
                let mut success = TestConsumer::new(true);
                js::glue::StreamConsumerNoteResponseURLs(
                    success.erase(),
                    c"stream.wasm".as_ptr(),
                    c"stream.wasm.map".as_ptr(),
                );
                assert!(js::glue::StreamConsumerConsumeChunk(
                    success.erase(),
                    CHUNK.as_ptr(),
                    CHUNK.len(),
                ));
                js::glue::StreamConsumerStreamEnd(success.erase());

                let mut failure = TestConsumer::new(false);
                js::glue::StreamConsumerNoteResponseURLs(
                    failure.erase(),
                    c"stream.wasm".as_ptr(),
                    ptr::null(),
                );
                js::glue::StreamConsumerStreamError(failure.erase(), STREAM_ERROR_CODE);

                Observation::new(vec![
                    (
                        "note_response_urls",
                        Observed::Number(NOTE_URLS.load(Ordering::SeqCst) as f64),
                    ),
                    (
                        "consume_chunk",
                        Observed::Number(CONSUME_CHUNKS.load(Ordering::SeqCst) as f64),
                    ),
                    (
                        "stream_end",
                        Observed::Number(STREAM_ENDS.load(Ordering::SeqCst) as f64),
                    ),
                    (
                        "stream_error",
                        Observed::Number(STREAM_ERRORS.load(Ordering::SeqCst) as f64),
                    ),
                    (
                        "source_map_missing",
                        Observed::Number(SOURCE_MAP_MISSING.load(Ordering::SeqCst) as f64),
                    ),
                ])
            }
            #[cfg(target_env = "msvc")]
            {
                let _ = (
                    NOTE_URLS,
                    CONSUME_CHUNKS,
                    STREAM_ENDS,
                    STREAM_ERRORS,
                    SOURCE_MAP_MISSING,
                );
                Observation::new(vec![
                    ("consumer_abi", Observed::String("unavailable".to_owned())),
                    (
                        "streaming_compilation",
                        Observed::String("msvc_vtable_unsupported".to_owned()),
                    ),
                ])
            }
        }
    })
}

pub fn run(engine: &JSEngine) {
    run_case(
        "streaming.consume_callback",
        &[
            "InitConsumeStreamCallback",
            "StreamConsumerConsumeChunk",
            "StreamConsumerNoteResponseURLs",
            "StreamConsumerStreamEnd",
            "StreamConsumerStreamError",
        ],
        || stream_observation(engine),
    );
}
