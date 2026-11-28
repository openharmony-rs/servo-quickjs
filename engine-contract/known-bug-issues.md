# Tracking issues for `known-bug` differences

`quickjs-difference-allowlist.toml` allows a `known-bug` entry to keep the gate green, but only when
the entry names the issue that tracks the defect. A single issue cannot honestly track unrelated
shim defects, so each entry names the issue whose scope covers it.

File defects in <https://github.com/openharmony-rs/servo-quickjs/issues>.

| Issue | Scope | Allowlist entries |
| --- | --- | --- |
| [#25](https://github.com/openharmony-rs/servo-quickjs/issues/25) | `JS_GetGCParameter`/`JS_SetGCParameter` do not persist, and `runtime.context_lifecycle` passes anyway | `runtime.context_lifecycle` |
| [#28](https://github.com/openharmony-rs/servo-quickjs/issues/28) | `JS_FireOnNewGlobalObject` never dispatches | `debugger.new_global_object_hook` |
| [#33](https://github.com/openharmony-rs/servo-quickjs/issues/33) | `JS_GetScriptedCallerPrivate` is hidden by `HideScriptedCaller` | `errors.get_scripted_caller_private_api` |

Each issue carries the full analysis, the code locations and the reproduction command; this file
only records which allowlist entry points at which issue.

## Entries that keep issue 57

`eval.environment_chain`, `edge.eval.compile_and_environment`, `edge.weakmap`, `edge.weakmap_overwrite`,
`objects.weakmap_wpt_get_entry` and `objects.weakmap_wpt_set_entry` stay on
<https://github.com/openharmony-rs/servo-ohos/issues/57>, which covers exactly those two defects: the
environment-chain append order and the holder-map key type.

## Differences that are not defects

`streaming.consume_callback` is not a defect at all. QuickJS has no WebAssembly, so it has no stream
consumer; the entry is `accepted` instead of `known-bug`.

`misc.standard_classes` is `accepted` as well: QuickJS defines every standard class eagerly, so a
global's resolve and enumerate hooks have no class left to add lazily. The contract case observes
different hook return values, but no JS-visible behaviour difference has been demonstrated.

`rooter.trace_on_gc`, `class.edge_tracers`, `realm.enter_exit_and_private` and the callback/tracer
part of `misc.gc_callbacks` are accepted because QuickJS does not trace Rust roots: the shim counts
or pools them instead (`docs/design.md:59-62`), and its cycle collector calls a trace hook once per
pass (`quickjs.c:6975`, `:7017`). `misc.roots_options_memory` is accepted because the object that
survives `RemoveRawValueRoot` is held by the autorelease pool until the task ends
(`docs/design.md:29,36`). `promise.internal_job_queue` is accepted because its case installs
QuickJS's own queue with `UseInternalJobQueues`; with Servo's queue installed, `enqueue_job` puts
every job in the queue that `HasAnyMicroTasks` reads.

`errors.core` remains accepted only for the reported column numbers. The `JS_GetScriptedCallerPrivate`
divergence it used to absorb is now its own case, `errors.get_scripted_caller_private_api`, tracked
in #33.
