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
//! | `THEGN_CAL_MAX_EVENTS` | cap the plugin should respect |
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
use thegn_core::calendar::CalEvent;
use thegn_core::config_calendar::CalendarAccount;
use thegn_core::plugin_api::{Capability, ExtensionPoint, HostContract, PluginManifest};

use super::{CalendarBackend, CalendarCaps, CalendarError, EventPage};
use crate::plugin::proc::{self, PluginError};

/// What one plugin run's `events` messages added up to.
#[derive(Debug, Default)]
struct Collected {
    events: Vec<CalEvent>,
    deleted: Vec<String>,
    sync_token: String,
    manifest: Option<PluginManifest>,
}

pub struct CommandBackend {
    argv: Vec<String>,
    cwd: String,
    env: BTreeMap<String, String>,
    granted: Vec<Capability>,
    timeout: Duration,
    zone: String,
    max_events: usize,
}

impl CommandBackend {
    pub fn new(a: &CalendarAccount) -> Self {
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
            max_events: 0,
        }
    }

    pub fn with_zone(mut self, zone: &str) -> Self {
        self.zone = zone.to_string();
        self
    }

    pub fn with_max_events(mut self, n: usize) -> Self {
        self.max_events = n;
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
        env.insert("THEGN_CAL_MAX_EVENTS".into(), self.max_events.to_string());
        env.insert(
            "THEGN_PLUGIN_API".into(),
            thegn_core::plugin_api::API_VERSION.to_string(),
        );
        env
    }

    /// Fold a run's messages into events, checking the manifest as we go.
    fn collect(&self, run: &proc::PluginRun) -> Result<Collected, CalendarError> {
        let mut out = Collected::default();
        for msg in &run.messages {
            match msg.method.as_str() {
                "manifest" => match serde_json::from_value::<PluginManifest>(msg.params.clone()) {
                    Ok(m) => {
                        self.check_manifest(&m)?;
                        out.manifest = Some(m);
                    }
                    Err(e) => {
                        return Err(CalendarError::Parse(format!("bad manifest: {e}")));
                    }
                },
                "events" => {
                    // Every field optional: `{"events":[...]}` is a complete
                    // message, and a plugin may page by sending several.
                    if let Some(v) = msg.params.get("events")
                        && let Ok(mut evs) = serde_json::from_value::<Vec<CalEvent>>(v.clone())
                    {
                        out.events.append(&mut evs);
                    }
                    if let Some(v) = msg.params.get("deleted")
                        && let Ok(mut ids) = serde_json::from_value::<Vec<String>>(v.clone())
                    {
                        out.deleted.append(&mut ids);
                    }
                    if let Some(t) = msg.params.get("sync_token").and_then(|v| v.as_str()) {
                        out.sync_token = t.to_string();
                    }
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
                }
                other => tracing::debug!(
                    target: "thegn::calendar::plugin",
                    verb = other,
                    "ignoring unknown verb"
                ),
            }
        }
        Ok(out)
    }

    /// Negotiate the manifest against what this account grants.
    ///
    /// Uses [`HostContract::negotiate`] verbatim — it is already written and
    /// tested; this is the first thing to actually call it.
    fn check_manifest(&self, m: &PluginManifest) -> Result<(), CalendarError> {
        // A calendar plugin may only contribute a data source; asking for a
        // sidebar tab or a theme here is simply not accepted.
        let contract = HostContract::new(thegn_core::plugin_api::API_VERSION)
            .with_extension_points([ExtensionPoint::DataSource])
            .with_grants(self.granted.clone());
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

    async fn list_events(
        &self,
        from: NaiveDate,
        to: NaiveDate,
        sync_token: &str,
    ) -> Result<EventPage, CalendarError> {
        if self.argv.is_empty() {
            return Err(CalendarError::NotConfigured);
        }
        let argv = self.argv.clone();
        let env = self.query_env(from, to, sync_token);
        let cwd = self.cwd.clone();
        let timeout = self.timeout;
        // The runner blocks (it polls for exit to enforce the timeout), so it
        // must not sit on an async worker.
        let run = tokio::task::spawn_blocking(move || {
            let dir = (!cwd.trim().is_empty()).then(|| std::path::PathBuf::from(&cwd));
            proc::spawn_ndjson(&argv, &env, dir.as_deref(), timeout)
        })
        .await
        .map_err(|e| CalendarError::Subprocess(e.to_string()))?;

        let run = run.map_err(|e| match e {
            PluginError::Timeout(_) => CalendarError::Network(e.to_string()),
            other => CalendarError::Subprocess(other.to_string()),
        })?;

        for j in run.junk.iter().take(3) {
            tracing::warn!(
                target: "thegn::calendar::plugin",
                line = %j,
                "plugin wrote a non-JSON line to stdout — diagnostics belong on stderr or in a `log` message"
            );
        }

        let mut c = self.collect(&run)?;
        let partial = run.truncated || (self.max_events > 0 && c.events.len() > self.max_events);
        if self.max_events > 0 {
            c.events.truncate(self.max_events);
        }
        Ok(EventPage {
            events: c.events,
            deleted: c.deleted,
            sync_token: c.sync_token,
            partial,
            unchanged: false,
        })
    }
}
