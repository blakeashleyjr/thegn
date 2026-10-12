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
//! `mode` is optional and says what the run produced: `snapshot` (the whole
//! window; anything cached and absent is dropped, and a `sync_token` may still
//! be returned), `delta` (changes since `THEGN_CAL_SYNC_TOKEN`; only valid when
//! one was sent), `unchanged` (nothing changed; only valid when a token was
//! sent, and with no events) or `reset` (replace the cache even if empty). A
//! plugin that omits it keeps the old reading: no token sent means a snapshot;
//! otherwise a returned `sync_token` means a delta and none means a snapshot.
//!
//! Out — one JSON object per line:
//!
//! | verb | params |
//! |---|---|
//! | `manifest` | a [`PluginManifest`]; optional, but the only way to request capabilities |
//! | `events` | `{events: [...], deleted: [...], sync_token: "...", mode: "..."}`; repeatable |
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
use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use thegn_core::calendar::admission::AdmissionLimit;
use thegn_core::calendar::validate::{EventDefect, validate_event};
use thegn_core::calendar::{AdmissionError, AdmissionMeter, CalEvent};
use thegn_core::config_calendar::CalendarAccount;
use thegn_core::plugin_api::{Capability, ExtensionPoint, HostContract, PluginManifest};

use super::{AccountAdmission, CalendarBackend, CalendarCaps, CalendarError, EventPage, PageMode};
use crate::plugin::proc::{self, NdjsonSink, PluginError};

/// Non-JSON stdout lines kept for the warning log.
const MAX_JUNK_KEPT: usize = 3;

/// What one plugin run admitted so far.
struct Admitted {
    meter: AdmissionMeter,
    events: Vec<CalEvent>,
    deleted: Vec<String>,
    sync_token: String,
    /// The mode the plugin declared, if any (THE-452).
    mode: Option<PageMode>,
    /// Set when a visitor stopped on the budget, so the serde error it had to
    /// raise to stop is reported as the admission refusal it really is.
    refusal: Option<AdmissionError>,
    /// Set when an event decoded but failed domain validation.
    invalid_event: Option<(usize, EventDefect)>,
}

impl Admitted {
    fn refuse<E: de::Error>(&mut self, e: AdmissionError) -> E {
        self.refusal = Some(e);
        E::custom("admission budget exceeded")
    }
}

/// Admits a plugin's output one line at a time, on the reader thread.
///
/// An `events` line is never decoded into an intermediate JSON tree: its
/// `events` and `deleted` arrays are walked element by element straight off
/// the line, the account's record budget is checked **before** each element is
/// decoded (an element that would not fit is skipped unallocated to find out
/// whether it exists), and each decoded event is charged immediately. Any
/// refusal or malformed element fails the whole run — a plugin's output is
/// never published partially — and the pipe is still drained.
struct AdmittingSink {
    admitted: Admitted,
    granted: Vec<Capability>,
    junk: Vec<String>,
    error: Option<CalendarError>,
}

/// Just the verb, so a line can be routed without building its params.
/// Unknown fields (the params) are skipped without being allocated.
#[derive(serde::Deserialize)]
struct Head {
    method: String,
}

#[derive(serde::Deserialize)]
struct ManifestLine {
    params: PluginManifest,
}

#[derive(serde::Deserialize, Default)]
struct LogParams {
    #[serde(default)]
    level: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(serde::Deserialize)]
struct LogLine {
    #[serde(default)]
    params: Option<LogParams>,
}

/// `{"method":"events","params":{...}}`, visited in place.
struct EventsLine<'s>(&'s mut Admitted);
/// The `params` object of an events line.
struct EventsParams<'s>(&'s mut Admitted);
/// The `events` array.
struct EventList<'s>(&'s mut Admitted);
/// The `deleted` array.
struct DeletedList<'s>(&'s mut Admitted);

impl<'de> DeserializeSeed<'de> for EventsLine<'_> {
    type Value = ();
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for EventsLine<'_> {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("an events message")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let sink = self.0;
        while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
            if key == "params" {
                map.next_value_seed(EventsParams(&mut *sink))?;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(())
    }
}

impl<'de> DeserializeSeed<'de> for EventsParams<'_> {
    type Value = ();
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for EventsParams<'_> {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("an events params object")
    }
    /// `params` omitted or null: an empty message.
    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let sink = self.0;
        while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
            match key.as_ref() {
                "events" => map.next_value_seed(EventList(&mut *sink))?,
                "deleted" => map.next_value_seed(DeletedList(&mut *sink))?,
                "sync_token" => sink.sync_token = map.next_value::<String>()?,
                "mode" => {
                    let raw = map.next_value::<std::borrow::Cow<'de, str>>()?;
                    let Some(mode) = PageMode::parse(&raw) else {
                        return Err(de::Error::custom("unknown page mode"));
                    };
                    // Messages are repeatable; they must agree on what the run is.
                    if sink.mode.is_some_and(|prior| prior != mode) {
                        return Err(de::Error::custom("conflicting page modes"));
                    }
                    sink.mode = Some(mode);
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(())
    }
}

impl<'de> DeserializeSeed<'de> for EventList<'_> {
    type Value = ();
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for EventList<'_> {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("an array of events")
    }
    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        let sink = self.0;
        let mut index = 0;
        loop {
            if !sink.meter.has_record_room() {
                // Full: find out without allocating whether another exists.
                return match seq.next_element::<IgnoredAny>()? {
                    None => Ok(()),
                    Some(_) => {
                        Err(sink.refuse(AdmissionError::new(AdmissionLimit::AccountRecords)))
                    }
                };
            }
            let Some(e) = seq.next_element::<CalEvent>()? else {
                return Ok(());
            };
            if let Err(defect) = validate_event(&e) {
                sink.invalid_event = Some((index, defect));
                return Err(<A::Error as de::Error>::custom("invalid event"));
            }
            if let Err(a) = sink.meter.admit_materialized(&e) {
                return Err(sink.refuse(a));
            }
            sink.events.push(e);
            index += 1;
        }
    }
}

impl<'de> DeserializeSeed<'de> for DeletedList<'_> {
    type Value = ();
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for DeletedList<'_> {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("an array of event ids")
    }
    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        let sink = self.0;
        loop {
            if !sink.meter.has_record_room() {
                return match seq.next_element::<IgnoredAny>()? {
                    None => Ok(()),
                    Some(_) => {
                        Err(sink.refuse(AdmissionError::new(AdmissionLimit::AccountRecords)))
                    }
                };
            }
            let Some(id) = seq.next_element::<String>()? else {
                return Ok(());
            };
            if let Err(a) = sink.meter.admit_deletion(id.len()) {
                return Err(sink.refuse(a));
            }
            sink.deleted.push(id);
        }
    }
}

impl AdmittingSink {
    fn events_line(&mut self, text: &str) -> Result<(), CalendarError> {
        let mut de = serde_json::Deserializer::from_str(text);
        let result = EventsLine(&mut self.admitted)
            .deserialize(&mut de)
            .and_then(|()| de.end());
        match result {
            Ok(()) => Ok(()),
            Err(e) => Err(match self.admitted.invalid_event.take() {
                Some((index, defect)) => CalendarError::Parse(format!(
                    "plugin sent an invalid event (element {index}: {defect})"
                )),
                None => match self.admitted.refusal.take() {
                    Some(a) => a.into(),
                    // Position only: a serde message can quote plugin data.
                    None => CalendarError::Parse(format!(
                        "plugin sent a malformed events message (column {})",
                        e.column()
                    )),
                },
            }),
        }
    }

    fn message(&mut self, method: &str, text: &str) -> Result<(), CalendarError> {
        match method {
            "manifest" => match serde_json::from_str::<ManifestLine>(text) {
                Ok(m) => check_manifest(&self.granted, &m.params),
                Err(e) => Err(CalendarError::Parse(format!("bad manifest: {e}"))),
            },
            "events" => self.events_line(text),
            "log" => {
                // A plugin's own diagnostics belong in the log, never in the
                // UI — it has no way to know what is on screen.
                let p = serde_json::from_str::<LogLine>(text)
                    .ok()
                    .and_then(|l| l.params)
                    .unwrap_or_default();
                tracing::debug!(
                    target: "thegn::calendar::plugin",
                    level = p.level.as_deref().unwrap_or("info"),
                    message = p.message.as_deref().unwrap_or_default(),
                    "plugin log"
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
        let Ok(head) = serde_json::from_str::<Head>(text) else {
            if self.junk.len() < MAX_JUNK_KEPT {
                self.junk.push(text.chars().take(200).collect());
            }
            return true;
        };
        match self.message(&head.method, text) {
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
    zone: thegn_core::calendar::Tz,
    admission: AccountAdmission,
}

impl CommandBackend {
    pub(crate) fn new(a: &CalendarAccount, admission: AccountAdmission) -> Self {
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
            zone: admission.zone(),
            admission,
        }
    }

    /// The environment handed to the plugin.
    pub(crate) fn query_env(
        &self,
        from: NaiveDate,
        to: NaiveDate,
        token: &str,
    ) -> BTreeMap<String, String> {
        let mut env = self.env.clone();
        env.insert("THEGN_CAL_FROM".into(), from.to_string());
        env.insert("THEGN_CAL_TO".into(), to.to_string());
        env.insert("THEGN_CAL_SYNC_TOKEN".into(), token.to_string());
        env.insert("THEGN_CAL_HOME_ZONE".into(), self.zone.name().to_string());
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
        // No write support yet. Sync-token handling is not a capability: a
        // plugin decides for itself whether to honour the token, and the host
        // interprets the page it gets back.
        CalendarCaps::default()
    }

    fn list_events<'a>(
        &'a self,
        from: NaiveDate,
        to: NaiveDate,
        sync_token: &'a str,
    ) -> BoxFuture<'a, Result<EventPage, CalendarError>> {
        Box::pin(async move {
            // The persisted cursor is tagged with the home zone, like an ICS
            // ETag. A stale or legacy token forces a full plugin snapshot.
            let request_token = super::ics_url::request_etag(self.zone.name(), sync_token);
            // One absolute deadline for the whole fetch, fallback included, so
            // a plugin can never hold the sync thread for two full timeouts.
            let deadline = std::time::Instant::now() + self.timeout;
            match self.run_once(from, to, request_token, deadline).await {
                // A delta over the account's own budget would be refused again
                // on every tick, since the cursor is (correctly) not advanced.
                // Ask once for a full snapshot instead; it may well fit.
                Err(CalendarError::Admission(a))
                    if !request_token.is_empty() && a.is_account_limit() =>
                {
                    tracing::debug!(
                        target: "thegn::calendar::plugin",
                        "plugin delta exceeds the admission budget — retrying as a full fetch"
                    );
                    self.run_once(from, to, "", deadline).await
                }
                other => other,
            }
        })
    }
}

impl CommandBackend {
    /// One plugin run, admitted line by line.
    async fn run_once(
        &self,
        from: NaiveDate,
        to: NaiveDate,
        sync_token: &str,
        deadline: std::time::Instant,
    ) -> Result<EventPage, CalendarError> {
        if self.argv.is_empty() {
            return Err(CalendarError::NotConfigured);
        }
        // What is left of the shared deadline; a fallback run gets the
        // remainder, never a fresh timeout.
        let timeout = deadline.saturating_duration_since(std::time::Instant::now());
        if timeout.is_zero() {
            return Err(CalendarError::Timeout(
                "calendar command exceeded its deadline",
            ));
        }
        let argv = self.argv.clone();
        let env = self.query_env(from, to, sync_token);
        let cwd = self.cwd.clone();
        let sink = AdmittingSink {
            admitted: Admitted {
                meter: self.admission.meter(),
                events: Vec::new(),
                deleted: Vec::new(),
                sync_token: String::new(),
                mode: None,
                refusal: None,
                invalid_event: None,
            },
            granted: self.granted.clone(),
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
            PluginError::Timeout(_) => {
                CalendarError::Timeout("calendar command exceeded its deadline")
            }
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
        let a = sink.admitted;
        let resumed = !sync_token.is_empty();
        let mode = match a.mode {
            Some(declared) => declared,
            // A plugin that predates `mode` (THE-452): a request with no
            // cursor can only have produced a full picture, and on a resume a
            // returned cursor is read as a delta, none as a snapshot.
            None if resumed && !a.sync_token.is_empty() => PageMode::Delta,
            None => PageMode::Snapshot,
        };
        match mode {
            // Fail closed on a contradiction rather than guess at one.
            PageMode::Unchanged if !resumed => Err(CalendarError::Parse(
                "plugin declared the calendar unchanged without a cursor to compare".into(),
            )),
            PageMode::Unchanged if !a.events.is_empty() || !a.deleted.is_empty() => Err(
                CalendarError::Parse("plugin declared the calendar unchanged but sent data".into()),
            ),
            PageMode::Unchanged => {
                let token = if a.sync_token.is_empty() {
                    sync_token
                } else {
                    a.sync_token.as_str()
                };
                Ok(EventPage::unchanged(super::ics_url::stored_cursor(
                    self.zone.name(),
                    token,
                )))
            }
            PageMode::Delta if !resumed => Err(CalendarError::Parse(
                "plugin declared a delta for a request that carried no cursor".into(),
            )),
            mode => {
                // A snapshot omits what is gone, so tombstones a mode-less
                // plugin sent alongside one carry nothing. A plugin that
                // *declared* `snapshot` and sent them anyway is contradicting
                // itself, and `from_meter` refuses that.
                let deleted = if a.mode.is_none() && mode != PageMode::Delta {
                    Vec::new()
                } else {
                    a.deleted
                };
                EventPage::from_meter(
                    a.meter,
                    mode,
                    a.events,
                    deleted,
                    super::ics_url::stored_cursor(self.zone.name(), &a.sync_token),
                )
            }
        }
    }
}

#[cfg(test)]
#[path = "command_tests.rs"]
mod tests;
