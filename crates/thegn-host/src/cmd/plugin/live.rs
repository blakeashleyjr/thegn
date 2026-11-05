use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};
use tg_kit::{AppTile, ChangeHook};
use thegn_core::plugin_api::{
    API_VERSION, ExtensionPoint, PluginCallback, PluginMode, RpcError, RpcErrorCode, RpcResponse,
};
use thegn_core::plugin_app::AppView;
use thegn_svc::plugin::{
    LoadedPlugin, ResidentSession, ResidentSupervisor, SessionEvent, SessionWriter,
};

pub(super) const DEFAULT_WAIT_MS: u64 = 3000;
const MIN_WAIT_MS: u64 = 100;
const MAX_WAIT_MS: u64 = 30_000;
const CLOSE_WAIT: Duration = Duration::from_secs(5);

use super::trace::Trace;
use std::sync::{Arc, Mutex};

/// The session label every live record carries. The real session token is never written to a trace.
const LIVE_TRACE_SESSION: &str = "live";
type SharedTrace = Arc<Mutex<Trace>>;

/// Metadata for one plugin event, excluding params, results and junk text.
fn event_frame(event: &SessionEvent) -> Value {
    match event {
        SessionEvent::Message(message) => json!({"method": message.method, "id": message.id}),
        SessionEvent::Response(response) => {
            let mut frame = json!({"method": "response", "id": response.id});
            if response.error.is_some() {
                frame["error"] = json!(true);
            }
            frame
        }
        SessionEvent::Junk(_) => json!({"method": "junk"}),
        SessionEvent::Exit { .. } => json!({"method": "exit"}),
    }
}

fn capture_event(trace: &SharedTrace, started: Instant, event: &SessionEvent) {
    super::preview::record(
        trace,
        "incoming",
        LIVE_TRACE_SESSION,
        started,
        &event_frame(event),
    );
}

fn capture_outgoing(
    trace: &SharedTrace,
    started: Instant,
    method: &str,
    id: Option<u64>,
    error: bool,
) {
    let mut frame = json!({"method": method, "id": id});
    if error {
        frame["error"] = json!(true);
    }
    super::preview::record(trace, "outgoing", LIVE_TRACE_SESSION, started, &frame);
}

fn precheck_record(record: Option<&Path>) -> Result<()> {
    if let Some(path) = record
        && std::fs::symlink_metadata(path).is_ok()
    {
        bail!(
            "trace destination {} already exists; a trace is never overwritten",
            path.display()
        );
    }
    Ok(())
}

fn save_trace(trace: &SharedTrace, record: Option<&Path>) -> Result<()> {
    match record {
        Some(path) => trace.lock().unwrap_or_else(|e| e.into_inner()).save(path),
        None => Ok(()),
    }
}

pub(super) fn wait_ms(value: &str) -> std::result::Result<u64, String> {
    let wait = value
        .parse::<u64>()
        .map_err(|_| "wait must be 100..=30000 milliseconds".to_string())?;
    if !(MIN_WAIT_MS..=MAX_WAIT_MS).contains(&wait) {
        return Err("wait must be 100..=30000 milliseconds".into());
    }
    Ok(wait)
}

pub(super) fn load_plugin(dir: &Path) -> Result<(LoadedPlugin, String)> {
    let manifest = dir.join("plugin.toml");
    let mut bytes = Vec::new();
    thegn_core::fsperm::open_regular_file_nofollow(&manifest)
        .with_context(|| format!("could not open plugin manifest {}", manifest.display()))?
        .take(thegn_svc::plugin::loader::MAX_MANIFEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > thegn_svc::plugin::loader::MAX_MANIFEST_BYTES {
        bail!(
            "plugin.toml exceeds {} bytes",
            thegn_svc::plugin::loader::MAX_MANIFEST_BYTES
        );
    }

    let stage = tempfile::tempdir()?;
    let staged_plugin = stage.path().join("plugins/preview");
    std::fs::create_dir_all(&staged_plugin)?;
    std::fs::write(staged_plugin.join("plugin.toml"), &bytes)?;
    let (mut loaded, problems) =
        thegn_svc::plugin::discover(&thegn_core::config::Config::default(), stage.path());
    if !problems.is_empty() || loaded.len() != 1 {
        let detail = problems
            .iter()
            .map(|problem| format!("{}: {}", problem.plugin, problem.problem))
            .collect::<Vec<_>>()
            .join("; ");
        if detail.is_empty() {
            bail!(
                "plugin preview expected one discovered plugin, found {}",
                loaded.len()
            );
        }
        bail!("plugin preview manifest rejected: {detail}");
    }
    let mut plugin = loaded.remove(0);
    plugin.dir = Some(std::fs::canonicalize(dir)?);
    let problems = thegn_svc::plugin::check_spec(&plugin);
    if !problems.is_empty() {
        let detail = problems
            .iter()
            .map(|problem| format!("{}: {}", problem.plugin, problem.problem))
            .collect::<Vec<_>>()
            .join("; ");
        bail!("plugin preview manifest rejected: {detail}");
    }
    if plugin.spec.mode != PluginMode::Resident {
        bail!("plugin preview needs a resident plugin (mode = \"resident\")");
    }
    let negotiated = thegn_svc::plugin::negotiate(&plugin.spec).map_err(anyhow::Error::msg)?;
    let surface = negotiated
        .accepted_contributions
        .iter()
        .find(|contribution| contribution.extension_point == ExtensionPoint::AppTab)
        .and_then(|contribution| contribution.surface.as_ref())
        .map(|surface| surface.as_str().to_string())
        .ok_or_else(|| {
            anyhow::anyhow!("plugin has no accepted AppTab contribution with a surface")
        })?;
    Ok((plugin, surface))
}

struct Collected {
    view: AppView,
    rejected_views: u64,
    junk_lines: u64,
}

fn collect_first_view(
    rx: &Receiver<SessionEvent>,
    writer: &SessionWriter,
    surface: &str,
    deadline: Instant,
    wait_ms: u64,
    trace: &SharedTrace,
    started: Instant,
) -> Result<Collected> {
    let mut rejected_views = 0_u64;
    let mut junk_lines = 0_u64;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(SessionEvent::Message(message)) if message.method == "update" => {
                if message.params.get("surface").and_then(Value::as_str) != Some(surface) {
                    continue;
                }
                let Some(app) = message.params.get("app") else {
                    rejected_views += 1;
                    continue;
                };
                let Ok(view) = serde_json::from_value::<AppView>(app.clone()) else {
                    rejected_views += 1;
                    continue;
                };
                if view.validate().is_err() {
                    rejected_views += 1;
                    continue;
                }
                return Ok(Collected {
                    view,
                    rejected_views,
                    junk_lines,
                });
            }
            Ok(SessionEvent::Message(message)) => {
                if let Some(id) = message.id {
                    let response = RpcResponse::err(
                        id,
                        RpcError::new(RpcErrorCode::Denied, "plugin preview grants no host calls"),
                    );
                    match writer.respond(&response) {
                        Ok(()) => capture_outgoing(trace, started, "response", Some(id), true),
                        Err(error) => {
                            tracing::debug!(target: "thegn::plugin::preview", %error, "denial not delivered");
                        }
                    }
                }
            }
            Ok(SessionEvent::Response(_)) => {}
            Ok(SessionEvent::Junk(_)) => junk_lines += 1,
            Ok(SessionEvent::Exit { code }) => {
                bail!("plugin exited before its first app view (exit code {code:?})");
            }
            Err(RecvTimeoutError::Timeout) => {
                let rejected = if rejected_views == 0 {
                    String::new()
                } else {
                    format!(" ({rejected_views} invalid view(s) rejected)")
                };
                bail!("no valid app view within {wait_ms} ms{rejected}");
            }
            Err(RecvTimeoutError::Disconnected) => {
                bail!("plugin session ended before its first app view");
            }
        }
    }
}

fn preview_value_traced(
    dir: &Path,
    size: (u16, u16),
    wait: Duration,
    trace: &SharedTrace,
    started: Instant,
) -> Result<Value> {
    let (plugin, surface) = load_plugin(dir)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("thegn-plugpre")
        .enable_all()
        .build()?;
    let supervisor = ResidentSupervisor::new(runtime.handle().clone());
    let (tx, rx) = mpsc::channel::<SessionEvent>();
    let callback_trace = trace.clone();
    let session = ResidentSession::spawn(
        &supervisor,
        "preview",
        &plugin.spec.command,
        &BTreeMap::new(),
        plugin.effective_cwd().as_deref(),
        move |event| {
            capture_event(&callback_trace, started, &event);
            if tx.send(event).is_err() {
                // The preview has finished receiving events.
            }
        },
    )
    .map_err(anyhow::Error::msg)?;
    let writer = session.writer();
    let result = (|| {
        let granted = if plugin
            .spec
            .manifest
            .capabilities
            .iter()
            .any(|capability| capability.as_str() == "surface:app")
        {
            vec!["surface:app"]
        } else {
            Vec::new()
        };
        writer.notify_control(
            PluginCallback::Activate,
            json!({
                "api": API_VERSION,
                "session": writer.session_token(),
                "granted": granted,
                "background": false
            }),
        )?;
        capture_outgoing(trace, started, "activate", None, false);
        let wait_ms = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX);
        let collected = collect_first_view(
            &rx,
            &writer,
            &surface,
            Instant::now() + wait,
            wait_ms,
            trace,
            started,
        )?;
        let mut tile = super::preview::tile_for_view(collected.view.clone(), size)?;
        let mut value = super::preview::snapshot_value(tile.as_mut(), size);
        value["live"] = json!({
            "plugin": plugin.spec.manifest.id.as_str(),
            "surface": surface,
            "revision": collected.view.revision,
            "rejected_views": collected.rejected_views,
            "junk_lines": collected.junk_lines
        });
        Ok(value)
    })();

    capture_outgoing(trace, started, "deactivate", None, false);
    let close = close_session(runtime, supervisor, session, &writer);
    result.and_then(|value| close.map(|()| value))
}

#[cfg(test)]
fn preview_value(dir: &Path, size: (u16, u16), wait: Duration) -> Result<Value> {
    preview_value_traced(
        dir,
        size,
        wait,
        &Arc::new(Mutex::new(Trace::default())),
        Instant::now(),
    )
}

pub(super) fn close_session(
    runtime: tokio::runtime::Runtime,
    supervisor: ResidentSupervisor,
    session: ResidentSession,
    writer: &SessionWriter,
) -> Result<()> {
    if let Err(error) = writer.notify_control(PluginCallback::Deactivate, json!({})) {
        tracing::debug!(target: "thegn::plugin::preview", %error, "deactivate not delivered");
    }
    session.kill();
    let report =
        runtime.block_on(supervisor.shutdown_until(tokio::time::Instant::now() + CLOSE_WAIT));
    let clean = report.is_settled();
    if !clean {
        tracing::warn!(target: "thegn::plugin::preview", unresolved = ?report.unresolved, "plugin preview left an unsettled resident session");
    }
    runtime.shutdown_timeout(Duration::from_secs(2));
    if clean {
        Ok(())
    } else {
        bail!("plugin did not shut down cleanly within 5 s")
    }
}

struct LiveParts {
    runtime: tokio::runtime::Runtime,
    supervisor: ResidentSupervisor,
    session: ResidentSession,
    writer: SessionWriter,
}

pub(super) fn run_interactive(dir: &Path, _size: (u16, u16), record: Option<&Path>) -> Result<()> {
    precheck_record(record)?;
    let trace = Arc::new(Mutex::new(if record.is_some() {
        Trace::new(false)
    } else {
        Trace::default()
    }));
    let started = Instant::now();
    let (plugin, surface) = load_plugin(dir)?;
    let mut parts = None;
    let result = tg_kit::standalone::run(|hook| {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("thegn-plugpre")
            .enable_all()
            .build()?;
        let supervisor = ResidentSupervisor::new(runtime.handle().clone());
        let inbox = std::sync::Arc::new(std::sync::Mutex::new(super::live_tile::Inbox::default()));
        let inbox_callback = inbox.clone();
        let callback_surface = surface.clone();
        let callback_hook: ChangeHook = hook.clone();
        let callback_trace = trace.clone();
        let session = ResidentSession::spawn(
            &supervisor,
            "preview",
            &plugin.spec.command,
            &BTreeMap::new(),
            plugin.effective_cwd().as_deref(),
            move |event| {
                capture_event(&callback_trace, started, &event);
                super::live_tile::on_session_event(
                    &inbox_callback,
                    &callback_surface,
                    &callback_hook,
                    event,
                );
            },
        )
        .map_err(anyhow::Error::msg)?;
        let writer = session.writer();
        parts = Some(LiveParts {
            runtime,
            supervisor,
            session,
            writer: writer.clone(),
        });
        let granted = if plugin
            .spec
            .manifest
            .capabilities
            .iter()
            .any(|capability| capability.as_str() == "surface:app")
        {
            vec!["surface:app"]
        } else {
            Vec::new()
        };
        writer.notify_control(
            PluginCallback::Activate,
            json!({
                "api": API_VERSION,
                "session": writer.session_token(),
                "granted": granted,
                "background": false
            }),
        )?;
        capture_outgoing(&trace, started, "activate", None, false);
        Ok(Box::new(super::live_tile::LiveTile::new(
            surface,
            writer,
            inbox,
            super::live_tile::REQUEST_ACK_TIMEOUT,
        )) as Box<dyn AppTile>)
    });
    let result = if let Some(parts) = parts {
        capture_outgoing(&trace, started, "deactivate", None, false);
        let close = close_session(
            parts.runtime,
            parts.supervisor,
            parts.session,
            &parts.writer,
        );
        result.and(close)
    } else {
        result
    };
    let saved = save_trace(&trace, record);
    match (result, saved) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(save_error)) => {
            tracing::warn!(target: "thegn::plugin::preview", error = %save_error, "trace not written");
            Err(error)
        }
    }
}

pub(super) fn run_headless(
    dir: &Path,
    size: (u16, u16),
    wait: Duration,
    record: Option<&Path>,
) -> Result<()> {
    precheck_record(record)?;
    let trace = Arc::new(Mutex::new(if record.is_some() {
        Trace::new(false)
    } else {
        Trace::default()
    }));
    let started = Instant::now();
    let value = preview_value_traced(dir, size, wait, &trace, started);
    let saved = save_trace(&trace, record);
    match (value, saved) {
        (Ok(value), Ok(())) => crate::cmd::emit_json(&value),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(save_error)) => {
            tracing::warn!(target: "thegn::plugin::preview", error = %save_error, "trace not written");
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::path::PathBuf;

    #[allow(
        clippy::disallowed_macros,
        reason = "report when the optional Python runtime is unavailable"
    )]
    #[expect(
        clippy::disallowed_methods,
        reason = "probe whether the optional Python runtime is installed"
    )]
    fn python_available() -> bool {
        let result = std::process::Command::new("python3")
            .arg("--version")
            .output();
        if result.is_err() {
            eprintln!("skipping: python3 is not installed");
            false
        } else {
            true
        }
    }

    fn fault_script() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins/fixtures/fault.py")
    }

    fn staged_plugin(
        mode: &str,
        script: &Path,
        args: &[&str],
        surface: &str,
        extension: &str,
    ) -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        let mut command = vec!["python3".to_string(), script.display().to_string()];
        command.extend(args.iter().map(|arg| (*arg).to_string()));
        let capabilities = if extension == "StatusBarSegment" {
            "[\"surface:app\", \"surface:statusbar\"]"
        } else {
            "[\"surface:app\"]"
        };
        let manifest = format!(
            "id = \"livetest\"\nname = \"Live test\"\nversion = \"0.1.0\"\napi = \"0.4.0\"\nmode = {mode:?}\ncapabilities = {capabilities}\ncommand = {}\n\n[[contributions]]\nid = \"workspace\"\nextension_point = \"{extension}\"\nlabel = \"Live\"\nsurface = \"{surface}\"\n",
            serde_json::to_value(&command).unwrap(),
        );
        std::fs::write(temp.path().join("plugin.toml"), manifest).unwrap();
        temp
    }

    fn fault_plugin(mode: &str, surface: &str) -> tempfile::TempDir {
        staged_plugin(
            "resident",
            &fault_script(),
            &["--mode", mode],
            surface,
            "AppTab",
        )
    }

    fn assert_line(value: &Value, text: &str) {
        assert!(
            value["lines"]
                .as_array()
                .unwrap()
                .iter()
                .any(|line| { line.as_str().is_some_and(|line| line.contains(text)) })
        );
    }

    #[test]
    fn live_preview_renders_the_first_view_of_a_resident_plugin() {
        if !python_available() {
            return;
        }
        let plugin = fault_plugin("normal", "fixture.workspace");
        let value = preview_value(plugin.path(), (60, 8), Duration::from_secs(10)).unwrap();
        assert_eq!(value["live"]["revision"], 1);
        assert_eq!(value["live"]["surface"], "fixture.workspace");
        assert_eq!(value["live"]["rejected_views"], 0);
        assert_eq!(value["lines"].as_array().unwrap().len(), 8);
        assert_eq!(value["cols"], 60);
        assert_line(&value, "Ready");
    }

    #[test]
    fn live_preview_ignores_junk_lines_and_still_renders() {
        if !python_available() {
            return;
        }
        let plugin = fault_plugin("malformed", "fixture.workspace");
        let value = preview_value(plugin.path(), (60, 8), Duration::from_secs(10)).unwrap();
        assert!(value["live"]["junk_lines"].as_u64().unwrap() <= 1);
        assert_line(&value, "Ready");
    }

    #[test]
    fn live_preview_counts_rejected_views_and_keeps_waiting() {
        if !python_available() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("bad_then_good.py");
        std::fs::write(
            &script,
            r#"import json, sys
def update(revision, widget_id, text):
    print(json.dumps({"method":"update","params":{"surface":"livetest.workspace","app":{"state_scope":"fixture","revision":revision,"status":"Live","root":{"kind":"text","id":widget_id,"title":"Live","text":text}}}}), flush=True)
update(1, "", "Rejected")
update(2, "text", "Recovered")
for line in sys.stdin:
    if json.loads(line).get("method") == "deactivate":
        break
"#,
        )
        .unwrap();
        let plugin = staged_plugin("resident", &script, &[], "livetest.workspace", "AppTab");
        let value = preview_value(plugin.path(), (60, 8), Duration::from_secs(10)).unwrap();
        assert_eq!(value["live"]["rejected_views"], 1);
        assert_eq!(value["live"]["revision"], 2);
        assert_line(&value, "Recovered");
    }

    #[test]
    fn live_preview_timeout_reports_rejected_view_count() {
        if !python_available() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("invalid_only.py");
        std::fs::write(
            &script,
            r#"import json, sys
print(json.dumps({"method":"update","params":{"surface":"livetest.workspace","app":{"state_scope":"fixture","revision":1,"status":"Live","root":{"kind":"text","id":"","title":"Live","text":"Rejected"}}}}), flush=True)
for line in sys.stdin:
    if json.loads(line).get("method") == "deactivate":
        break
"#,
        )
        .unwrap();
        let plugin = staged_plugin("resident", &script, &[], "livetest.workspace", "AppTab");
        let error = preview_value(plugin.path(), (60, 8), Duration::from_millis(300)).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("no valid app view within 300 ms (1 invalid view(s) rejected)")
        );
    }

    #[test]
    fn live_preview_reports_a_crash_before_the_first_view() {
        if !python_available() {
            return;
        }
        let plugin = fault_plugin("crash", "fixture.workspace");
        let error = preview_value(plugin.path(), (60, 8), Duration::from_secs(10)).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("exited before its first app view")
        );
        assert!(error.to_string().contains("Some(3)"));
    }

    #[test]
    fn live_preview_times_out_when_no_view_arrives() {
        if !python_available() {
            return;
        }
        let plugin = fault_plugin("no-read", "fixture.workspace");
        let started = Instant::now();
        let error = preview_value(plugin.path(), (60, 8), Duration::from_millis(300)).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("no valid app view within 300 ms")
        );
        assert!(started.elapsed() < Duration::from_secs(20));
    }

    #[test]
    fn live_preview_answers_host_calls_with_denied_before_the_first_view() {
        if !python_available() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("calls.py");
        std::fs::write(
            &script,
            r#"import json, sys
print(json.dumps({"id":7,"method":"host.call","params":{"capability":"sessions.list","params":{}}}), flush=True)
for line in sys.stdin:
    message = json.loads(line)
    if message.get("id") == 7:
        denied = message.get("error", {}).get("code") == "denied"
        text = "denied" if denied else "allowed"
        print(json.dumps({"method":"update","params":{"surface":"livetest.workspace","app":{"state_scope":"fixture","revision":1,"status":"Live","root":{"kind":"text","id":"text","title":"Live","text":text}}}}), flush=True)
    if message.get("method") == "deactivate":
        break
"#,
        )
        .unwrap();
        let plugin = staged_plugin("resident", &script, &[], "livetest.workspace", "AppTab");
        let value = preview_value(plugin.path(), (60, 8), Duration::from_secs(10)).unwrap();
        assert_line(&value, "denied");
    }

    #[test]
    fn live_preview_grants_only_surface_app_in_activate() {
        if !python_available() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("echo_activate.py");
        std::fs::write(
            &script,
            r#"import json, sys
for line in sys.stdin:
    message = json.loads(line)
    if message.get("method") == "activate":
        params = message["params"]
        text = ",".join(params["granted"]) + "|" + str(params["background"])
        print(json.dumps({"method":"update","params":{"surface":"livetest.workspace","app":{"state_scope":"fixture","revision":1,"status":"Live","root":{"kind":"text","id":"text","title":"Live","text":text}}}}), flush=True)
    if message.get("method") == "deactivate":
        break
"#,
        )
        .unwrap();
        let plugin = staged_plugin("resident", &script, &[], "livetest.workspace", "AppTab");
        let value = preview_value(plugin.path(), (60, 8), Duration::from_secs(10)).unwrap();
        assert_line(&value, "surface:app|False");
        assert!(
            !value["lines"]
                .as_array()
                .unwrap()
                .iter()
                .any(|line| { line.as_str().is_some_and(|line| line.contains("host:")) })
        );
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "use Python to create a portable symlink fixture when available"
    )]
    fn load_plugin_refuses_non_resident_missing_app_tab_oversized_and_symlinked_manifests() {
        if !python_available() {
            return;
        }

        let non_resident = staged_plugin(
            "one_shot",
            &fault_script(),
            &[],
            "workspace",
            "StatusBarSegment",
        );
        let error = load_plugin(non_resident.path()).unwrap_err();
        assert!(error.to_string().contains("resident"));

        let no_app_tab = staged_plugin(
            "resident",
            &fault_script(),
            &[],
            "workspace",
            "StatusBarSegment",
        );
        let error = load_plugin(no_app_tab.path()).unwrap_err();
        assert!(error.to_string().contains("AppTab"));

        let oversized = tempfile::tempdir().unwrap();
        std::fs::write(oversized.path().join("plugin.toml"), "# ".repeat(35_000)).unwrap();
        let error = load_plugin(oversized.path()).unwrap_err();
        assert!(error.to_string().contains("exceeds"));

        if python_available() {
            let valid = fault_plugin("normal", "fixture.workspace");
            let linked = tempfile::tempdir().unwrap();
            let source = valid.path().join("plugin.toml");
            let link = linked.path().join("plugin.toml");
            let output = std::process::Command::new("python3")
                .arg("-c")
                .arg("import os, sys; os.symlink(sys.argv[1], sys.argv[2])")
                .arg(source)
                .arg(&link)
                .output()
                .expect("python3 symlink fixture starts");
            if output.status.success() {
                let error = load_plugin(linked.path()).unwrap_err();
                assert!(error.to_string().contains("plugin.toml"));
            }
        }
    }

    #[test]
    fn wait_ms_is_bounded() {
        assert_eq!(wait_ms("100"), Ok(100));
        assert_eq!(wait_ms("30000"), Ok(30000));
        assert!(wait_ms("99").is_err());
        assert!(wait_ms("30001").is_err());
        assert!(wait_ms("abc").is_err());
    }

    #[test]
    fn live_trace_headless_records_metadata_only_in_a_private_file() {
        if !python_available() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("calls.py");
        let token_path = temp.path().join("session-token");
        std::fs::write(
            &script,
            r#"import json, sys
for line in sys.stdin:
    message = json.loads(line)
    if message.get("method") == "activate":
        with open(sys.argv[1], "w") as token_file:
            token_file.write(message["params"]["session"])
    if message.get("id") == 7:
        denied = message.get("error", {}).get("code") == "denied"
        text = "denied" if denied else "allowed"
        print(json.dumps({"method":"update","params":{"surface":"livetest.workspace","app":{"state_scope":"fixture","revision":1,"status":"Live","root":{"kind":"text","id":"text","title":"Live","text":text}}}}), flush=True)
    if message.get("method") == "deactivate":
        break
"#,
        )
        .unwrap();
        let token_path_arg = token_path.display().to_string();
        let plugin = staged_plugin(
            "resident",
            &script,
            &[token_path_arg.as_str()],
            "livetest.workspace",
            "AppTab",
        );
        let trace = Arc::new(Mutex::new(Trace::new(false)));
        let value = preview_value_traced(
            plugin.path(),
            (60, 8),
            Duration::from_secs(10),
            &trace,
            Instant::now(),
        )
        .unwrap();
        assert_line(&value, "denied");

        let path = temp.path().join("live.jsonl");
        save_trace(&trace, Some(&path)).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let token = std::fs::read_to_string(&token_path).unwrap();
        assert!(!token.is_empty());
        assert!(!text.contains("denied"));
        assert!(!text.contains("sessions.list"));
        assert!(!text.contains("capability"));
        assert!(!text.contains("Live"));
        assert!(!text.contains(&token));

        let rows = text
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert!(rows.iter().all(|row| {
            row["direction"].is_string()
                && row["frame"]["method"].is_string()
                && row["frame"].get("id").is_some()
                && row["frame"]["error"].is_boolean()
                && row["redacted"].as_bool().unwrap_or(false)
                && row["frame"]["payload"] == "redacted"
        }));
        let observed = rows
            .iter()
            .map(|row| {
                (
                    row["direction"].as_str().unwrap(),
                    row["frame"]["method"].as_str().unwrap(),
                    row["frame"]["id"].clone(),
                    row["frame"]["error"].as_bool().unwrap_or(false),
                )
            })
            .collect::<Vec<_>>();
        assert!(observed.contains(&("incoming", "host.call", json!(7), false)));
        assert!(observed.contains(&("incoming", "update", Value::Null, false)));
        assert!(observed.contains(&("outgoing", "activate", Value::Null, false)));
        assert!(observed.contains(&("outgoing", "response", json!(7), true)));
        assert!(observed.contains(&("outgoing", "deactivate", Value::Null, false)));
        assert!(save_trace(&trace, Some(&path)).is_err());
    }

    #[test]
    fn live_trace_refuses_an_existing_destination_before_any_spawn() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("existing.jsonl");
        let contents = b"keep this trace";
        std::fs::write(&path, contents).unwrap();

        let error = run_headless(
            Path::new("/nonexistent-plugin-dir"),
            (60, 8),
            Duration::from_millis(100),
            Some(&path),
        )
        .unwrap_err();
        assert!(error.to_string().contains("already exists"));
        assert_eq!(std::fs::read(path).unwrap(), contents);
    }

    #[test]
    fn live_trace_headless_saves_a_trace_when_the_plugin_crashes() {
        if !python_available() {
            return;
        }
        let plugin = fault_plugin("crash", "fixture.workspace");
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("crash.jsonl");
        let error =
            run_headless(plugin.path(), (60, 8), Duration::from_secs(10), Some(&path)).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("exited before its first app view")
        );
        let text = std::fs::read_to_string(path).unwrap();
        assert!(text.lines().any(|line| {
            serde_json::from_str::<Value>(line)
                .is_ok_and(|record| record["frame"]["method"] == "exit")
        }));
    }

    #[test]
    fn live_trace_event_frame_drops_params_junk_text_and_results() {
        let message = event_frame(&SessionEvent::Message(thegn_core::plugin_api::RpcMessage {
            id: Some(9),
            method: "host.call".into(),
            params: json!({"secret":"params-value"}),
        }));
        let success = event_frame(&SessionEvent::Response(RpcResponse::ok(
            10,
            json!({"secret":"result-value"}),
        )));
        let failure = event_frame(&SessionEvent::Response(RpcResponse::err(
            11,
            RpcError::new(RpcErrorCode::Denied, "private error text"),
        )));
        let junk = event_frame(&SessionEvent::Junk("token-value".into()));
        let exit = event_frame(&SessionEvent::Exit { code: None });
        let encoded = format!("{message} {success} {failure} {junk} {exit}");

        assert!(!encoded.contains("params"));
        assert!(!encoded.contains("result"));
        assert!(!encoded.contains("token-value"));
        assert!(!encoded.contains("private error text"));
        assert_eq!(message["method"], "host.call");
        assert_eq!(success.get("error"), None);
        assert!(failure["error"].as_bool().unwrap_or(false));
        assert_eq!(message.get("error"), None);
        assert_eq!(junk.get("error"), None);
        assert_eq!(exit.get("error"), None);
        assert_eq!(junk, json!({"method":"junk"}));
        assert_eq!(exit, json!({"method":"exit"}));
    }

    #[test]
    fn live_trace_capture_stops_at_the_record_bound_and_keeps_the_prefix() {
        let trace = Arc::new(Mutex::new(Trace::new(false)));
        let started = Instant::now();
        let event = SessionEvent::Exit { code: None };
        for _ in 0..20_100 {
            capture_event(&trace, started, &event);
        }
        assert_eq!(
            trace
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .record_count(),
            20_000
        );
    }
}
