//! The `command` inbox backend (THE-2214): any program that prints
//! newline-delimited JSON.
//!
//! # The contract
//!
//! thegn passes the query in the **environment** and reads JSON from stdout,
//! like the calendar command backend: a shell script only has to *print*
//! JSON, never *parse* it.
//!
//! In:
//!
//! | variable | meaning |
//! |---|---|
//! | `THEGN_INBOX_ACCOUNT` | the account's `name` |
//! | `THEGN_INBOX_CURSOR` | the cursor the last successful run returned, or empty for a full fetch |
//! | `THEGN_INBOX_MAX_ITEMS` | the most items one run may return: more than this fails the whole run |
//! | `THEGN_PLUGIN_API` | the API version thegn speaks |
//!
//! The account's `env` is added too. Each value is a secret ref
//! (`keyring:`/`env:`/`file:`, resolved by the daemon at fetch time) or a
//! plain string. A ref that does not resolve fails the fetch before anything
//! is spawned.
//!
//! Out — one JSON object per line:
//!
//! | verb | params |
//! |---|---|
//! | `items` | `{items: [{id, title, url, from, labels, due, updated_at, priority}], removed: [...], cursor: "...", mode: "snapshot" \| "delta"}`; repeatable, items concatenated |
//! | `log` | `{level, message}` → tracing, never the UI |
//!
//! Every item field except `id` is optional. `mode` defaults to `snapshot`
//! (the items are everything currently in the inbox; `removed` must then be
//! empty). `delta` (changes since `THEGN_INBOX_CURSOR`, with `removed` the
//! ids that left) is only valid when a cursor was sent.
//!
//! Protocol rules: unknown verbs, a `delta` with no cursor sent, more than
//! `THEGN_INBOX_MAX_ITEMS` items in total, conflicting modes or conflicting
//! non-empty cursors fail the whole run, and nothing changes. A run that
//! sends no `items` message at all also fails (a script that printed only
//! an error must not read as an empty inbox). Non-JSON stdout lines are junk:
//! the first three are quoted, scrubbed, in the error. Everything the command
//! prints is untrusted; the producer sanitizes every item before it is
//! stored, shown or turned into a rule fact, and item text never becomes
//! argv, env or a path.
//!
//! The command runs with **your user's authority** (filesystem, network,
//! executables; environment limited to the plugin allowlist plus the
//! account's `env`). `[inbox]` is global config only, so no repo overlay,
//! workspace, profile or Lead rule can define one.
//!
//! Unread mail with [himalaya](https://github.com/pimalaya/himalaya):
//!
//! ```toml
//! [[inbox.accounts]]
//! name = "mail"
//! kind = "command"
//! command = ["sh", "-c", """
//! himalaya envelope list --output json --page-size "$THEGN_INBOX_MAX_ITEMS" not flag seen \
//!   | jq -c '{method:"items", params:{mode:"snapshot", items:[.[] | {
//!       id: ("msg-" + (.id|tostring)), title: .subject,
//!       from: (.from.name // .from.addr // ""), labels: .flags}]}}'
//! """]
//! ```
//!
//! Jira issues assigned to you, through JQL:
//!
//! ```toml
//! [[inbox.accounts]]
//! name = "jira"
//! kind = "command"
//! env = { JIRA_TOKEN = "keyring:jira", JIRA_URL = "https://example.atlassian.net" }
//! command = ["sh", "-c", """
//! curl -fsS -H "Authorization: Bearer $JIRA_TOKEN" -G "$JIRA_URL/rest/api/2/search" \
//!   --data-urlencode 'jql=assignee = currentUser() AND statusCategory != Done' \
//!   --data-urlencode "maxResults=$THEGN_INBOX_MAX_ITEMS" \
//!   | jq -c --arg base "$JIRA_URL" '{method:"items", params:{mode:"snapshot", items:[.issues[] | {
//!       id: .key, title: .fields.summary, url: ($base + "/browse/" + .key),
//!       from: (.fields.reporter.displayName // ""), labels: .fields.labels,
//!       due: .fields.duedate}]}}'
//! """]
//! ```

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::future::BoxFuture;
use thegn_core::config::InboxAccount;
use thegn_core::config_inbox::{MAX_ITEMS_CAP, MAX_TIMEOUT_SECS};
use thegn_core::inbox::{InboxItem, InboxPage, MAX_SEEN, PageMode};
use thegn_core::seam::{Availability, ProbeReport};
use thegn_core::secretref::{BareAs, SecretRef};

use super::{InboxBackend, InboxCaps, InboxError};
use crate::plugin::proc::{self, NdjsonSink, PluginError, StreamRun};

/// Non-JSON stdout lines kept (scrubbed) for the error.
const MAX_JUNK_KEPT: usize = 3;
/// Characters of one junk line kept.
const MAX_JUNK_CHARS: usize = 200;
/// The longest cursor a command may return (it is persisted and handed back
/// in the environment).
pub const MAX_CURSOR_BYTES: usize = 1024;
/// The note every command probe carries.
pub const AUTHORITY_NOTE: &str = "runs with your user's authority";
/// Tracing target for the command's own `log` lines and junk warnings.
const TARGET: &str = "thegn::inbox::command";

/// Secret values that a command may echo into output or diagnostics: every
/// account env value (raw and resolved) and every argument after the
/// program. A copy of the calendar command backend's scrubber.
#[derive(Debug, Clone, Default)]
struct SecretScrub {
    values: Vec<String>,
}

impl SecretScrub {
    fn for_account(argv: &[String], env: &BTreeMap<String, String>) -> Self {
        Self::from_values(env.values().chain(argv.iter().skip(1)))
    }

    fn from_values<'v>(source: impl Iterator<Item = &'v String>) -> Self {
        let mut values: Vec<String> = Vec::new();
        for value in source {
            if !value.trim().is_empty() && !values.contains(value) {
                values.push(value.clone());
            }
        }
        values.sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
        Self { values }
    }

    fn scrub(&self, text: &str) -> String {
        let mut scrubbed = String::with_capacity(text.len());
        let mut offset = 0;
        while offset < text.len() {
            let remaining = &text[offset..];
            if let Some(value) = self
                .values
                .iter()
                .filter(|value| remaining.starts_with(value.as_str()))
                .max_by_key(|value| value.len())
            {
                scrubbed.push_str("<redacted>");
                offset += value.len();
            } else {
                let character = remaining.chars().next().expect("offset is before end");
                scrubbed.push(character);
                offset += character.len_utf8();
            }
        }
        scrubbed
    }

    /// [`scrub`](Self::scrub) for a stderr tail `proc` may have cut mid-secret:
    /// a trailing prefix of a secret is redacted too.
    fn scrub_capped_stderr(&self, text: &str) -> String {
        let scrubbed = self.scrub(text);
        if text.len() < proc::MAX_STDERR - 4 {
            return scrubbed;
        }

        let checked = scrubbed.strip_suffix('\u{fffd}').unwrap_or(&scrubbed);
        for value in &self.values {
            let value_bytes = value.as_bytes();
            let checked_bytes = checked.as_bytes();
            let max_prefix = value_bytes.len().min(checked_bytes.len());
            for prefix_len in (1..=max_prefix).rev() {
                if prefix_len < value_bytes.len()
                    && value.is_char_boundary(prefix_len)
                    && checked_bytes.ends_with(&value_bytes[..prefix_len])
                    && let Some(prefix) = checked.get(..checked.len() - prefix_len)
                {
                    return format!("{prefix}<redacted>");
                }
            }
        }
        scrubbed
    }

    fn scrub_error(&self, error: InboxError) -> InboxError {
        match error {
            InboxError::Network(text) => InboxError::Network(self.scrub(&text)),
            InboxError::Auth(text) => InboxError::Auth(self.scrub(&text)),
            InboxError::Api(text) => InboxError::Api(self.scrub(&text)),
            InboxError::Subprocess(text) => InboxError::Subprocess(self.scrub(&text)),
            InboxError::Parse(text) => InboxError::Parse(self.scrub(&text)),
            InboxError::Io(text) => InboxError::Io(self.scrub(&text)),
            other => other,
        }
    }
}

/// One item as the command prints it. Every field but `id` may be omitted.
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct WireItem {
    id: String,
    title: String,
    url: String,
    from: String,
    labels: Vec<String>,
    due: Option<String>,
    updated_at: Option<i64>,
    priority: Option<u8>,
}

impl From<WireItem> for InboxItem {
    fn from(w: WireItem) -> Self {
        InboxItem {
            id: w.id,
            title: w.title,
            url: w.url,
            from: w.from,
            labels: w.labels,
            due: w.due,
            updated_at: w.updated_at,
            priority: w.priority,
        }
    }
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct ItemsParams {
    items: Vec<WireItem>,
    removed: Vec<String>,
    cursor: Option<String>,
    mode: Option<String>,
}

#[derive(serde::Deserialize)]
struct ItemsLine {
    #[serde(default)]
    params: Option<ItemsParams>,
}

/// Just the verb, so a line can be routed before its params are decoded.
#[derive(serde::Deserialize)]
struct Head {
    method: String,
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

/// Collects one run's output on `proc`'s reader thread. The first protocol
/// error stops the run; nothing is published from a failed run.
struct ItemSink {
    scrub: SecretScrub,
    max_items: usize,
    /// A cursor was sent (`THEGN_INBOX_CURSOR` non-empty).
    resumed: bool,
    items: Vec<InboxItem>,
    removed: Vec<String>,
    cursor: String,
    mode: Option<PageMode>,
    saw_items: bool,
    junk: Vec<String>,
    /// Uncapped count of non-JSON lines (`junk` keeps only a few, scrubbed).
    junk_lines: usize,
    error: Option<InboxError>,
    retained: usize,
}

impl ItemSink {
    fn new(scrub: SecretScrub, max_items: usize, resumed: bool) -> Self {
        ItemSink {
            scrub,
            max_items,
            resumed,
            items: Vec::new(),
            removed: Vec::new(),
            cursor: String::new(),
            mode: None,
            saw_items: false,
            junk: Vec::new(),
            junk_lines: 0,
            error: None,
            retained: 0,
        }
    }

    fn items_line(&mut self, text: &str) -> Result<(), InboxError> {
        self.saw_items = true;
        let params = match serde_json::from_str::<ItemsLine>(text) {
            Ok(line) => line.params.unwrap_or_default(),
            // Position only: a serde message can quote command output.
            Err(e) => {
                return Err(InboxError::Parse(format!(
                    "command sent a malformed items message (column {})",
                    e.column()
                )));
            }
        };
        if let Some(raw) = params.mode.as_deref() {
            let mode = match raw {
                "snapshot" => PageMode::Snapshot,
                "delta" => PageMode::Delta,
                _ => return Err(InboxError::Parse("command sent an unknown mode".into())),
            };
            if self.mode.is_some_and(|prior| prior != mode) {
                return Err(InboxError::Parse("command sent conflicting modes".into()));
            }
            if mode == PageMode::Delta && !self.resumed {
                return Err(InboxError::Parse(
                    "command sent a delta for a request that carried no cursor".into(),
                ));
            }
            self.mode = Some(mode);
        }
        if let Some(cursor) = params.cursor.filter(|c| !c.is_empty()) {
            if cursor.len() > MAX_CURSOR_BYTES || cursor.chars().any(char::is_control) {
                return Err(InboxError::Parse(format!(
                    "command sent a cursor that is not printable text of at most {MAX_CURSOR_BYTES} bytes"
                )));
            }
            if !self.cursor.is_empty() && self.cursor != cursor {
                return Err(InboxError::Parse("command sent conflicting cursors".into()));
            }
            self.cursor = cursor;
        }
        if self.items.len().saturating_add(params.items.len()) > self.max_items {
            return Err(InboxError::Parse(format!(
                "command sent more than {} items",
                self.max_items
            )));
        }
        if self.removed.len().saturating_add(params.removed.len()) > MAX_SEEN {
            return Err(InboxError::Parse(format!(
                "command sent more than {MAX_SEEN} removed ids"
            )));
        }
        self.retained = self.retained.saturating_add(text.len());
        if self.retained > proc::MAX_RUN_BYTES {
            return Err(InboxError::Parse(format!(
                "command output exceeded {} bytes",
                proc::MAX_RUN_BYTES
            )));
        }
        self.items
            .extend(params.items.into_iter().map(InboxItem::from));
        self.removed.extend(params.removed);
        Ok(())
    }

    fn message(&mut self, method: &str, text: &str) -> Result<(), InboxError> {
        match method {
            "items" => self.items_line(text),
            "log" => {
                // The command's own diagnostics belong in the log, never in
                // the UI or a notification.
                let p = serde_json::from_str::<LogLine>(text)
                    .ok()
                    .and_then(|l| l.params)
                    .unwrap_or_default();
                tracing::debug!(
                    target: TARGET,
                    level = ?self.scrub.scrub(p.level.as_deref().unwrap_or("info")),
                    text = ?self.scrub.scrub(p.message.as_deref().unwrap_or_default()),
                    "inbox command log"
                );
                Ok(())
            }
            // Scrubbed before it is cut: a cut secret no longer matches.
            other => Err(InboxError::Parse(format!(
                "command sent an unknown message {:?}",
                self.scrub.scrub(other).chars().take(32).collect::<String>()
            ))),
        }
    }

    /// The kept junk lines, as a suffix for an error message (empty if none).
    fn junk_suffix(&self) -> String {
        if self.junk.is_empty() {
            return String::new();
        }
        format!(
            " (non-JSON stdout: {})",
            self.junk
                .iter()
                .map(|j| format!("{j:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

impl NdjsonSink for ItemSink {
    fn line(&mut self, text: &str) -> bool {
        let Ok(head) = serde_json::from_str::<Head>(text) else {
            self.junk_lines = self.junk_lines.saturating_add(1);
            if self.junk.len() < MAX_JUNK_KEPT {
                self.junk.push(
                    self.scrub
                        .scrub(text)
                        .chars()
                        .take(MAX_JUNK_CHARS)
                        .collect(),
                );
            }
            return true;
        };
        match self.message(&head.method, text) {
            Ok(()) => true,
            Err(e) => {
                self.error = Some(self.scrub.scrub_error(e));
                false
            }
        }
    }
}

/// How one run is executed. The real one is
/// [`proc::spawn_ndjson_stream_cancellable`]; tests swap in a counter.
type Runner = fn(
    &[String],
    &BTreeMap<String, String>,
    Option<&Path>,
    Duration,
    ItemSink,
    &dyn Fn() -> bool,
) -> Result<StreamRun<ItemSink>, PluginError>;

/// Raises the flag when dropped: a dropped `fetch` future (deadline, reload,
/// shutdown) is how a run is abandoned, and `proc` then kills its group.
struct AbandonOnDrop(Arc<AtomicBool>);

impl Drop for AbandonOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// The `command` backend for one `[[inbox.accounts]]` entry.
pub struct CommandBackend {
    account: String,
    argv: Vec<String>,
    cwd: String,
    /// The account env as configured: values may be secret refs.
    env: BTreeMap<String, String>,
    timeout: Duration,
    runner: Runner,
}

impl CommandBackend {
    pub fn new(acct: &InboxAccount) -> Self {
        CommandBackend {
            account: acct.name.clone(),
            argv: acct.command.clone(),
            cwd: thegn_core::util::expand_tilde(&acct.cwd),
            env: acct.env.clone(),
            timeout: Duration::from_secs(acct.timeout_secs.clamp(1, MAX_TIMEOUT_SECS)),
            runner: proc::spawn_ndjson_stream_cancellable::<ItemSink>,
        }
    }

    #[cfg(test)]
    fn with_runner(mut self, runner: Runner) -> Self {
        self.runner = runner;
        self
    }

    /// The account env with every secret ref resolved. Fails on the first
    /// ref that does not resolve (nothing is spawned).
    fn resolved_env(&self) -> Result<BTreeMap<String, String>, InboxError> {
        let mut out = BTreeMap::new();
        for (key, raw) in &self.env {
            let value = resolve_secret(&self.account, raw)
                .ok_or_else(|| InboxError::Auth(format!("env {key} secret ref unresolved")))?;
            out.insert(key.clone(), value);
        }
        Ok(out)
    }

    /// The environment handed to the command: the resolved account env, then
    /// the four query variables (which win over the account's).
    fn query_env(
        &self,
        resolved: &BTreeMap<String, String>,
        cursor: &str,
        max_items: usize,
    ) -> BTreeMap<String, String> {
        let mut env = resolved.clone();
        env.insert("THEGN_INBOX_ACCOUNT".into(), self.account.clone());
        env.insert("THEGN_INBOX_CURSOR".into(), cursor.to_string());
        env.insert("THEGN_INBOX_MAX_ITEMS".into(), max_items.to_string());
        env.insert(
            "THEGN_PLUGIN_API".into(),
            thegn_core::plugin_api::API_VERSION.to_string(),
        );
        env
    }

    async fn run(&self, cursor: &str, max_items: usize) -> Result<InboxPage, InboxError> {
        if self.argv.first().is_none_or(|bin| bin.trim().is_empty()) {
            return Err(InboxError::NotConfigured);
        }
        let max_items = max_items.min(MAX_ITEMS_CAP);
        // Resolve before anything is spawned: an unresolved ref runs nothing.
        let resolved = self.resolved_env()?;
        // Raw refs and resolved values alike are scrubbed.
        let scrub = SecretScrub::from_values(
            resolved
                .values()
                .chain(self.env.values())
                .chain(self.argv.iter().skip(1)),
        );
        let env = self.query_env(&resolved, cursor, max_items);
        let argv = self.argv.clone();
        let cwd = self.cwd.clone();
        let timeout = self.timeout;
        let runner = self.runner;
        let sink = ItemSink::new(scrub.clone(), max_items, !cursor.is_empty());
        // The runner blocks (it polls for exit to enforce the timeout), so it
        // must not sit on an async worker.
        let abandoned = Arc::new(AtomicBool::new(false));
        let _abandon = AbandonOnDrop(Arc::clone(&abandoned));
        let run = tokio::task::spawn_blocking(move || {
            let dir = (!cwd.trim().is_empty()).then(|| std::path::PathBuf::from(&cwd));
            runner(&argv, &env, dir.as_deref(), timeout, sink, &|| {
                abandoned.load(Ordering::Acquire)
            })
        })
        .await
        .map_err(|e| InboxError::Io(format!("inbox command task failed: {e}")))?;

        // Both paths scrub exactly once: `map_run_error` its own text, the
        // sink every error and junk line it keeps.
        let run = run.map_err(|e| map_run_error(&scrub, e))?;
        finish(run, !cursor.is_empty())
    }
}

/// A `proc` failure as an [`InboxError`], every text scrubbed once.
fn map_run_error(scrub: &SecretScrub, e: PluginError) -> InboxError {
    match e {
        PluginError::Spawn(why) => InboxError::Io(format!(
            "could not start the command: {}",
            scrub.scrub(&why)
        )),
        PluginError::Timeout(_) => InboxError::Timeout,
        PluginError::Cancelled => InboxError::Io("inbox command cancelled".into()),
        PluginError::Exit { code, stderr } => InboxError::Subprocess(
            PluginError::Exit {
                code,
                stderr: scrub.scrub_capped_stderr(&stderr),
            }
            .to_string(),
        ),
        PluginError::Protocol(text) => InboxError::Parse(scrub.scrub(&text)),
    }
}

/// Turn a finished run into a page, or the error that fails it.
fn finish(run: StreamRun<ItemSink>, resumed: bool) -> Result<InboxPage, InboxError> {
    let sink = run.sink;
    for j in &sink.junk {
        tracing::warn!(
            target: TARGET,
            line = ?j,
            "inbox command wrote a non-JSON line to stdout — diagnostics belong on stderr or in a `log` message"
        );
    }
    if let Some(e) = sink.error {
        return Err(match e {
            InboxError::Parse(text) => InboxError::Parse(format!("{text}{}", sink.junk_suffix())),
            other => other,
        });
    }
    // More output than one run may send: incomplete, so refused rather than
    // published as the whole inbox.
    if run.truncated {
        return Err(InboxError::Parse(format!(
            "command output exceeded the run's limits{}",
            sink.junk_suffix()
        )));
    }
    if !sink.saw_items {
        return Err(InboxError::Parse(format!(
            "command sent no items message{}",
            sink.junk_suffix()
        )));
    }
    let mode = sink.mode.unwrap_or(PageMode::Snapshot);
    if mode == PageMode::Delta && !resumed {
        return Err(InboxError::Parse(
            "command sent a delta for a request that carried no cursor".into(),
        ));
    }
    if mode == PageMode::Snapshot && !sink.removed.is_empty() {
        return Err(InboxError::Parse(
            "command sent removed ids with a snapshot".into(),
        ));
    }
    Ok(InboxPage {
        items: sink.items,
        removed: sink.removed,
        cursor: (!sink.cursor.is_empty()).then_some(sink.cursor),
        mode,
        truncated: false,
    })
}

/// Resolve one env value: a non-literal secret ref through the daemon's
/// broker (`inbox:<account>`), anything else kept as written. `None` when a
/// ref does not resolve to a non-empty value.
fn resolve_secret(account: &str, raw: &str) -> Option<String> {
    let reference = SecretRef::parse(raw, BareAs::Literal);
    if reference.is_literal() || !reference.is_configured() {
        return Some(raw.to_string());
    }
    crate::secret::resolve_ref(&reference, &format!("inbox:{account}"))
        .filter(|value| !value.trim().is_empty())
}

impl InboxBackend for CommandBackend {
    fn kind(&self) -> &'static str {
        "command"
    }

    fn caps(&self) -> InboxCaps {
        InboxCaps {
            cursor: true,
            mark_read: false,
        }
    }

    fn fetch<'a>(
        &'a self,
        cursor: &'a str,
        max_items: usize,
    ) -> BoxFuture<'a, Result<InboxPage, InboxError>> {
        Box::pin(self.run(cursor, max_items))
    }
}

/// Whether the account's program can be started: an absolute path must
/// exist, a relative path with a `/` is checked against `cwd`, and a bare name
/// is looked up on `PATH`.
pub fn availability(acct: &InboxAccount) -> Availability {
    let bin = acct.command.first().map(String::as_str).unwrap_or_default();
    if bin.trim().is_empty() {
        return Availability::Unavailable("no command configured".into());
    }
    if bin.contains('/') {
        let expanded = thegn_core::util::expand_tilde(bin);
        let path = Path::new(&expanded);
        let path = if path.is_absolute() || acct.cwd.trim().is_empty() {
            path.to_path_buf()
        } else {
            Path::new(&thegn_core::util::expand_tilde(&acct.cwd)).join(path)
        };
        return if path.exists() {
            Availability::Ready
        } else {
            Availability::Unavailable(format!("command not found: {bin}"))
        };
    }
    crate::seam::registry::binary_availability(bin)
}

/// The probe for a `command` account: availability plus the authority note.
pub fn probe(acct: &InboxAccount) -> ProbeReport {
    ProbeReport::new(
        "inbox",
        format!("command:{}", acct.name),
        availability(acct),
    )
    .note(AUTHORITY_NOTE)
}

#[cfg(test)]
#[path = "command_tests.rs"]
mod tests;
