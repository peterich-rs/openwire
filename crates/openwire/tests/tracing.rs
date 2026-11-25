//! Each scoped capture scenario runs alone in a fresh process. The parent
//! harness remains parallel; dispatcher/callsite caches cannot cross the boundary.

mod support;

use std::future::Future;
use std::process::{Command, Stdio};
use std::time::Duration;

use http::{Request, Response, StatusCode};
use hyper::body::Incoming;
use openwire::{Client, WireErrorKind};
use openwire_test::{ok_text, spawn_http1, RecordingEventListenerFactory, StaticDnsResolver};
use tracing_subscriber::prelude::*;

use support::*;

const CHILD_SCENARIO: &str = "OPENWIRE_TRACING_TEST_SCENARIO";
const PARENT_PID: &str = "OPENWIRE_TRACING_TEST_PARENT_PID";
const COMPLETED: &str = "OPENWIRE_TRACING_SCENARIO_COMPLETED";

fn is_child_scenario(name: &str) -> bool {
    std::env::var_os(CHILD_SCENARIO).as_deref() == Some(std::ffi::OsStr::new(name))
}

fn run_isolated_tracing_test(name: &str, scenario: impl Future<Output = ()>) {
    if is_child_scenario(name) {
        let parent_pid = std::env::var(PARENT_PID)
            .expect("parent PID supplied by tracing launcher")
            .parse::<u32>()
            .expect("numeric parent PID");
        assert_ne!(
            std::process::id(),
            parent_pid,
            "capture must run in a fresh process"
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("child Tokio runtime");
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(30), scenario)
                .await
                .expect("tracing scenario must make progress");
        });
        println!("{COMPLETED} {name} pid={}", std::process::id());
    } else if let Err(error) = run_child_scenario(name) {
        panic!("{error}");
    }
}

fn run_child_scenario(name: &str) -> Result<(), String> {
    let executable =
        std::env::current_exe().map_err(|error| format!("test executable: {error}"))?;
    let child = Command::new(executable)
        .args(["--exact", name, "--nocapture"])
        .env(CHILD_SCENARIO, name)
        .env(PARENT_PID, std::process::id().to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("launch tracing scenario {name:?}: {error}"))?;
    let pid = child.id();
    let output = child
        .wait_with_output()
        .map_err(|error| format!("wait for tracing scenario {name:?}: {error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(format!(
            "tracing scenario {name:?} failed ({})\nstdout:\n{stdout}\nstderr:\n{stderr}",
            output.status
        ));
    }
    let completed = format!("{COMPLETED} {name} pid={pid}");
    if !stdout.contains(&completed) {
        return Err(format!("tracing scenario {name:?} did not complete its selected scenario\nstdout:\n{stdout}\nstderr:\n{stderr}"));
    }
    Ok(())
}

#[test]
fn tracing_attempt_spans_record_stable_connection_reuse_fields() {
    run_isolated_tracing_test(
        "tracing_attempt_spans_record_stable_connection_reuse_fields",
        async {
            let server = spawn_http1(|_request| async move { ok_text("pooled") }).await;
            let trace = TraceCapture::default();
            let client = Client::builder().build().expect("client");

            with_trace_capture(&trace, async {
                let response = client
                    .execute(empty_request(server.http_url("/first")))
                    .await
                    .expect("response");
                let _ = response.into_body().text().await.expect("body");

                let response = client
                    .execute(empty_request(server.http_url("/second")))
                    .await
                    .expect("response");
                let _ = response.into_body().text().await.expect("body");
            })
            .await;

            let spans = trace.spans_named("openwire.attempt");
            assert_eq!(spans.len(), 2, "spans = {spans:?}");
            assert_eq!(
                spans[0].fields.get("attempt").map(String::as_str),
                Some("1")
            );
            assert_eq!(
                spans[0].fields.get("retry_count").map(String::as_str),
                Some("0")
            );
            assert_eq!(
                spans[0].fields.get("redirect_count").map(String::as_str),
                Some("0")
            );
            assert_eq!(
                spans[0].fields.get("auth_count").map(String::as_str),
                Some("0")
            );
            assert_eq!(
                spans[0].fields.get("pool_hit").map(String::as_str),
                Some("false")
            );
            assert!(!spans[0].fields.contains_key("pool_connection_id"));
            assert_eq!(
                spans[0].fields.get("connection_reused").map(String::as_str),
                Some("false")
            );
            assert_eq!(
                spans[1].fields.get("attempt").map(String::as_str),
                Some("1")
            );
            assert_eq!(
                spans[1].fields.get("auth_count").map(String::as_str),
                Some("0")
            );
            assert_eq!(
                spans[1].fields.get("pool_hit").map(String::as_str),
                Some("true")
            );
            assert_eq!(
                spans[1].fields.get("pool_connection_id"),
                spans[1].fields.get("connection_id"),
                "spans = {spans:?}",
            );
            assert_eq!(
                spans[1].fields.get("connection_reused").map(String::as_str),
                Some("true")
            );
            assert_eq!(
                spans[0].fields.get("connection_id"),
                spans[1].fields.get("connection_id"),
                "spans = {spans:?}",
            );
        },
    );
}

#[test]
fn retry_and_redirect_events_follow_stable_order_and_trace_fields() {
    run_isolated_tracing_test(
        "retry_and_redirect_events_follow_stable_order_and_trace_fields",
        async {
            let server = spawn_http1(|request: Request<Incoming>| async move {
                match request.uri().path() {
                    "/redirect-after-retry" => Response::builder()
                        .status(StatusCode::FOUND)
                        .header("location", "/redirect-target")
                        .body(http_body_util::Full::new(bytes::Bytes::new()))
                        .expect("redirect response"),
                    _ => ok_text("redirect after retry"),
                }
            })
            .await;

            let connector = FailingTcpConnector::new(1);
            let events = RecordingEventListenerFactory::default();
            let trace = TraceCapture::default();

            let client = Client::builder()
                .dns_resolver(StaticDnsResolver::new(server.addr()))
                .tcp_connector(connector)
                .event_listener_factory(events.clone())
                .max_retries(1)
                .build()
                .expect("client");

            with_trace_capture(&trace, async {
                let response = client
                    .execute(empty_request(format!(
                        "http://openwire.test:{}/redirect-after-retry",
                        server.addr().port()
                    )))
                    .await
                    .expect("response");
                let _ = response.into_body().text().await.expect("body");
            })
            .await;

            let events = events.events();
            assert_event_subsequence(
                &events,
                &[
                    "call_start GET",
                    "connect_failed ",
                    "retry 1 connect",
                    "connect_end ",
                    "connection_acquired ",
                    "response_headers_end 302 Found",
                    // Intermediate redirect body is drained so the connection can be
                    // reused before the follow-up hop starts.
                    "response_body_end ",
                    "connection_released ",
                    "redirect 1 http://openwire.test:",
                    "connection_acquired ",
                    "response_headers_end 200 OK",
                    "response_body_end 20",
                    "connection_released ",
                    "call_end",
                ],
            );

            let retry_event = trace
                .event_by_message("retrying request after connection-establishment failure")
                .or_else(|| trace.event_with_field("retry_reason", "connect"))
                .unwrap_or_else(|| {
                    panic!(
                        "retry trace event missing; captured messages = {:?}",
                        trace.event_messages()
                    )
                });
            assert_eq!(
                retry_event.fields.get("attempt").map(String::as_str),
                Some("2")
            );
            assert_eq!(
                retry_event.fields.get("retry_count").map(String::as_str),
                Some("1")
            );
            assert_eq!(
                retry_event.fields.get("redirect_count").map(String::as_str),
                Some("0")
            );
            assert_eq!(
                retry_event.fields.get("auth_count").map(String::as_str),
                Some("0")
            );
            assert_eq!(
                retry_event
                    .fields
                    .get("retry_reason")
                    .map(|value| normalize_trace_message(value)),
                Some("connect".to_owned())
            );

            let redirect_event = trace
                .event_by_message("following redirect")
                .or_else(|| {
                    trace
                        .inner
                        .lock()
                        .expect("trace capture lock")
                        .events
                        .iter()
                        .find(|event| event.fields.contains_key("redirect_location"))
                        .cloned()
                })
                .unwrap_or_else(|| {
                    panic!(
                        "redirect trace event missing; captured messages = {:?}",
                        trace.event_messages()
                    )
                });
            assert_eq!(
                redirect_event.fields.get("attempt").map(String::as_str),
                Some("3")
            );
            assert_eq!(
                redirect_event.fields.get("retry_count").map(String::as_str),
                Some("1")
            );
            assert_eq!(
                redirect_event
                    .fields
                    .get("redirect_count")
                    .map(String::as_str),
                Some("1")
            );
            assert_eq!(
                redirect_event.fields.get("auth_count").map(String::as_str),
                Some("0")
            );
            assert!(
                redirect_event
                    .fields
                    .get("redirect_location")
                    .is_some_and(|location| location.contains("/redirect-target")),
                "redirect event = {redirect_event:?}",
            );
        },
    );
}

#[test]
fn response_body_failures_emit_call_failed_but_not_response_body_end() {
    run_isolated_tracing_test(
        "response_body_failures_emit_call_failed_but_not_response_body_end",
        async {
            let server = spawn_raw_http1_response(
                b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nabc".to_vec(),
            )
            .await;
            let events = RecordingEventListenerFactory::default();
            let trace = TraceCapture::default();

            let client = Client::builder()
                .dns_resolver(StaticDnsResolver::new(server.addr()))
                .event_listener_factory(events.clone())
                .build()
                .expect("client");

            let error = with_trace_capture(&trace, async {
                let response = client
                    .execute(empty_request(format!(
                        "http://openwire.test:{}/broken",
                        server.addr().port()
                    )))
                    .await
                    .expect("response");
                assert_eq!(response.status(), StatusCode::OK);

                response
                    .into_body()
                    .text()
                    .await
                    .expect_err("body should fail")
            })
            .await;
            assert_eq!(error.kind(), WireErrorKind::Protocol);

            let events = events.events();
            assert_event_subsequence(
                &events,
                &[
                    "call_start GET",
                    "response_headers_end 200 OK",
                    "response_body_failed protocol",
                    "connection_released ",
                    "call_failed Protocol",
                ],
            );
            assert!(
                !events
                    .iter()
                    .any(|event| event.starts_with("response_body_end ")),
                "events = {events:?}",
            );

            let body_failure = trace
                .event_by_message("response body failed")
                .expect("body failure trace event");
            assert_eq!(
                body_failure.fields.get("error_kind").map(String::as_str),
                Some("protocol")
            );
            assert_eq!(
                body_failure.fields.get("bytes_read").map(String::as_str),
                Some("3")
            );
            assert_eq!(
                body_failure.fields.get("attempt").map(String::as_str),
                Some("1")
            );
            assert!(
                body_failure.fields.contains_key("call_id"),
                "body failure event = {body_failure:?}",
            );
        },
    );
}

fn isolation_probe() {
    let span = tracing::debug_span!(
        "openwire.test.isolation_probe",
        process_id = u64::from(std::process::id()),
    );
    let _entered = span.enter();
    tracing::debug!(
        process_id = u64::from(std::process::id()),
        "isolated tracing probe",
    );
}

#[test]
fn capture_is_isolated_from_parent_callsite_registration() {
    const NAME: &str = "capture_is_isolated_from_parent_callsite_registration";
    // Force a first use in an uncaptured parent thread while TRACE is enabled.
    // Its process-wide callsite state must have no effect on the child. We do
    // not assert the dependency's buggy parent result, so an upstream repair
    // will not invalidate this isolation contract.
    let _parent_dispatch = if !is_child_scenario(NAME) {
        let dispatch = tracing::Dispatch::new(
            tracing_subscriber::registry().with(tracing_subscriber::filter::LevelFilter::TRACE),
        );
        std::thread::spawn(isolation_probe)
            .join()
            .expect("uncaptured parent probe");
        Some(dispatch)
    } else {
        None
    };
    run_isolated_tracing_test(NAME, async {
        let trace = TraceCapture::default();
        with_trace_capture(&trace, async { isolation_probe() }).await;
        let spans = trace.spans_named("openwire.test.isolation_probe");
        assert_eq!(spans.len(), 1, "one captured child probe: {spans:?}");
        let pid = std::process::id().to_string();
        assert_eq!(spans[0].fields.get("process_id"), Some(&pid));
        let event = trace
            .event_by_message("isolated tracing probe")
            .expect("child event");
        assert_eq!(event.fields.get("process_id"), Some(&pid));
        assert_eq!(
            trace
                .event_messages()
                .iter()
                .filter(|message| normalize_trace_message(message) == "isolated tracing probe")
                .count(),
            1,
        );
    });
}

#[test]
fn launcher_reports_child_failure_diagnostics() {
    const NAME: &str = "launcher_reports_child_failure_diagnostics";
    if is_child_scenario(NAME) {
        panic!("intentional child diagnostic");
    }
    let error = run_child_scenario(NAME).expect_err("a failing scenario cannot pass its parent");
    assert!(error.contains("intentional child diagnostic"), "{error}");
    assert!(error.contains("stdout:"), "{error}");
    assert!(error.contains("stderr:"), "{error}");
}

#[test]
fn launcher_rejects_a_filter_that_selects_no_scenario() {
    let error = run_child_scenario("not_an_openwire_tracing_scenario")
        .expect_err("zero selected tests cannot count as a successful capture");
    assert!(
        error.contains("did not complete its selected scenario"),
        "{error}"
    );
}
