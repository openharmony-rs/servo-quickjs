# QuickJS shim fix record

This file is the A4 hand-off between "fix the shim and regress" and the next full QuickJS run.
`quickjs-difference-allowlist.toml` records only differences that are still present. When a shim
fix makes an entry disappear, add a record here before deleting the entry from the allowlist.

Required for every fix:

- one or more contract case ids that covered the defect;
- the QuickJS commit or change that fixed it;
- what changed in the shim, in one or two sentences;
- the command and result that proved the difference disappeared.

The regression command must run both engines with the same case selection and finish with
`run.sh compare-engine-results`. A fix is not closed until the comparison reports zero missing cases, zero unaccepted
differences, and no entry for the case being removed (or a `pending`/`bug` entry while follow-up
work remains).

## Template

Copy this block for each fix. Keep newest records first.

```markdown
## <case id>

- Date: YYYY-MM-DD
- Owner: <name or team>
- Issue: <link or "none">
- Previous status: <pending | bug | accepted | wontfix>
- Previous severity: <P0 | P1 | P2 | P3>
- Fixed in: <servo-quickjs commit sha or PR>
- Shim change: <what changed and why the contract now matches SpiderMonkey>
- Regression: `run.sh run-spidermonkey-contract --case <case id> && run.sh run-quickjs-contract --case <case id> && run.sh compare-engine-results`
- Result: `0 mismatches, 0 unaccepted, 0 missing`
- Follow-up: <none, or the remaining accepted difference and its case id>
```

## Records

## debugger.global_object

- Date: 2026-11-16
- Owner: servo-ohos engine contract
- Issue: none
- Previous status: accepted
- Previous severity: P3
- Fixed in: servo-quickjs 6ac4a17
- Shim change: `JS_DefineDebuggerObject` emulates SpiderMonkey's `Debugger` API instead of panicking from a `qjs_todo!` stub. The shim builds that emulation behind its `debugger` feature; the contract runner now builds the QuickJS target with `SERVO_QUICKJS_DEBUGGER=1` so the case exercises the real entry point.
- Regression: `run.sh run-spidermonkey-contract --case debugger.global_object && run.sh run-quickjs-contract --case debugger.global_object && run.sh compare-engine-results`
- Result: `1 compared, 0 mismatches, 0 unaccepted, 0 missing`; both engines record `{"ran":true}`
- Follow-up: none
