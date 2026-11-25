# Tracing Test Isolation Fixes

Status: implemented and verified on macOS arm64 with Rust 1.99.1.
Base: `e74fa27337b605ce1ad246e4851fcec30e798354`; independent of the Age repair in PR #82.

Issue 1 [P1]: Isolate scoped tracing capture from parallel callsite registration

File: `crates/openwire/tests/support/harness.rs` — `with_trace_capture` and
`trace_test_lock` (baseline lines 2413–2430).

Problem: `tracing_attempt_spans_record_stable_connection_reuse_fields`
(`tests/events.rs`, baseline lines 210–291) can capture no spans in a parallel
run. This was reproduced on unmodified main (10 passed, one failed).
[PR #82's macOS default-feature CI](https://github.com/peterich-rs/openwire/actions/runs/43404057007/job/131813850616)
also lost the body-failure trace event in `timeout_cancel.rs` (19 passed,
one failed). The lock only excludes other capture tests; ordinary
HTTP tests still register the same production callsites in this process.

In tracing-core 0.1.36, `Dispatchers::rebuilder` (`callsite.rs`, lines 544–547)
chooses `JustOne` when one subscriber is registered; its `for_each` (lines
562–566) queries the current thread's dispatcher. A cold callsite registered by
an uncaptured test thread can therefore retain `Interest::never()` for a later
captured request. See the [official versioned source](https://docs.rs/tracing-core/0.1.36/src/tracing_core/callsite.rs.html).

Approach — Run each capture scenario in a fresh test process:

Move the three integration capture scenarios into `tests/tracing.rs`. Each
parent test invokes its own executable with `--exact` and the scenario name;
a private child marker executes the scenario once in a Tokio runtime. Verify
the parent/child process boundary, successful child completion, and a completion
marker so a misspelled filter cannot silently pass with zero selected tests.
Forward child output on failure.

A process boundary removes shared dispatcher/callsite state between unrelated
scenarios while the parent tests remain parallel. Merely locking capture tests
does not exclude ordinary tests. A temporary second dispatcher or rebuilding
the interest cache does not exclude an already in-flight cold registration.
Global single-thread testing hides the test-isolation defect and is not the fix.

The private helper in `tests/tracing.rs` (line 26) is:

```rust
fn run_isolated_tracing_test(name: &str, scenario: impl std::future::Future<Output = ()>)
```

Structural changes:

- Move the two capture cases from `tests/events.rs` and the body-failure capture
  case from `tests/timeout_cancel.rs` into the new tracing test binary.
- Remove the obsolete capture-only async lock from `tests/support/harness.rs`.
- Keep scoped capture and all production code, dependencies and CI parallelism.
- The synchronous unit capture case in `src/transport/tests.rs` (baseline line
  146) uses a callsite unique to that one test, so it does not share the cold
  registration window and remains unchanged.

Existing test adjustments:

- `tracing_attempt_spans_record_stable_connection_reuse_fields`
  (`tests/events.rs`:210): preserve all field/reuse assertions in the child.
- `retry_and_redirect_events_follow_stable_order_and_trace_fields`
  (`tests/events.rs`:400): preserve event ordering and retry/redirect fields.
- `response_body_failures_emit_call_failed_but_not_response_body_end`
  (`tests/timeout_cancel.rs`:555): preserve failure, lifecycle and byte-count
  assertions; only the execution location changes.

New tests:

- `capture_is_isolated_from_parent_callsite_registration` (`tests/tracing.rs`:399):
  prime a static trace probe in the parent without a capturing default while
  a TRACE dispatcher is registered, then capture that probe in the child.
  Assert one span/event and the child PID, demonstrating state isolation.
- `run_child_scenario` (`tests/tracing.rs`:52) requires a child-completion
  marker containing the selected name and actual child PID. A successful exit
  with zero selected tests cannot count as a capture.
- `launcher_reports_child_failure_diagnostics` (`tests/tracing.rs`:439): run
  an intentional child panic and check that its diagnostic reaches the parent.
- `launcher_rejects_a_filter_that_selects_no_scenario` (`tests/tracing.rs`:451):
  a missing test name must fail despite the child's successful zero-test exit.

Verification

- Run the tracing binary with its default parallel test runner and multiple
  threads. Causal isolation assertions are the proof; repeats are stress only.
- Run workspace default/all-feature tests with normal parallelism, doctests,
  formatting and clippy.
- Update `docs/testing.md` with the capture-only process boundary. No network
  protocol behavior or production tracing contract changes; no RFC change.
- The three migrated scenario bodies were compared with main after ignoring
  whitespace and the removed lock; all original assertions and actions remain.

Results (normal parallel test runners; no RUST_TEST_THREADS override):

| Command | Result |
| --- | --- |
| `cargo test --workspace --all-targets` | 567 passed, 16 ignored; benchmark/example targets completed |
| `cargo test --workspace --all-targets --all-features` | 608 passed, 16 ignored; benchmark/example targets completed |
| `cargo test --workspace --all-features --doc` | 2 passed |
| `cargo test -p openwire --test events --test timeout_cancel --test tracing -- --test-threads=8` | 34 passed; 10 further fresh-process rounds also passed (340 cases) |
| `cargo clippy --workspace --all-targets -- -D warnings` | passed |
| `cargo fmt --all --check` | passed |

Local commands used a shared CARGO_TARGET_DIR and CARGO_INCREMENTAL=0 to avoid
regenerable build-cache duplication; no test scheduling behavior was overridden.
The complete suites exercise all normal parallel tests. PID checks and the cold
probe provide structural isolation evidence; repeated runs are stress evidence,
not the proof of isolation. Production code, public API and dependencies are
unchanged. Desktop CI on other operating systems supplies separate evidence.
