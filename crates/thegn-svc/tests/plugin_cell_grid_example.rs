//! Contract tests for the resident cell-grid example plugin.

use std::io::Write;
use std::path::PathBuf;

use thegn_core::plugin_api::ExtensionPoint;
use thegn_core::plugin_app::{AppView, Widget};
use thegn_svc::plugin::{discover, negotiate};

fn examples_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins")
}

#[test]
fn cell_grid_example_manifest_negotiates_one_app_tab() {
    let tmp = tempfile::tempdir().unwrap();
    let source = examples_dir().join("cell-grid");
    let staged = tmp.path().join("plugins/cell-grid");
    std::fs::create_dir_all(&staged).unwrap();
    std::fs::copy(source.join("plugin.toml"), staged.join("plugin.toml")).unwrap();
    std::fs::copy(source.join("grid.py"), staged.join("grid.py")).unwrap();

    let (loaded, problems) = discover(&thegn_core::config::Config::default(), tmp.path());
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(loaded.len(), 1, "{loaded:?}");
    assert_eq!(loaded[0].spec.manifest.id.as_str(), "cell-grid");

    let neg = negotiate(&loaded[0].spec).expect("negotiates");
    assert_eq!(neg.accepted_contributions.len(), 1);
    assert_eq!(
        neg.accepted_contributions[0].extension_point,
        ExtensionPoint::AppTab
    );
    assert!(neg.unsupported_contributions.is_empty());
}

#[test]
fn cell_grid_example_answers_activate_key_resize_and_stale_session_over_the_real_wire() {
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: python3 is not installed");
        return;
    }

    let mut child = std::process::Command::new("python3")
        .arg("grid.py")
        .current_dir(examples_dir().join("cell-grid"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("python3 child starts");

    let input = concat!(
        "{\"id\":1,\"method\":\"activate\",\"params\":{\"session\":\"s1\"}}\n",
        "{\"id\":2,\"method\":\"on_event\",\"params\":{\"kind\":\"App\",\"payload\":{\"surface\":\"cell-grid.demo\",\"session\":\"s1\",\"event\":\"key\",\"data\":{\"widget\":\"grid\",\"key\":{\"name\":\"character\",\"value\":\"a\"},\"modifiers\":{\"control\":false,\"alt\":false,\"shift\":false,\"super_key\":false}}}}}\n",
        "{\"method\":\"on_event\",\"params\":{\"kind\":\"App\",\"payload\":{\"surface\":\"cell-grid.demo\",\"session\":\"s1\",\"event\":\"resize\",\"data\":{\"widget\":\"grid\",\"width\":3,\"height\":2}}}}\n",
        "{\"id\":4,\"method\":\"on_event\",\"params\":{\"kind\":\"App\",\"payload\":{\"surface\":\"cell-grid.demo\",\"session\":\"WRONG\",\"event\":\"key\",\"data\":{\"widget\":\"grid\",\"key\":{\"name\":\"character\",\"value\":\"z\"},\"modifiers\":{\"control\":false,\"alt\":false,\"shift\":false,\"super_key\":false}}}}}\n",
    );
    child
        .stdin
        .take()
        .expect("child stdin is piped")
        .write_all(input.as_bytes())
        .expect("write protocol frames");

    let output = child.wait_with_output().expect("wait for python child");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "python child failed: {stderr}");
    assert!(output.stderr.is_empty(), "python wrote stderr: {stderr}");

    let stdout = String::from_utf8(output.stdout).expect("plugin stdout is UTF-8");
    let messages: Vec<serde_json::Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).expect("stdout line is JSON"))
        .collect();
    assert_eq!(messages.len(), 5, "{messages:?}");
    assert_eq!(messages[0]["method"], "update");
    assert_eq!(messages[1]["id"], 2);
    assert!(messages[1]["result"].is_null());
    assert_eq!(messages[2]["method"], "update");
    assert_eq!(messages[3]["method"], "update");
    assert_eq!(messages[4]["id"], 4);
    assert_eq!(messages[4]["error"]["code"], "denied");

    for (message, revision) in [(&messages[0], 1), (&messages[2], 2), (&messages[3], 3)] {
        assert_eq!(message["params"]["surface"], "cell-grid.demo");
        let app: AppView = serde_json::from_value(message["params"]["app"].clone())
            .expect("update app deserializes as AppView");
        app.validate().expect("AppView validates");
        assert_eq!(app.revision, revision);
        match &app.root {
            Widget::Grid {
                id,
                width,
                height,
                cells,
                ..
            } => {
                assert_eq!(id, "grid");
                assert!(!cells.iter().any(|cell| cell.span.text == "z"));
                match revision {
                    1 => {
                        assert_eq!((*width, *height), (80, 24));
                    }
                    2 => {
                        assert!(
                            cells.iter().any(|cell| {
                                cell.x == 0 && cell.y == 0 && cell.span.text == "a"
                            })
                        );
                    }
                    3 => assert_eq!((*width, *height), (3, 2)),
                    _ => unreachable!("only revisions 1 through 3 are expected"),
                }
            }
            other => panic!("expected grid widget, got {other:?}"),
        }
    }
}
