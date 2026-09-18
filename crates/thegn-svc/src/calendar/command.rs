//! The plugin backend: any program that prints newline-delimited JSON.
//!
//! # The contract
//!
//! thegn passes the query in the **environment** and reads JSON from stdout.
//! That asymmetry is deliberate and is what makes the surface approachable — a
//! shell plugin has to *print* JSON, never *parse* it — and it matches how
//! `agent_task` already hands `THEGN_TASK_*` to hook commands.
//!
//! In:
//!
//! | variable | meaning |
//! |---|---|
//! | `THEGN_CAL_FROM` / `THEGN_CAL_TO` | window bounds, `YYYY-MM-DD` |
//! | `THEGN_CAL_SYNC_TOKEN` | last token, or empty for a full fetch |
//! | `THEGN_CAL_HOME_ZONE` | IANA zone for floating times |
//! | `THEGN_CAL_MAX_EVENTS` | the account's admission budget: more events + deletions than this fails the whole run |
//! | `THEGN_PLUGIN_API` | the API version thegn speaks |
//!
//! Out — one JSON object per line:
//!
//! | verb | params |
//! |---|---|
//! | `manifest` | a [`PluginManifest`]; optional, but the only way to request capabilities |
//! | `events` | `{events: [...], deleted: [...], sync_token: "..."}`; repeatable |
//! | `log` | `{level, message}` → tracing, never the UI |
//!
//! ```sh
//! #!/bin/sh
//! echo '{"method":"manifest","params":{"id":"khal","name":"khal","version":"1.0",
//!   "api":"0.1.0","capabilities":["run:khal"],
//!   "contributions":[{"id":"khal.events","extension_point":"DataSource","label":"khal"}]}}'
//! khal list --json "$THEGN_CAL_FROM" "$THEGN_CAL_TO" \
//!   | jq -c '{method:"events", params:{events:., sync_token:""}}'
//! ```

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::NaiveDate;
use futures_util::future::BoxFuture;
use thegn_core::calendar::admission::AdmissionLimit;
use thegn_core::calendar::{AdmissionError, AdmissionMeter, CalEvent};
use thegn_core::config_calendar::CalendarAccount;
use thegn_core::plugin_api::{
    Capability, ExtensionPoint, HostContract, PluginManifest, RpcMessage,
};

use super::{AccountAdmission, CalendarBackend, CalendarCaps, CalendarError, EventPage};
use crate::plugin::proc::{self, NdjsonSink, PluginError};

/// Non-JSON stdout lines kept for the warning log.
const MAX_JUNK_KEPT: usize = 3;

/// Admits a plugin's output one line at a time, on the reader thread.
///
/// Each line is at most [`proc::MAX_LINE_BYTES`], so its decoded JSON is
/// bounded by the line; every event is checked against the account's record
/// budget **before** it is decoded into a [`CalEvent`] and charged right after.
/// The first refusal stops admission (the pipe is still drained) and the whole
/// run fails — a plugin's output is never published truncated.
struct AdmittingSink {
    meter: AdmissionMeter,
    granted: Vec<Capability>,
    events: Vec<CalEvent>,
    deleted: Vec<String>,
    sync_token: String,
    junk: Vec<String>,
    error: Option<CalendarError>,
}

impl AdmittingSink {
    fn message(&mut self, msg: RpcMessage) -> Result<(), CalendarError> {
        match msg.method.as_str() {
            "manifest" => match serde_json::from_value::<PluginManifest>(msg.params) {
                Ok(m) => check_manifest(&self.granted, &m),
                Err(e) => Err(CalendarError::Parse(format!("bad manifest: {e}"))),
            },
            "events" => {
                let serde_json::Value::Object(mut params) = msg.params else {
                    return Ok(());
                };
                // Every field optional: `{"events":[...]}` is a complete
                // message, and a plugin may page by sending several. A list
                // that does not decode is dropped whole, as before.
                if let Some(serde_json::Value::Array(items)) = params.remove("events") {
                    let cp = self.meter.checkpoint();
                    let mark = self.events.len();
                    for v in items {
                        if !self.meter.has_record_room() {
                            return Err(AdmissionError::new(AdmissionLimit::AccountRecords).into());
                        }
                        match serde_json::from_value::<CalEvent>(v) {
                            Ok(e) => {
                                self.meter.admit_materialized(&e)?;
                                self.events.push(e);
                            }
                            Err(_) => {
                                self.events.truncate(mark);
                                self.meter.rollback(cp);
                                break;
                            }
                        }
                    }
                }
                if let Some(serde_json::Value::Array(items)) = params.remove("deleted")
                    && items.iter().all(serde_json::Value::is_string)
                {
                    for v in items {
                        if let serde_json::Value::String(id) = v {
                            self.meter.admit_deletion(id.len())?;
                            self.deleted.push(id);
                        }
                    }
                }
                if let Some(serde_json::Value::String(t)) = params.remove("sync_token") {
                    self.sync_token = t;
                }
                Ok(())
            }
            "log" => {
                // A plugin's own diagnostics belong in the log, never in the
                // UI — it has no way to know what is on screen.
                let level = msg
                    .params
                    .get("level")
                    .and_then(|v| v.as_str())
                    .unwrap_or("info");
                let message = msg
                    .params
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                tracing::debug!(
                    target: "thegn::calendar::plugin",
                    level, message, "plugin log"
                );
                Ok(())
            }
            other => {
                tracing::debug!(
                    target: "thegn::calendar::plugin",
                    verb = other,
                    "ignoring unknown verb"
                );
                Ok(())
            }
        }
    }
}

impl NdjsonSink for AdmittingSink {
    fn line(&mut self, text: &str) -> bool {
        let msg = match serde_json::from_str::<RpcMessage>(text) {
            Ok(m) => m,
            Err(_) => {
                if self.junk.len() < MAX_JUNK_KEPT {
                    self.junk.push(text.chars().take(200).collect());
                }
                return true;
            }
        };
        match self.message(msg) {
            Ok(()) => true,
            Err(e) => {
                self.error = Some(e);
                false
            }
        }
    }
}

pub struct CommandBackend {
    argv: Vec<String>,
    cwd: String,
    env: BTreeMap<String, String>,
    granted: Vec<Capability>,
    timeout: Duration,
    zone: String,
    admission: AccountAdmission,
}

impl CommandBackend {
    pub fn new(a: &CalendarAccount, admission: AccountAdmission) -> Self {
        CommandBackend {
            argv: a.command.clone(),
            cwd: thegn_core::util::expand_tilde(&a.cwd),
            env: a.env.clone(),
            // Malformed grants are dropped here; `config validate` is where
            // the user is told about them.
            granted: a
                .capabilities
                .iter()
                .filter_map(|c| Capability::parse(c))
                .collect(),
            timeout: Duration::from_secs(a.timeout_secs.clamp(1, 300)),
            zone: String::new(),
            admission,
        }
    }

    pub fn with_zone(mut self, zone: &str) -> Self {
        self.zone = zone.to_string();
        self
    }

    /// The environment handed to the plugin.
    fn query_env(&self, from: NaiveDate, to: NaiveDate, token: &str) -> BTreeMap<String, String> {
        let mut env = self.env.clone();
        env.insert("THEGN_CAL_FROM".into(), from.to_string());
        env.insert("THEGN_CAL_TO".into(), to.to_string());
        env.insert("THEGN_CAL_SYNC_TOKEN".into(), token.to_string());
        env.insert(
            "THEGN_CAL_HOME_ZONE".into(),
            if self.zone.is_empty() {
                "UTC".into()
            } else {
                self.zone.clone()
            },
        );
        env.insert(
            "THEGN_CAL_MAX_EVENTS".into(),
            self.admission.budget().max_events().to_string(),
        );
        env.insert(
            "THEGN_PLUGIN_API".into(),
            thegn_core::plugin_api::API_VERSION.to_string(),
        );
        env
    }
}

/// Negotiate the manifest against what this account grants.
///
/// Uses [`HostContract::negotiate`] verbatim — it is already written and
/// tested; this is the first thing to actually call it.
fn check_manifest(granted: &[Capability], m: &PluginManifest) -> Result<(), CalendarError> {
    // A calendar plugin may only contribute a data source; asking for a
    // sidebar tab or a theme here is simply not accepted.
    let contract = HostContract::new(thegn_core::plugin_api::API_VERSION)
        .with_extension_points([ExtensionPoint::DataSource])
        .with_grants(granted.to_vec());
    let neg = contract
        .negotiate(m)
        .map_err(|e| CalendarError::Api(format!("plugin {:?}: {e}", m.id.as_str())))?;

    for denied in &neg.denied {
        // Denials are logged, not fatal: a plugin that asks for more than it
        // was granted should still deliver whatever it can without it. The
        // log line is the audit trail.
        tracing::warn!(
            target: "thegn::calendar::plugin",
            plugin = m.id.as_str(),
            capability = %denied,
            "capability denied — add it to this account's `capabilities` if intended"
        );
    }
    for c in &neg.unsupported_contributions {
        tracing::warn!(
            target: "thegn::calendar::plugin",
            plugin = m.id.as_str(),
            extension_point = ?c.extension_point,
            "contribution ignored — a calendar account only accepts DataSource"
        );
    }
    Ok(())
}

impl CalendarBackend for CommandBackend {
    fn provider_id(&self) -> &'static str {
        "command"
    }

    fn caps(&self) -> CalendarCaps {
        CalendarCaps {
            // A plugin decides for itself whether to honour the sync token.
            incremental: true,
            ..Default::default()
        }
    }

    fn list_events<'a>(
        &'a self,
        from: NaiveDate,
        to: NaiveDate,
        sync_token: &'a str,
    ) -> BoxFuture<'a, Result<EventPage, CalendarError>> {
        Box::pin(async move {
            if self.argv.is_empty() {
                return Err(CalendarError::NotConfigured);
            }
            let argv = self.argv.clone();
            let env = self.query_env(from, to, sync_token);
            let cwd = self.cwd.clone();
            let timeout = self.timeout;
            let sink = AdmittingSink {
                meter: self.admission.meter(),
                granted: self.granted.clone(),
                events: Vec::new(),
                deleted: Vec::new(),
                sync_token: String::new(),
                junk: Vec::new(),
                error: None,
            };
            // The runner blocks (it polls for exit to enforce the timeout), so it
            // must not sit on an async worker.
            let run = tokio::task::spawn_blocking(move || {
                let dir = (!cwd.trim().is_empty()).then(|| std::path::PathBuf::from(&cwd));
                proc::spawn_ndjson_stream(&argv, &env, dir.as_deref(), timeout, sink)
            })
            .await
            .map_err(|e| CalendarError::Subprocess(e.to_string()))?;

            let run = run.map_err(|e| match e {
                PluginError::Timeout(_) => CalendarError::Network(e.to_string()),
                other => CalendarError::Subprocess(other.to_string()),
            })?;

            let sink = run.sink;
            for j in &sink.junk {
                tracing::warn!(
                    target: "thegn::calendar::plugin",
                    line = %j,
                    "plugin wrote a non-JSON line to stdout — diagnostics belong on stderr or in a `log` message"
                );
            }
            if let Some(e) = sink.error {
                return Err(e);
            }
            // More messages than one run may send: the output is incomplete, so
            // it is refused rather than published as a whole calendar.
            if run.truncated {
                return Err(AdmissionError::new(AdmissionLimit::Messages).into());
            }
            EventPage::from_meter(sink.meter, sink.events, sink.deleted, sink.sync_token)
        })
    }
}
