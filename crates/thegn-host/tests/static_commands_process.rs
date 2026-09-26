//! THE611 real CLI acceptance. No test dispatches an agent/provider or launches a
//! shell. HOME remains absent; private roots are not an OS filesystem sandbox.
mod static_cli_support;
use static_cli_support::{Fixture, Output, success};
use std::fs;
use std::path::Path;
use thegn_core::capability::{CATALOG, Surface, scope_of};

const STATIC: &[&[&str]] = &[
    &["config", "path"],
    &["config", "schema"],
    &["api", "list"],
    &["api", "list", "--json"],
    &["api", "coverage"],
    &["api", "coverage", "--json"],
    &["api", "schema"],
    &["completions", "bash"],
    &["completions", "bash", "--static"],
];

fn check_output(args: &[&str], output: &Output, config: &Path, bin: &str) {
    success(output);
    let text = &output.stdout;
    match (args[0], args[1]) {
        ("config", "path") => assert_eq!(text, &format!("{}\n", config.display())),
        ("config", "schema") => {
            let schema: serde_json::Value = serde_json::from_str(text).unwrap();
            assert_eq!(schema["title"], "Config");
            assert_eq!(schema["$schema"], "http://json-schema.org/draft-07/schema#");
            for key in ["sandbox", "merge_queue", "theme", "host"] {
                assert!(
                    schema["properties"].get(key).is_some(),
                    "missing schema field {key}"
                );
            }
            assert!(text.ends_with('\n'));
        }
        ("api", "schema") => assert_eq!(
            text,
            &format!(
                "{}\n",
                include_str!("../../../docs/api/control-v1.json").trim_end()
            )
        ),
        ("api", "list") if args.contains(&"--json") => {
            let rows: Vec<serde_json::Value> = serde_json::from_str(text).unwrap();
            assert_eq!(rows.len(), CATALOG.len());
            for (row, cap) in rows.iter().zip(CATALOG) {
                assert_eq!(row["id"], cap.id.as_str());
                assert_eq!(row["summary"], cap.summary);
                assert_eq!(row["scope"], format!("{:?}", scope_of(cap)).to_lowercase());
                let surfaces: Vec<_> = Surface::ALL
                    .iter()
                    .filter(|s| cap.surfaces.contains(**s))
                    .map(|s| s.as_str())
                    .collect();
                assert_eq!(row["surfaces"], serde_json::json!(surfaces));
                assert!(row.get("callable").is_some());
            }
        }
        ("api", "list") => {
            let rows: Vec<_> = text.lines().collect();
            assert_eq!(rows.len(), CATALOG.len());
            for (line, cap) in rows.iter().zip(CATALOG) {
                assert_eq!(line.split_whitespace().next(), Some(cap.id.as_str()));
                assert!(line.contains(cap.summary));
            }
        }
        ("api", "coverage") if args.contains(&"--json") => {
            let value: serde_json::Value = serde_json::from_str(text).unwrap();
            assert!(
                value["revision"]
                    .as_str()
                    .is_some_and(|revision| !revision.is_empty())
            );
            assert!(value["schema_version"].is_number());
            let rows = value["surfaces"].as_array().unwrap();
            assert_eq!(rows.len(), 5);
            for (row, surface) in rows.iter().zip([
                Surface::Http,
                Surface::Grpc,
                Surface::Cli,
                Surface::Mcp,
                Surface::Plugin,
            ]) {
                assert_eq!(row["surface"], surface.as_str());
                assert_eq!(
                    row["declared"].as_u64().unwrap(),
                    CATALOG
                        .iter()
                        .filter(|c| c.surfaces.contains(surface))
                        .count() as u64
                );
                for key in ["implemented", "stub", "excused"] {
                    assert!(row[key].as_u64().is_some());
                }
                assert_eq!(
                    row["excused"].as_u64().unwrap(),
                    row["gaps"].as_array().unwrap().len() as u64
                );
            }
        }
        ("api", "coverage") => {
            assert!(text.lines().next().unwrap().starts_with("revision: "));
            assert!(text.lines().nth(1).unwrap().starts_with("schema_version: "));
            assert!(text.lines().any(|line| line.contains("implemented")));
            for name in ["http", "grpc", "cli", "mcp", "plugin"] {
                assert!(
                    text.lines()
                        .any(|line| line.split_whitespace().next() == Some(name))
                );
            }
        }
        ("completions", "bash") => {
            assert!(
                text.contains(bin),
                "registration must target invoked executable"
            );
            let registration = text
                .lines()
                .filter(|line| line.trim_start().starts_with("complete "))
                .collect::<Vec<_>>();
            assert!(!registration.is_empty());
            assert!(
                registration
                    .iter()
                    .all(|line| line.split_whitespace().last() == Some(bin)),
                "every bash registration must target the invoked basename"
            );
            if args.contains(&"--static") {
                assert!(
                    text.contains("automations")
                        && text.contains("config")
                        && text.contains("sandbox")
                );
                assert!(!text.contains("COMPLETE=\"bash\""));
            } else {
                assert!(text.contains("COMPLETE=\"bash\""));
            }
        }
        _ => panic!("unclassified output fixture {args:?}"),
    }
}

#[test]
fn static_output_ignores_missing_malformed_config_and_invalid_overrides() {
    for malformed in [false, true] {
        let mut fixture = Fixture::new();
        let config = fixture.path("input.toml");
        if malformed {
            fs::write(&config, b"[broken TOML\n").unwrap();
        }
        let before = fixture.snapshot();
        for args in STATIC {
            let mut with_override = args.to_vec();
            with_override.extend(["--set", "this-is-not-a-config-override"]);
            let output = fixture.run(&with_override, Some(&config), false);
            check_output(args, &output, &config, "thegn");
            assert_eq!(
                fixture.snapshot(),
                before,
                "static argv {args:?} changed private state"
            );
        }
        fixture.close();
    }
}

#[test]
fn static_output_precedes_legacy_migration_and_both_profile_selectors() {
    for env_profile in [false, true] {
        let mut fixture = Fixture::new();
        fixture.legacy();
        let config = fixture.path("input.toml");
        fs::write(&config, b"[broken TOML\n").unwrap();
        let before = fixture.snapshot();
        for args in STATIC {
            let mut selected = args.to_vec();
            if !env_profile {
                selected.extend(["--profile", "fixture-profile"]);
            }
            let output = fixture.run(&selected, Some(&config), env_profile);
            check_output(args, &output, &config, "thegn");
            assert_eq!(
                fixture.snapshot(),
                before,
                "startup side effect from {args:?}"
            );
            assert!(!fixture.path("app").exists());
            assert!(!fixture.path("config/thegn").exists());
            assert!(!fixture.path("state/thegn").exists());
        }
        fixture.close();
    }
}

#[test]
fn paths_alias_registration_help_and_version_keep_their_output_contract() {
    let mut fixture = Fixture::new();
    fixture.legacy();
    let before = fixture.snapshot();
    for path in [
        Path::new("relative/missing.toml").to_path_buf(),
        fixture.path("absolute.toml"),
    ] {
        let output = fixture.run(
            &["config", "path", "--profile", "fixture-profile"],
            Some(&path),
            false,
        );
        check_output(&["config", "path"], &output, &path, "thegn");
    }
    let output = fixture.run(&["config", "path"], None, true);
    check_output(
        &["config", "path"],
        &output,
        &fixture.path("config/thegn/config.toml"),
        "thegn",
    );
    let bad_config = fixture.path("missing.toml");
    for args in [&["--help"][..], &["--version"][..]] {
        let output = fixture.run(args, Some(&bad_config), true);
        success(&output);
        if args[0] == "--help" {
            assert!(output.stdout.contains("config") && output.stdout.contains("completions"));
        } else {
            assert!(output.stdout.starts_with("thegn "));
        }
    }
    assert_eq!(fixture.snapshot(), before);
    // Keep the copied executable under outputs, excluded from state canaries.
    let alias = fixture.path(if cfg!(windows) {
        "outputs/tg.exe"
    } else {
        "outputs/tg"
    });
    fs::copy(fixture.binary(), &alias).unwrap();
    for args in [
        &["completions", "bash"][..],
        &["completions", "bash", "--static"][..],
    ] {
        let output = fixture.run_binary(&alias, args, Some(&bad_config), true, None);
        check_output(
            args,
            &output,
            &bad_config,
            if cfg!(windows) { "tg.exe" } else { "tg" },
        );
        assert_eq!(fixture.snapshot(), before);
    }
    fixture.close();
}

#[test]
fn configured_get_still_loads_file_and_runs_legacy_profile_startup() {
    let mut fixture = Fixture::new();
    fixture.legacy();
    let config = fixture.path("input.toml");
    fs::write(&config, b"[drawer]\nheight = \"17\"\n").unwrap();
    let output = fixture.run(
        &[
            "config",
            "get",
            "drawer.height",
            "--json",
            "--profile",
            "fixture-profile",
        ],
        Some(&config),
        false,
    );
    assert!(output.status.success(), "{}", output.stderr);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::json!("17")
    );
    assert_eq!(
        fs::read(fixture.path("config/thegn/sentinel")).unwrap(),
        b"private legacy bytes\n"
    );
    #[cfg(not(windows))]
    assert_eq!(
        fs::read(fixture.path("state/thegn/sentinel")).unwrap(),
        b"private legacy bytes\n"
    );
    #[cfg(windows)]
    assert_eq!(
        fs::read(fixture.path("local/thegn/sentinel")).unwrap(),
        b"private legacy bytes\n"
    );
    assert!(!fixture.path("config/superzej").exists());
    assert!(fixture.path("app/profiles/fixture-profile/state").is_dir());
    fixture.close();
}

#[cfg(unix)]
#[path = "static_cli_support/unix.rs"]
mod unix;
