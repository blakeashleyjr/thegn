//! Recorded-shape fixtures for the pure Vercel Sandbox response parsers.
//!
//! These fixtures are intentionally modeled response shapes, not live captures;
//! see `fixtures/vercel/README.md`.

use thegn_core::vercel::api::{
    LogLine, SessionStatus, parse_list_page, parse_log_line, parse_sandbox, parse_session_usage,
    parse_snapshots,
};
use thegn_core::vercel::policy::tags_prove_ours;

fn fixture(name: &str) -> serde_json::Value {
    let body = match name {
        "sandbox_running.json" => {
            include_str!("fixtures/vercel/sandbox_running.json")
        }
        "sandbox_stopped.json" => {
            include_str!("fixtures/vercel/sandbox_stopped.json")
        }
        "list_page.json" => include_str!("fixtures/vercel/list_page.json"),
        "list_last.json" => include_str!("fixtures/vercel/list_last.json"),
        "snapshots.json" => include_str!("fixtures/vercel/snapshots.json"),
        "sandbox_no_name.json" => {
            include_str!("fixtures/vercel/sandbox_no_name.json")
        }
        "sandbox_capitalised_status.json" => {
            include_str!("fixtures/vercel/sandbox_capitalised_status.json")
        }
        _ => panic!("unknown JSON fixture: {name}"),
    };
    serde_json::from_str(body).expect("fixture is valid JSON")
}

#[test]
fn running_sandbox_parses_every_field_thegn_relies_on() {
    let view = parse_sandbox(&fixture("sandbox_running.json")).unwrap();

    assert_eq!(view.name, "tg-fixture-1");
    assert_eq!(view.status, SessionStatus::Running);
    assert_eq!(view.session_id.as_deref(), Some("sess_abc123"));
    assert_eq!(view.vcpus, Some(2));
    assert_eq!(view.created_at_ms, Some(1_763_500_000_000));
    assert_eq!(view.region.as_deref(), Some("iad1"));
    assert_eq!(view.cwd.as_deref(), Some("/vercel/sandbox"));
    assert!(tags_prove_ours(&view.tags, "ctl-fixture", ""));
}

#[test]
fn stopped_sandbox_has_no_session() {
    let view = parse_sandbox(&fixture("sandbox_stopped.json")).unwrap();

    assert_eq!(view.status, SessionStatus::Stopped);
    assert_eq!(view.session_id, None);
}

#[test]
fn list_pages_carry_the_cursor() {
    let (items, next) = parse_list_page(&fixture("list_page.json")).unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(next.as_deref(), Some("cur_2"));

    let (items, next) = parse_list_page(&fixture("list_last.json")).unwrap();
    assert!(items.is_empty());
    assert_eq!(next, None);
}

#[test]
fn session_usage_reads_cpu_memory_vcpus() {
    let usage = parse_session_usage(&fixture("sandbox_running.json"))
        .unwrap()
        .expect("running fixture has a session");

    assert_eq!(usage.active_cpu_ms, Some(12000));
    assert_eq!(usage.memory_mb, Some(4096));
    assert_eq!(usage.vcpus, Some(2));
}

#[test]
fn snapshots_skip_invalid_ids() {
    let (snapshots, next) = parse_snapshots(&fixture("snapshots.json")).unwrap();

    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].id, "snap_1");
    assert_eq!(next, None);
}

#[test]
fn ndjson_run_log_parses_each_stream() {
    let mut lines = include_str!("fixtures/vercel/run_log.ndjson")
        .lines()
        .map(|line| parse_log_line(line.as_bytes()).unwrap());

    assert_eq!(lines.next(), Some(LogLine::Stdout(b"hello\n".to_vec())));
    assert_eq!(lines.next(), Some(LogLine::Stderr(b"warn\n".to_vec())));
    assert_eq!(
        lines.next(),
        Some(LogLine::Command {
            id: "cmd_1".into(),
            exit_code: Some(3),
        })
    );
    assert_eq!(lines.next(), None);

    assert_eq!(
        parse_log_line(include_str!("fixtures/vercel/run_error.ndjson").as_bytes()).unwrap(),
        LogLine::Error {
            code: "sandbox_stopped".into(),
            message: "stopped".into(),
        }
    );
}

#[test]
fn missing_name_is_refused() {
    assert!(parse_sandbox(&fixture("sandbox_no_name.json")).is_err());
}

#[test]
fn capitalised_status_is_unknown_today() {
    // THE-2234: status parsing is currently case-sensitive by design.
    let view = parse_sandbox(&fixture("sandbox_capitalised_status.json")).unwrap();
    assert_eq!(view.status, SessionStatus::Unknown);
}
