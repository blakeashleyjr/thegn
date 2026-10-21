//! CI provider adapter backed by a resident plugin's `provider.call` seam.

use std::collections::VecDeque;
use std::sync::{Arc, mpsc};

use thegn_core::ci::{CiCaps, CiError, CiJob, CiLog, CiRun, CiSystem, CiWorkflow, RerunScope};
use thegn_core::plugin_api::RpcErrorCode;
use thegn_core::remote::GitLoc;
use thegn_core::seam::{Availability, Probe, ProbeReport};

use crate::ci::{
    CiProvider, CiRunDetail, CiRunList, MAX_PROVIDER_FIELD_BYTES, PROVIDER_LOG_MAX_BYTES,
    PROVIDER_LOG_MAX_LINES, validate_branch_ref, validate_ci_id, validate_workflow_selector,
};

use super::provider::{BridgeError, ProviderBridge};

/// A synchronous CI seam adapter for a resident plugin.
pub struct PluginCiProvider {
    bridge: Arc<ProviderBridge>,
    runtime: tokio::runtime::Handle,
    provider_id: Arc<str>,
    caps: CiCaps,
}

impl PluginCiProvider {
    /// Bind a bridge to a validated plugin identity and its negotiated caps.
    pub fn new(
        bridge: Arc<ProviderBridge>,
        runtime: tokio::runtime::Handle,
        plugin_id: &str,
        caps: CiCaps,
    ) -> Result<Self, CiError> {
        crate::issue::identity::plugin_namespace(plugin_id).map_err(CiError::Other)?;
        Ok(Self {
            bridge,
            runtime,
            provider_id: Arc::from(format!("plugin:{plugin_id}")),
            caps,
        })
    }

    /// The stable seam provider id (`plugin:<plugin-id>`).
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    /// Callers must use a blocking thread. The future runs on the resident
    /// runtime, while this synchronous seam waits without blocking a runtime
    /// worker (the daemon routes invoke providers from `spawn_blocking`).
    fn call(
        &self,
        op: &'static str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, CiError> {
        let bridge = self.bridge.clone();
        let (sender, receiver) = mpsc::channel();
        self.runtime.spawn(async move {
            let result = bridge.call("ci", op, args).await;
            if let Err(error) = sender.send(result) {
                tracing::debug!(%error, "plugin CI caller dropped the reply receiver");
            }
        });
        receiver
            .recv()
            .map_err(|error| CiError::Other(format!("plugin transport: {error}")))?
            .map_err(|error| match error {
                BridgeError::Rpc(rpc) => match rpc.code {
                    RpcErrorCode::Unsupported => unsupported(op),
                    RpcErrorCode::NotFound => CiError::NotFound,
                    RpcErrorCode::Auth | RpcErrorCode::Denied => CiError::NotAuthenticated,
                    RpcErrorCode::RateLimited => CiError::RateLimited,
                    RpcErrorCode::Timeout => CiError::Offline,
                    RpcErrorCode::Invalid | RpcErrorCode::Other => {
                        CiError::Other(format!("plugin answered {:?}: {}", rpc.code, rpc.message))
                    }
                },
                BridgeError::Transport(message) => {
                    CiError::Other(format!("plugin transport: {message}"))
                }
            })
    }

    fn loc_args(loc: &GitLoc) -> serde_json::Value {
        serde_json::json!({ "worktree": loc.path(), "remote": loc.is_remote() })
    }

    fn require_cap(enabled: bool, op: &'static str) -> Result<(), CiError> {
        if enabled {
            Ok(())
        } else {
            Err(unsupported(op))
        }
    }
}

fn unsupported(op: &'static str) -> CiError {
    CiError::Other(format!("plugin CI provider does not support {op}"))
}

fn fields_fit(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(text) => text.len() <= MAX_PROVIDER_FIELD_BYTES,
        serde_json::Value::Array(items) => items.iter().all(fields_fit),
        serde_json::Value::Object(fields) => fields.values().all(fields_fit),
        _ => true,
    }
}

fn valid_run(value: &serde_json::Value) -> Option<CiRun> {
    if !fields_fit(value) {
        return None;
    }
    let run: CiRun = match serde_json::from_value(value.clone()) {
        Ok(run) => run,
        Err(_) => return None,
    };
    if validate_ci_id(&run.id).is_err() {
        return None;
    }
    Some(run)
}

fn valid_job(job: &CiJob) -> bool {
    validate_ci_id(&job.id).is_ok()
        && job.name.len() <= MAX_PROVIDER_FIELD_BYTES
        && job
            .url
            .as_ref()
            .is_none_or(|value| value.len() <= MAX_PROVIDER_FIELD_BYTES)
        && job
            .started_at
            .as_ref()
            .is_none_or(|value| value.len() <= MAX_PROVIDER_FIELD_BYTES)
        && job
            .finished_at
            .as_ref()
            .is_none_or(|value| value.len() <= MAX_PROVIDER_FIELD_BYTES)
        && job.steps.iter().all(|step| {
            step.name.len() <= MAX_PROVIDER_FIELD_BYTES
                && step
                    .started_at
                    .as_ref()
                    .is_none_or(|value| value.len() <= MAX_PROVIDER_FIELD_BYTES)
                && step
                    .finished_at
                    .as_ref()
                    .is_none_or(|value| value.len() <= MAX_PROVIDER_FIELD_BYTES)
        })
}

fn parse_runs(value: serde_json::Value, limit: usize) -> Result<CiRunList, CiError> {
    let rows = value
        .as_array()
        .ok_or_else(|| CiError::Other("plugin runs reply is not an array".into()))?;
    let cap = limit.max(1);
    let mut runs = Vec::with_capacity(rows.len().min(cap));
    let mut discarded_rows = 0;
    for row in rows {
        if runs.len() >= cap {
            discarded_rows += 1;
            continue;
        }
        match valid_run(row) {
            Some(mut run) => {
                run.jobs.clear();
                runs.push(run);
            }
            None => discarded_rows += 1,
        }
    }
    Ok(CiRunList {
        runs,
        discarded_rows,
    })
}

fn parse_detail(value: serde_json::Value, requested_id: &str) -> Result<CiRunDetail, CiError> {
    let mut object = value
        .as_object()
        .cloned()
        .ok_or_else(|| CiError::Other("plugin run_detail reply is not an object".into()))?;
    let jobs = object
        .remove("jobs")
        .unwrap_or_else(|| serde_json::json!([]));
    if !fields_fit(&serde_json::Value::Object(object.clone())) {
        return Err(CiError::Other(
            "plugin run_detail reply contains an oversize field".into(),
        ));
    }
    let mut run: CiRun = serde_json::from_value(serde_json::Value::Object(object))
        .map_err(|error| CiError::Other(format!("malformed plugin run_detail reply: {error}")))?;
    validate_ci_id(&run.id)?;
    if run.id != requested_id {
        return Err(CiError::Other(
            "plugin run_detail reply names a different run".into(),
        ));
    }
    let rows = jobs
        .as_array()
        .ok_or_else(|| CiError::Other("plugin run_detail jobs are not an array".into()))?;
    let mut kept = Vec::with_capacity(rows.len());
    let mut discarded_jobs = 0;
    for row in rows {
        let parsed = serde_json::from_value::<CiJob>(row.clone());
        match parsed {
            Ok(job) if fields_fit(row) && valid_job(&job) => kept.push(job),
            _ => discarded_jobs += 1,
        }
    }
    run.jobs = kept;
    Ok(CiRunDetail {
        run,
        discarded_jobs,
    })
}

fn bounded_log(value: serde_json::Value) -> Result<CiLog, CiError> {
    let mut log: CiLog = serde_json::from_value(value)
        .map_err(|error| CiError::Other(format!("malformed plugin logs reply: {error}")))?;
    let mut in_pem = false;
    let mut lines = VecDeque::new();
    let mut kept_bytes = 0usize;
    let mut truncated = log.truncated;
    for line in log.text.split_inclusive('\n') {
        let redacted = thegn_core::ci_log::redact_stream_line(line.as_bytes(), &mut in_pem);
        if redacted.len() > PROVIDER_LOG_MAX_BYTES {
            truncated = true;
            continue;
        }
        kept_bytes += redacted.len();
        lines.push_back(redacted);
        while lines.len() > PROVIDER_LOG_MAX_LINES || kept_bytes > PROVIDER_LOG_MAX_BYTES {
            if let Some(old) = lines.pop_front() {
                kept_bytes -= old.len();
                truncated = true;
            }
        }
    }
    log.text = lines.into_iter().collect();
    log.truncated = truncated;
    Ok(log)
}

fn mutation_result(op: &'static str, value: serde_json::Value) -> Result<(), CiError> {
    if value.is_null() {
        Err(CiError::Other(format!(
            "plugin {op} reply has no success result"
        )))
    } else {
        Ok(())
    }
}

impl Probe for PluginCiProvider {
    fn probe(&self) -> ProbeReport {
        ProbeReport::new("ci", self.provider_id(), Availability::Ready)
            .with_caps(&self.caps)
            .note("resident plugin CI provider (not selectable by [ci] provider yet)")
    }
}

impl CiProvider for PluginCiProvider {
    fn system(&self) -> CiSystem {
        CiSystem::Plugin
    }
    fn caps(&self) -> CiCaps {
        self.caps
    }

    fn runs(&self, loc: &GitLoc, branch: Option<&str>, limit: usize) -> Result<CiRunList, CiError> {
        if let Some(branch) = branch {
            validate_branch_ref(branch)?;
        }
        let mut args = Self::loc_args(loc);
        args["branch"] = branch.map_or(serde_json::Value::Null, |value| serde_json::json!(value));
        args["limit"] = serde_json::json!(limit);
        parse_runs(self.call("runs", args)?, limit)
    }

    fn run_detail(&self, loc: &GitLoc, run_id: &str) -> Result<CiRunDetail, CiError> {
        validate_ci_id(run_id)?;
        let mut args = Self::loc_args(loc);
        args["run_id"] = serde_json::json!(run_id);
        parse_detail(self.call("run_detail", args)?, run_id)
    }

    fn logs(&self, loc: &GitLoc, run_id: &str, job_id: &str) -> Result<CiLog, CiError> {
        Self::require_cap(self.caps.logs, "logs")?;
        validate_ci_id(run_id)?;
        validate_ci_id(job_id)?;
        let mut args = Self::loc_args(loc);
        args["run_id"] = serde_json::json!(run_id);
        args["job_id"] = serde_json::json!(job_id);
        bounded_log(self.call("logs", args)?)
    }

    fn workflows(&self, loc: &GitLoc) -> Result<Vec<CiWorkflow>, CiError> {
        let value = self.call("workflows", Self::loc_args(loc))?;
        let rows = value
            .as_array()
            .ok_or_else(|| CiError::Other("plugin workflows reply is not an array".into()))?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                if !fields_fit(row) {
                    return None;
                }
                let workflow = match serde_json::from_value::<CiWorkflow>(row.clone()) {
                    Ok(workflow) => workflow,
                    Err(_) => return None,
                };
                Some(workflow)
            })
            .collect())
    }

    fn trigger(
        &self,
        loc: &GitLoc,
        workflow: &str,
        inputs: &[(String, String)],
    ) -> Result<(), CiError> {
        Self::require_cap(self.caps.trigger, "trigger")?;
        validate_workflow_selector(workflow)?;
        let mut args = Self::loc_args(loc);
        args["workflow"] = serde_json::json!(workflow);
        args["inputs"] = serde_json::json!(
            inputs
                .iter()
                .map(|(name, value)| [name, value])
                .collect::<Vec<_>>()
        );
        mutation_result("trigger", self.call("trigger", args)?)
    }

    fn rerun(&self, loc: &GitLoc, run_id: &str, scope: RerunScope) -> Result<(), CiError> {
        Self::require_cap(self.caps.rerun, "rerun")?;
        validate_ci_id(run_id)?;
        let scope = if scope == RerunScope::All {
            "all"
        } else if scope == RerunScope::Failed {
            Self::require_cap(self.caps.rerun_failed, "rerun")?;
            "failed"
        } else {
            return Err(unsupported("rerun"));
        };
        let mut args = Self::loc_args(loc);
        args["run_id"] = serde_json::json!(run_id);
        args["scope"] = serde_json::json!(scope);
        mutation_result("rerun", self.call("rerun", args)?)
    }

    fn cancel(&self, loc: &GitLoc, run_id: &str) -> Result<(), CiError> {
        Self::require_cap(self.caps.cancel, "cancel")?;
        validate_ci_id(run_id)?;
        let mut args = Self::loc_args(loc);
        args["run_id"] = serde_json::json!(run_id);
        mutation_result("cancel", self.call("cancel", args)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::provider::ProviderBridge;
    use crate::plugin::session::tests::FixtureSupervisor;
    use crate::plugin::session::{ResidentSession, SessionEvent};
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::time::Duration;

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    fn live_ci_bridge(
        script: &str,
        timeout: Duration,
    ) -> (FixtureSupervisor, ResidentSession, Arc<ProviderBridge>) {
        let fixture = FixtureSupervisor::new();
        let slot: Arc<Mutex<Option<Arc<ProviderBridge>>>> = Arc::new(Mutex::new(None));
        let route = slot.clone();
        let session = fixture
            .spawn(&sh(script), &BTreeMap::new(), None, move |event| {
                if let SessionEvent::Response(response) = event
                    && let Some(bridge) = route
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .as_ref()
                {
                    bridge.resolve(response);
                }
            })
            .unwrap();
        let bridge = ProviderBridge::new(session.writer(), timeout);
        *slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(bridge.clone());
        (fixture, session, bridge)
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap()
    }

    fn adapter(
        bridge: Arc<ProviderBridge>,
        runtime: &tokio::runtime::Runtime,
        caps: CiCaps,
    ) -> PluginCiProvider {
        PluginCiProvider::new(bridge, runtime.handle().clone(), "demo", caps).unwrap()
    }

    fn loc() -> GitLoc {
        GitLoc::Local(std::path::PathBuf::from("/worktree"))
    }

    fn all_caps() -> CiCaps {
        CiCaps {
            logs: true,
            trigger: true,
            rerun: true,
            rerun_failed: true,
            cancel: true,
            ..CiCaps::default()
        }
    }

    const SUCCESS_SCRIPT: &str = r#"
while read -r line; do
  id=${line#*'"id":'}; id=${id%%,*}
  case "$line" in
    *'"op":"runs"'*) printf '{"id":%s,"result":[{"id":"1","name":"build","state":"pass"}]}\n' "$id" ;;
    *'"op":"run_detail"'*) printf '{"id":%s,"result":{"id":"1","name":"build","state":"pass","jobs":[{"id":"2","name":"compile","state":"pass"}]}}\n' "$id" ;;
    *'"op":"logs"'*) printf '{"id":%s,"result":{"text":"ok","truncated":false}}\n' "$id" ;;
    *'"op":"workflows"'*) printf '{"id":%s,"result":[{"id":"ci.yml","name":"CI","dispatchable":true}]}\n' "$id" ;;
    *) printf '{"id":%s,"result":true}\n' "$id" ;;
  esac
done
"#;

    #[test]
    fn ci_ops_round_trip_through_a_scripted_plugin() {
        let rt = runtime();
        let (_fixture, _session, bridge) = live_ci_bridge(SUCCESS_SCRIPT, Duration::from_secs(3));
        let provider = adapter(bridge, &rt, all_caps());
        assert_eq!(
            provider.runs(&loc(), Some("main"), 10).unwrap().runs.len(),
            1
        );
        assert_eq!(provider.run_detail(&loc(), "1").unwrap().run.jobs.len(), 1);
        assert_eq!(provider.logs(&loc(), "1", "2").unwrap().text, "ok");
        assert_eq!(provider.workflows(&loc()).unwrap()[0].name, "CI");
        assert!(
            provider
                .trigger(&loc(), "ci.yml", &[("env".into(), "prod".into())])
                .is_ok()
        );
        assert!(provider.rerun(&loc(), "1", RerunScope::Failed).is_ok());
        assert!(provider.cancel(&loc(), "1").is_ok());
        assert_eq!(provider.system(), CiSystem::Plugin);
        assert_eq!(provider.provider_id(), "plugin:demo");
        let report = provider.probe();
        assert_eq!(report.seam, "ci");
        assert_eq!(report.id, "plugin:demo");
        assert!(report.notes[0].contains("not selectable"));
    }

    #[test]
    fn caps_off_ops_are_refused_without_a_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("ops");
        let script = format!(
            "while read -r line; do id=${{line#*'\"id\":'}}; id=${{id%%,*}}; printf 'called\\n' >> '{}'; printf '{{\"id\":%s,\"result\":true}}\\n' \"$id\"; done",
            marker.display()
        );
        let rt = runtime();
        let (_fixture, _session, bridge) = live_ci_bridge(&script, Duration::from_secs(3));
        let provider = adapter(bridge, &rt, CiCaps::default());
        assert!(
            matches!(provider.logs(&loc(), "1", "2"), Err(CiError::Other(message)) if message.contains("logs"))
        );
        assert!(
            matches!(provider.trigger(&loc(), "ci.yml", &[]), Err(CiError::Other(message)) if message.contains("trigger"))
        );
        assert!(
            matches!(provider.rerun(&loc(), "1", RerunScope::All), Err(CiError::Other(message)) if message.contains("rerun"))
        );
        assert!(
            matches!(provider.cancel(&loc(), "1"), Err(CiError::Other(message)) if message.contains("cancel"))
        );
        assert!(!marker.exists());
    }

    #[test]
    fn malformed_and_oversize_rows_are_discarded_and_counted() {
        let too_long = "x".repeat(MAX_PROVIDER_FIELD_BYTES + 1);
        let rows = serde_json::json!([
            {"id":"1","name":"ok","state":"pass"},
            {"id":"abc","name":"bad","state":"pass"},
            {"id":"0","name":"bad","state":"pass"},
            {"id":"2","name":too_long.clone(),"state":"pass"},
            {"id":"3","name":"second valid","state":"pass"},
            {"id":"4","name":"extra","state":"pass"}
        ]);
        let list = parse_runs(rows, 2).unwrap();
        assert_eq!(list.runs.len(), 2);
        assert_eq!(list.discarded_rows, 4);
        let detail = parse_detail(
            serde_json::json!({
                "id":"1","name":"ok","state":"pass",
                "jobs":[
                    {"id":"2","name":"valid","state":"pass"},
                    {"id":"bad","name":"invalid id","state":"pass"},
                    {"id":"3","name":too_long,"state":"pass"}
                ]
            }),
            "1",
        )
        .unwrap();
        assert_eq!(detail.run.jobs.len(), 1);
        assert_eq!(detail.discarded_jobs, 2);
    }

    #[test]
    fn run_detail_for_a_different_run_is_refused() {
        let reply = serde_json::json!({"id":"2","name":"build","state":"pass","jobs":[]});
        assert!(
            matches!(parse_detail(reply, "1"), Err(CiError::Other(message)) if message == "plugin run_detail reply names a different run")
        );
    }

    #[test]
    fn plugin_logs_are_redacted_and_capped() {
        let pem = "-----BEGIN PRIVATE KEY-----\nsecret\n-----END PRIVATE KEY-----\nAuthorization: Bearer abc";
        let redacted = bounded_log(serde_json::json!({"text":pem,"truncated":false})).unwrap();
        assert!(!redacted.text.contains("secret"));
        assert!(!redacted.text.contains("abc"));
        let open_pem = bounded_log(serde_json::json!({
            "text":"-----BEGIN PRIVATE KEY-----\nsecret\n",
            "truncated":false
        }))
        .unwrap();
        assert!(!open_pem.truncated);
        let text = (0..PROVIDER_LOG_MAX_LINES + 1)
            .map(|line| format!("line{line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let capped = bounded_log(serde_json::json!({"text":text,"truncated":false})).unwrap();
        assert!(capped.truncated);
        assert_eq!(capped.text.lines().count(), PROVIDER_LOG_MAX_LINES);
    }

    #[test]
    fn workflows_with_oversize_nested_fields_are_dropped() {
        let too_long = "x".repeat(MAX_PROVIDER_FIELD_BYTES + 1);
        let reply = serde_json::json!([
            {"id":"valid","name":"CI","dispatchable":true},
            {
                "id":"oversize-input",
                "name":"Deploy",
                "inputs":[{"name":"environment","description":too_long}]
            }
        ]);
        let script = format!(
            "while read -r line; do id=${{line#*'\"id\":'}}; id=${{id%%,*}}; printf '{{\"id\":%s,\"result\":{}}}\\n' \"$id\"; done",
            serde_json::to_string(&reply).unwrap()
        );
        let rt = runtime();
        let (_fixture, _session, bridge) = live_ci_bridge(&script, Duration::from_secs(3));
        let provider = adapter(bridge, &rt, CiCaps::default());

        let workflows = provider.workflows(&loc()).unwrap();
        assert_eq!(workflows.len(), 1);
        assert_eq!(workflows[0].id, "valid");
    }

    #[test]
    fn unsupported_and_transport_errors_are_typed() {
        let rt = runtime();
        let script = r#"while read -r line; do id=${line#*'"id":'}; id=${id%%,*}; case "$line" in *'"op":"runs"'*) printf '{"id":%s,"error":{"code":"unsupported","message":"no"}}\n' "$id";; *'"op":"workflows"'*) printf '{"id":%s,"error":{"code":"not_found","message":"no"}}\n' "$id";; esac; done"#;
        let (_fixture, _session, bridge) = live_ci_bridge(script, Duration::from_millis(200));
        let provider = adapter(bridge, &rt, all_caps());
        assert!(
            matches!(provider.runs(&loc(), None, 5), Err(CiError::Other(message)) if message.contains("does not support runs"))
        );
        assert!(matches!(provider.workflows(&loc()), Err(CiError::NotFound)));
        let (_fixture, _session, bridge) = live_ci_bridge(
            "while read -r line; do sleep 1; done",
            Duration::from_millis(200),
        );
        let provider = adapter(bridge, &rt, CiCaps::default());
        assert!(
            matches!(provider.runs(&loc(), None, 5), Err(CiError::Other(message)) if message.contains("plugin transport"))
        );
        let (_fixture, _session, bridge) = live_ci_bridge("exit 0", Duration::from_millis(200));
        let provider = adapter(bridge, &rt, CiCaps::default());
        assert!(
            matches!(provider.runs(&loc(), None, 5), Err(CiError::Other(message)) if message.contains("plugin transport"))
        );
    }

    #[test]
    fn runs_from_a_spawn_blocking_thread_succeeds() {
        let rt = runtime();
        let (_fixture, _session, bridge) = live_ci_bridge(SUCCESS_SCRIPT, Duration::from_secs(3));
        let provider = adapter(bridge, &rt, CiCaps::default());
        let result = rt
            .block_on(async move {
                tokio::task::spawn_blocking(move || provider.runs(&loc(), None, 1))
                    .await
                    .unwrap()
            })
            .unwrap();
        assert_eq!(result.runs.len(), 1);
    }

    #[test]
    fn null_mutation_result_is_not_success() {
        assert!(
            matches!(mutation_result("cancel", serde_json::Value::Null), Err(CiError::Other(message)) if message == "plugin cancel reply has no success result")
        );
        assert!(mutation_result("cancel", serde_json::json!(true)).is_ok());
    }

    #[test]
    fn plugin_ci_system_serializes_as_plugin() {
        assert_eq!(
            serde_json::to_string(&CiSystem::Plugin).unwrap(),
            "\"plugin\""
        );
        assert_eq!(CiSystem::Plugin.label(), "Plugin CI");
    }

    #[test]
    fn invalid_caller_ids_are_refused_before_sending() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("ops");
        let script = format!(
            "while read -r line; do printf 'called\\n' >> '{}'; done",
            marker.display()
        );
        let rt = runtime();
        let (_fixture, _session, bridge) = live_ci_bridge(&script, Duration::from_millis(200));
        let provider = adapter(bridge, &rt, CiCaps::default());
        assert!(matches!(
            provider.run_detail(&loc(), "abc"),
            Err(CiError::Other(_))
        ));
        assert!(!marker.exists());
    }
}
