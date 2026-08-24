//! Golden test for the bundled example plugin: `examples/plugins/hello.sh`
//! (and its `plugin.toml`) must keep loading through the REAL path — the
//! loader parses the manifest, `spawn_ndjson` runs the script, and the core
//! `PluginRuntime` accepts its messages and lands a renderable view. If the
//! wire contract or the example drifts, this fails before a user copies a
//! broken example.

use std::path::PathBuf;
use std::time::Duration;

use thegn_core::plugin_api::{PluginId, PluginRuntime, SurfaceId, View};
use thegn_svc::plugin::{discover, negotiate, spawn_ndjson};

fn examples_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins")
}

/// Stage the example as a user would: a `plugins/hello/` directory whose
/// manifest points at the script.
fn staged_config_dir() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("plugins/hello");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(
        examples_dir().join("hello.sh"),
        tmp.path().join("plugins/hello.sh"),
    )
    .unwrap();
    std::fs::copy(
        examples_dir().join("hello/plugin.toml"),
        dir.join("plugin.toml"),
    )
    .unwrap();
    tmp
}

#[test]
fn hello_example_registers_and_renders_through_the_real_path() {
    let tmp = staged_config_dir();
    let cfg = thegn_core::config::Config::default();

    // Loader: the directory manifest parses, negotiates cleanly, and anchors
    // its cwd to the plugin directory.
    let loaded = discover(&cfg, tmp.path());
    assert_eq!(loaded.len(), 1, "{loaded:?}");
    let p = &loaded[0];
    assert_eq!(p.spec.manifest.id.as_str(), "hello");
    let neg = negotiate(&p.spec).expect("hello negotiates against the host contract");
    assert_eq!(neg.accepted_contributions.len(), 1);
    assert!(neg.unsupported_contributions.is_empty());

    // Run the script exactly as the one-shot scheduler would.
    let run = spawn_ndjson(
        &p.spec.command,
        &p.spec.env,
        p.effective_cwd().as_deref(),
        Duration::from_secs(10),
    )
    .expect("hello.sh runs");
    assert!(run.junk.is_empty(), "example must not print junk: {run:?}");

    // Apply its messages to the core runtime.
    let mut rt = PluginRuntime::new(neg.clone());
    let plugin = PluginId::new("hello");
    for msg in &run.messages {
        match msg.method.as_str() {
            "register" => {
                let c = neg.accepted_contributions[0].clone();
                rt.register(plugin.clone(), c).expect("register accepted");
            }
            "update" => {
                let surface = SurfaceId::new(
                    msg.params
                        .get("surface")
                        .and_then(|s| s.as_str())
                        .expect("update names a surface"),
                );
                let view: View =
                    serde_json::from_value(msg.params.get("view").cloned().unwrap()).unwrap();
                rt.update(plugin.clone(), surface, view)
                    .expect("update accepted");
            }
            other => panic!("unexpected verb from the example: {other}"),
        }
    }
    let view = rt
        .view(&SurfaceId::new("hello.segment"))
        .expect("the segment has a view");
    assert_eq!(view.text_content(), "hello from a plugin");
}
