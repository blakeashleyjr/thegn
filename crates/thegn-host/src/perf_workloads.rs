//! Opt-in bounded workloads for candidate hot paths. No terminal or live DB.

#[cfg(unix)]
#[path = "perf_workloads_hydration.rs"]
mod hydration;

use std::time::Instant;
use termwiz::surface::{Change, Position, Surface};

fn median(mut samples: Vec<u128>) -> u128 {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

#[test]
#[ignore = "controlled performance investigation; machine-dependent, opt-in only"]
#[allow(
    clippy::disallowed_macros,
    reason = "opt-in fixture emits measured artifact"
)]
fn controlled_perf_workload() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let config = tmp.path().join("config");
    let runtime_state = tmp.path().join("runtime");
    std::fs::create_dir_all(config.join("thegn")).unwrap();
    std::fs::write(config.join("thegn/config.toml"), "[ui]\nlanguage = 'en'\n").unwrap();
    let _env = crate::testenv::EnvVarGuard::set(&[
        ("XDG_STATE_HOME", state.to_str().unwrap()),
        ("XDG_CONFIG_HOME", config.to_str().unwrap()),
        ("THEGN_DIR", runtime_state.to_str().unwrap()),
        ("APPDATA", config.to_str().unwrap()),
        ("LOCALAPPDATA", state.to_str().unwrap()),
    ]);
    let db = thegn_core::db::Db::open_at(&tmp.path().join("fixture.db")).unwrap();
    let cfg = thegn_core::config::Config::default();
    let approvals = thegn_core::config_resolve::Approvals::deny_all();
    let repo = tmp.path().join("synthetic-repo");
    std::fs::create_dir_all(&repo).unwrap();
    let mut output = Vec::new();
    for rows in [1, 8, 32] {
        let mut config_times = Vec::new();
        let mut resolve_times = Vec::new();
        for _ in 0..9 {
            let t = Instant::now();
            std::hint::black_box(crate::hydrate::load_hydration_config());
            config_times.push(t.elapsed().as_micros());
            let t = Instant::now();
            for row in 0..rows {
                let path = repo.join(format!("row-{row}"));
                let env = crate::handlers::repo_trust::effective_environment_for_worktree(
                    &cfg,
                    Some(&db),
                    &repo,
                    &path,
                    Some(""),
                    &approvals,
                );
                std::hint::black_box(env);
            }
            resolve_times.push(t.elapsed().as_micros());
        }
        output.push(serde_json::json!({
            "workload": "hydration_config_and_effective_environment",
            "rows": rows, "samples": 9,
            "config_median_us": median(config_times),
            "environment_median_us": median(resolve_times),
            "scope": "synthetic empty-state candidate phases, not full model hydration"
        }));
    }
    for (cols, rows) in [(80, 24), (160, 48), (240, 72)] {
        let mut source = Surface::new(cols, rows);
        for row in 0..rows {
            source.add_changes(vec![
                Change::CursorPosition {
                    x: Position::Absolute(0),
                    y: Position::Absolute(row),
                },
                Change::Text(format!(
                    "row {row:03} terminal output 世 {}",
                    "x".repeat(cols / 3)
                )),
            ]);
        }
        let baseline = source.clone();
        source.add_changes(vec![
            Change::CursorPosition {
                x: Position::Absolute(2),
                y: Position::Absolute(2),
            },
            Change::Text("changed".into()),
        ]);
        let mut bounded_times = Vec::new();
        let mut resync_times = Vec::new();
        let mut resync_changes = 0;
        for _ in 0..15 {
            let t = Instant::now();
            let bounded = baseline.diff_region(0, 2, cols, 1, &source, 0, 2);
            bounded_times.push(t.elapsed().as_micros());
            std::hint::black_box(bounded);
            let t = Instant::now();
            let full = crate::compositor::full_repaint_changes(&mut source);
            resync_times.push(t.elapsed().as_micros());
            resync_changes = full.len();
            let mut restored = Surface::new(cols, rows);
            restored.add_changes(full);
            for (actual, expected) in restored.screen_cells().iter().zip(source.screen_cells()) {
                for (actual, expected) in actual.iter().zip(expected) {
                    assert_eq!(actual.str(), expected.str());
                    assert_eq!(actual.attrs(), expected.attrs());
                }
            }
        }
        output.push(serde_json::json!({
            "workload": "bounded_diff_vs_full_resync", "cols": cols, "rows": rows,
            "samples": 15, "bounded_diff_median_us": median(bounded_times),
            "resync_median_us": median(resync_times), "resync_changes": resync_changes,
            "equivalent_cells": true,
            "scope": "actual surface functions; excludes pane compose, writer, terminal and system load"
        }));
    }
    println!("{}", serde_json::to_string_pretty(&output).unwrap());
}
