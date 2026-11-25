//! Pure agent-settings merge for `thegn mcp wire`.
//!
//! Writing thegn's secret-free entries ([`WIRED_ENTRIES`]: the proxy hub and
//! thegn's own `mcp serve`) into an agent CLI's MCP settings is a JSON merge
//! ([`apply_named`]) or, for Codex's `config.toml`, a TOML merge
//! ([`apply_toml`]), with three non-negotiables, all enforced here (host
//! adapters only supply the file path + the container's path):
//!
//! 1. **Secret-free.** An entry is argv only — no `env` block. Upstream
//!    secrets resolve at spawn in the hub, never in an agent settings file.
//! 2. **Marker-tagged & idempotent.** thegn's JSON entries carry
//!    [`MARKER_KEY`], its TOML tables a [`TOML_MARKER`] comment; wiring twice
//!    yields exactly one entry per key.
//! 3. **Never clobber the user.** thegn refuses to overwrite or remove an entry
//!    it did not mark — a user's own `thegn`-named entry is left untouched.

use serde_json::{Map, Value, json};

/// The `mcpServers` key thegn's proxy entry lives under.
pub const ENTRY_KEY: &str = "thegn";

/// The marker field thegn stamps on its own entry, so it is distinguishable
/// from a user-authored entry of the same name. `x-` prefixed (conventionally
/// ignored by MCP clients).
pub const MARKER_KEY: &str = "x-thegn-managed";

/// The outcome of applying a wire operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireOutcome {
    /// A new thegn entry was inserted.
    Added,
    /// An existing thegn-marked entry was updated (argv changed).
    Updated,
    /// The thegn entry was already exactly what we would write.
    Unchanged,
    /// A thegn-marked entry was removed.
    Removed,
    /// `--remove` ran but there was no thegn entry to remove.
    NothingToRemove,
}

impl WireOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            WireOutcome::Added => "added",
            WireOutcome::Updated => "updated",
            WireOutcome::Unchanged => "unchanged",
            WireOutcome::Removed => "removed",
            WireOutcome::NothingToRemove => "nothing to remove",
        }
    }
}

/// Build the single secret-free proxy entry: `thegn mcp proxy` argv, marked.
/// **No `env` block, ever** — this is the credential-custody guarantee.
pub fn proxy_entry(command: &str) -> Value {
    json!({
        "command": command,
        "args": ["mcp", "proxy"],
        MARKER_KEY: true,
    })
}

/// The `mcpServers` key thegn's own docs/state server entry lives under.
pub const SERVE_ENTRY_KEY: &str = "thegn-serve";

/// The secret-free entry for thegn's own MCP server: `thegn mcp serve` with no
/// `--scopes`, so the scope ceiling comes from config (`[mcp.serve] scopes`,
/// `read` when unset) at every launch rather than being frozen into the agent's
/// settings file. Marked like [`proxy_entry`]; never an `env` block.
pub fn serve_entry(command: &str) -> Value {
    json!({
        "command": command,
        "args": ["mcp", "serve"],
        MARKER_KEY: true,
    })
}

/// Every entry `thegn mcp wire` writes, as `(key, args)`: the aggregated
/// third-party proxy and thegn's own docs/state server. One table so the JSON
/// and TOML adapters can never drift apart.
pub const WIRED_ENTRIES: &[(&str, &[&str])] = &[
    (ENTRY_KEY, &["mcp", "proxy"]),
    (SERVE_ENTRY_KEY, &["mcp", "serve"]),
];

/// The JSON entry for one [`WIRED_ENTRIES`] row.
pub fn wired_entry(command: &str, args: &[&str]) -> Value {
    json!({
        "command": command,
        "args": args,
        MARKER_KEY: true,
    })
}

/// Whether a settings-entry value is thegn-managed (carries [`MARKER_KEY`]).
pub fn is_thegn_managed(entry: &Value) -> bool {
    entry
        .get(MARKER_KEY)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Apply a wire (or `--remove`) operation to an agent's parsed settings tree.
///
/// `container_path` is the JSON path from the document root to the object that
/// holds the server entries (e.g. `["mcpServers"]`, or `["mcp", "servers"]`);
/// it is created if absent. `entry` is [`proxy_entry`]. On `remove`, `entry` is
/// ignored.
///
/// Returns the outcome, or an error when the operation would touch a
/// user-authored entry thegn does not own.
pub fn apply(
    settings: &mut Value,
    container_path: &[&str],
    entry: Value,
    remove: bool,
) -> Result<WireOutcome, String> {
    apply_named(settings, container_path, ENTRY_KEY, entry, remove)
}

/// [`apply`] for an entry stored under `key` (e.g. [`SERVE_ENTRY_KEY`]). The
/// same three guarantees: secret-free, marked and idempotent, never touching
/// an entry of that name thegn did not mark.
pub fn apply_named(
    settings: &mut Value,
    container_path: &[&str],
    key: &str,
    entry: Value,
    remove: bool,
) -> Result<WireOutcome, String> {
    if remove && !path_exists(settings, container_path) {
        // Never create an empty container just to report that nothing was
        // there: removal must leave an unwired file byte-identical.
        return Ok(WireOutcome::NothingToRemove);
    }
    let container = ensure_object_path(settings, container_path)?;

    let existing = container.get(key);
    let existing_is_ours = existing.map(is_thegn_managed).unwrap_or(false);
    let existing_is_foreign = existing.is_some() && !existing_is_ours;

    if remove {
        if existing_is_foreign {
            return Err(format!(
                "an entry named `{key}` exists but was not created by thegn \
                 (no `{MARKER_KEY}` marker) — refusing to remove it"
            ));
        }
        return if existing_is_ours {
            container.remove(key);
            Ok(WireOutcome::Removed)
        } else {
            Ok(WireOutcome::NothingToRemove)
        };
    }

    if existing_is_foreign {
        return Err(format!(
            "an entry named `{key}` exists but was not created by thegn \
             (no `{MARKER_KEY}` marker) — refusing to overwrite it"
        ));
    }
    if existing == Some(&entry) {
        return Ok(WireOutcome::Unchanged);
    }
    let outcome = if existing_is_ours {
        WireOutcome::Updated
    } else {
        WireOutcome::Added
    };
    container.insert(key.to_string(), entry);
    Ok(outcome)
}

/// Whether every segment of `path` exists as an object below `root`.
fn path_exists(root: &Value, path: &[&str]) -> bool {
    let mut cur = root;
    for seg in path {
        match cur.get(*seg) {
            Some(next) if next.is_object() => cur = next,
            _ => return false,
        }
    }
    cur.is_object()
}

// ── TOML agent settings (Codex `config.toml`) ───────────────────────────────

/// The marker comment thegn writes above its own `[mcp_servers.<key>]` table
/// in a TOML settings file. A comment rather than a key: an MCP client may
/// reject unknown keys inside a server table, but every TOML reader ignores
/// comments, and `toml_edit` writers (Codex's own included) keep them.
pub const TOML_MARKER: &str = "# x-thegn-managed: written by `thegn mcp wire`; \
                               undo with `thegn mcp wire --remove`";

/// The substring that proves a TOML table is thegn's (see [`TOML_MARKER`]).
const TOML_MARKER_TAG: &str = "x-thegn-managed";

/// Whether a TOML item is a table carrying thegn's marker comment.
pub fn toml_is_thegn_managed(item: &toml_edit::Item) -> bool {
    item.as_table()
        .and_then(|t| t.decor().prefix())
        .and_then(|p| p.as_str())
        .is_some_and(|s| s.contains(TOML_MARKER_TAG))
}

/// Whether the marked table `table` already holds exactly `command` + `args`.
fn toml_entry_matches(table: &toml_edit::Table, command: &str, args: &[&str]) -> bool {
    let command_ok = table.get("command").and_then(|i| i.as_str()) == Some(command);
    let args_ok = table
        .get("args")
        .and_then(|i| i.as_array())
        .is_some_and(|arr| {
            arr.len() == args.len()
                && arr
                    .iter()
                    .zip(args.iter())
                    .all(|(v, want)| v.as_str() == Some(*want))
        });
    command_ok && args_ok && table.len() == 2
}

/// Build thegn's marked `[mcp_servers.<key>]` table: argv only, no `env`.
fn toml_entry(command: &str, args: &[&str]) -> toml_edit::Table {
    let mut table = toml_edit::Table::new();
    table.insert("command", toml_edit::value(command));
    let mut arr = toml_edit::Array::new();
    for arg in args {
        arr.push(*arg);
    }
    table.insert("args", toml_edit::value(arr));
    table.decor_mut().set_prefix(format!("\n{TOML_MARKER}\n"));
    table
}

/// [`apply_named`] for a TOML settings document whose server tables live under
/// a top-level `container` table (Codex: `mcp_servers`). The same guarantees:
/// argv only (no `env`), marked (by [`TOML_MARKER`]), idempotent, and a table
/// of that name thegn did not mark is never overwritten or removed. A
/// `container` that exists but is not a standard table (an inline table, a
/// string) is refused rather than rewritten.
pub fn apply_toml(
    doc: &mut toml_edit::DocumentMut,
    container: &str,
    key: &str,
    command: &str,
    args: &[&str],
    remove: bool,
) -> Result<WireOutcome, String> {
    let root = doc.as_table_mut();
    if remove && !root.contains_key(container) {
        return Ok(WireOutcome::NothingToRemove);
    }
    if !root.contains_key(container) {
        let mut fresh = toml_edit::Table::new();
        // `[mcp_servers.thegn]` alone; no empty `[mcp_servers]` header.
        fresh.set_implicit(true);
        root.insert(container, toml_edit::Item::Table(fresh));
    }
    let servers = root
        .get_mut(container)
        .and_then(|item| item.as_table_mut())
        .ok_or_else(|| {
            format!("`{container}` is not a TOML table — refusing to rewrite it; edit it by hand")
        })?;

    let existing = servers.get(key);
    let existing_is_ours = existing.map(toml_is_thegn_managed).unwrap_or(false);
    let existing_is_foreign = existing.is_some() && !existing_is_ours;

    if remove {
        if existing_is_foreign {
            return Err(format!(
                "a `[{container}.{key}]` table exists but was not created by thegn \
                 (no `{TOML_MARKER_TAG}` comment) — refusing to remove it"
            ));
        }
        return if existing_is_ours {
            servers.remove(key);
            Ok(WireOutcome::Removed)
        } else {
            Ok(WireOutcome::NothingToRemove)
        };
    }

    if existing_is_foreign {
        return Err(format!(
            "a `[{container}.{key}]` table exists but was not created by thegn \
             (no `{TOML_MARKER_TAG}` comment) — refusing to overwrite it"
        ));
    }
    if let Some(table) = existing.and_then(|item| item.as_table())
        && toml_entry_matches(table, command, args)
    {
        return Ok(WireOutcome::Unchanged);
    }
    let outcome = if existing_is_ours {
        WireOutcome::Updated
    } else {
        WireOutcome::Added
    };
    servers.insert(key, toml_edit::Item::Table(toml_entry(command, args)));
    Ok(outcome)
}

/// Navigate to (creating as needed) the object at `path`, returning a mutable
/// handle to it. Errors if a path segment exists but is not an object.
fn ensure_object_path<'a>(
    root: &'a mut Value,
    path: &[&str],
) -> Result<&'a mut Map<String, Value>, String> {
    if !root.is_object() {
        // An empty/absent settings file parses to null → start a fresh object.
        if root.is_null() {
            *root = Value::Object(Map::new());
        } else {
            return Err("settings root is not a JSON object".to_string());
        }
    }
    let mut cur = root;
    for seg in path {
        let obj = cur
            .as_object_mut()
            .ok_or_else(|| format!("settings path segment `{seg}` is not a JSON object"))?;
        cur = obj
            .entry(seg.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    cur.as_object_mut()
        .ok_or_else(|| "settings container is not a JSON object".to_string())
}

// ── Lifecycle hooks (Claude `settings.json`, Codex `notify`) ────────────────

/// The shell-comment tag that proves a Claude hook command is thegn's.
pub const HOOK_MARKER: &str = "x-thegn-managed";

/// Claude hook events thegn registers, as `(settings event, hook verb event)`:
/// a finished turn reports `stop`, a permission or idle prompt `notification`.
pub const CLAUDE_HOOK_EVENTS: &[(&str, &str)] = &[("Stop", "stop"), ("Notification", "attention")];

/// Quote one argv word for a POSIX shell (Claude runs hook commands through
/// one). Plain paths stay bare.
fn shell_word(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+' | ':'));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// The marked shell command for one Claude hook event.
pub fn claude_hook_command(command: &str, event: &str) -> String {
    format!(
        "{} agent hook claude {event} # {HOOK_MARKER}",
        shell_word(command)
    )
}

/// The marked shell command for one Claude hook event inside a sandbox: the
/// in-sandbox resolver ([`SANDBOX_RESOLVER`]) instead of the host's path.
pub fn sandbox_claude_hook_command(event: &str) -> String {
    format!(
        "{SANDBOX_COMMAND} -c {} thegn agent hook claude {event} # {HOOK_MARKER}",
        shell_word(SANDBOX_RESOLVER)
    )
}

fn hook_group(command: &str) -> Value {
    json!({
        "hooks": [{
            "type": "command",
            "command": command,
        }]
    })
}

/// Whether a Claude hook group holds a thegn-marked command.
fn hook_group_is_ours(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hooks| {
            hooks.iter().any(|h| {
                h.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|c| c.contains(HOOK_MARKER))
            })
        })
}

/// Add (or `remove`) thegn's lifecycle hook groups in a Claude settings tree.
/// Only groups carrying [`HOOK_MARKER`] are ever replaced or removed; the
/// user's own groups and every other key are left as they were. A `hooks` or
/// event value of the wrong JSON type is refused rather than rewritten.
pub fn apply_claude_hooks(
    settings: &mut Value,
    command: &str,
    remove: bool,
) -> Result<WireOutcome, String> {
    apply_claude_hooks_with(
        settings,
        &|event| claude_hook_command(command, event),
        remove,
    )
}

/// [`apply_claude_hooks`] for a sandbox: hooks run the in-sandbox resolver.
pub fn apply_sandbox_claude_hooks(
    settings: &mut Value,
    remove: bool,
) -> Result<WireOutcome, String> {
    apply_claude_hooks_with(settings, &sandbox_claude_hook_command, remove)
}

/// Shared body: `make(verb_event)` renders the marked hook command.
fn apply_claude_hooks_with(
    settings: &mut Value,
    make: &dyn Fn(&str) -> String,
    remove: bool,
) -> Result<WireOutcome, String> {
    if settings.is_null() {
        *settings = Value::Object(Map::new());
    }
    let root = settings
        .as_object_mut()
        .ok_or_else(|| "settings root is not a JSON object".to_string())?;
    if remove && !root.contains_key("hooks") {
        return Ok(WireOutcome::NothingToRemove);
    }
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| "`hooks` is not a JSON object — refusing to rewrite it".to_string())?;

    let mut any_added = false;
    let mut any_updated = false;
    let mut any_removed = false;
    for (event, verb_event) in CLAUDE_HOOK_EVENTS {
        if remove && !hooks.contains_key(*event) {
            continue;
        }
        let groups = hooks
            .entry(event.to_string())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| {
                format!("`hooks.{event}` is not a JSON array — refusing to rewrite it")
            })?;
        let ours = groups.iter().position(hook_group_is_ours);
        if remove {
            if let Some(i) = ours {
                groups.remove(i);
                any_removed = true;
            }
        } else {
            let want = hook_group(&make(verb_event));
            match ours {
                Some(i) if groups[i] == want => {}
                Some(i) => {
                    groups[i] = want;
                    any_updated = true;
                }
                None => {
                    groups.push(want);
                    any_added = true;
                }
            }
        }
        if groups.is_empty() {
            hooks.remove(*event);
        }
    }
    if any_removed && hooks.is_empty() {
        root.remove("hooks");
    }
    Ok(if remove {
        if any_removed {
            WireOutcome::Removed
        } else {
            WireOutcome::NothingToRemove
        }
    } else if any_added {
        WireOutcome::Added
    } else if any_updated {
        WireOutcome::Updated
    } else {
        WireOutcome::Unchanged
    })
}

/// The Codex `notify` argv thegn writes: Codex appends the event JSON as one
/// more argument, so the verb reads it from the tail.
pub fn codex_notify_argv(command: &str) -> Vec<String> {
    ["agent", "hook", "codex", "stop"]
        .iter()
        .fold(vec![command.to_string()], |mut v, a| {
            v.push((*a).to_string());
            v
        })
}

/// Whether a TOML `notify` value is thegn's own hook argv (its tail is
/// `agent hook codex stop`).
fn toml_notify_is_ours(item: &toml_edit::Item) -> bool {
    let Some(arr) = item.as_array() else {
        return false;
    };
    let words: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
    words.len() == arr.len()
        && words.len() >= 5
        && words[words.len() - 4..] == ["agent", "hook", "codex", "stop"]
}

/// The Codex `notify` argv for a sandbox: the in-sandbox resolver then the verb.
pub fn sandbox_codex_notify_argv() -> Vec<String> {
    std::iter::once(SANDBOX_COMMAND)
        .chain(SANDBOX_PREFIX.iter().copied())
        .chain(["agent", "hook", "codex", "stop"])
        .map(String::from)
        .collect()
}

/// Add (or `remove`) thegn's top-level `notify = [...]` in a Codex
/// `config.toml`. A `notify` the user wrote (any other argv) is never
/// overwritten or removed: Codex runs a single notifier, so this is refused.
pub fn apply_codex_notify(
    doc: &mut toml_edit::DocumentMut,
    command: &str,
    remove: bool,
) -> Result<WireOutcome, String> {
    apply_codex_notify_argv(doc, codex_notify_argv(command), remove)
}

/// [`apply_codex_notify`] for a sandbox: `notify` runs the in-sandbox resolver.
pub fn apply_sandbox_codex_notify(
    doc: &mut toml_edit::DocumentMut,
    remove: bool,
) -> Result<WireOutcome, String> {
    apply_codex_notify_argv(doc, sandbox_codex_notify_argv(), remove)
}

fn apply_codex_notify_argv(
    doc: &mut toml_edit::DocumentMut,
    want: Vec<String>,
    remove: bool,
) -> Result<WireOutcome, String> {
    let root = doc.as_table_mut();
    let existing = root.get("notify");
    let ours = existing.is_some_and(toml_notify_is_ours);
    if remove {
        return match existing {
            None => Ok(WireOutcome::NothingToRemove),
            Some(_) if ours => {
                root.remove("notify");
                Ok(WireOutcome::Removed)
            }
            Some(_) => Err("`notify` was not written by thegn — refusing to remove it".to_string()),
        };
    }
    if existing.is_some() && !ours {
        return Err("`notify` is already set by you — refusing to overwrite it".to_string());
    }
    if ours
        && existing.and_then(|i| i.as_array()).is_some_and(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .eq(want.iter().map(String::as_str))
        })
    {
        return Ok(WireOutcome::Unchanged);
    }
    let mut arr = toml_edit::Array::new();
    for w in &want {
        arr.push(w.as_str());
    }
    root.insert("notify", toml_edit::value(arr));
    Ok(if ours {
        WireOutcome::Updated
    } else {
        WireOutcome::Added
    })
}

// ── YAML agent settings (Hermes `config.yaml`) ──────────────────────────────

/// The marker comment thegn writes directly above its own entry in a YAML
/// settings file. A comment because YAML carries no spare key an agent is
/// guaranteed to ignore, and every YAML reader skips comments.
pub const YAML_MARKER: &str = "# x-thegn-managed: written by `thegn mcp wire`; \
                               undo with `thegn mcp wire --remove`";

fn yaml_indent(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

fn yaml_blank_or_comment(line: &str) -> bool {
    let t = line.trim();
    t.is_empty() || t.starts_with('#')
}

/// A block-mapping line `key: rest` → `(key, rest)`; quoted keys are unquoted.
fn yaml_key_of(line: &str) -> Option<(String, String)> {
    let t = line.trim_start();
    if let Some(q) = t.chars().next().filter(|c| *c == '"' || *c == '\'') {
        let close = t[1..].find(q)? + 1;
        let rest = t[close + 1..].trim_start().strip_prefix(':')?;
        return Some((t[1..close].to_string(), rest.to_string()));
    }
    let mut chars = t.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == ':' && chars.peek().is_none_or(|(_, n)| n.is_whitespace()) {
            return Some((t[..i].trim_end().to_string(), t[i + 1..].to_string()));
        }
    }
    None
}

fn yaml_scalar(s: &str) -> String {
    // A JSON string is a valid YAML double-quoted scalar.
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// The marked block for one entry at `pad` spaces of indent: argv only.
fn yaml_entry_lines(pad: usize, key: &str, command: &str, args: &[&str]) -> Vec<String> {
    let sp = " ".repeat(pad);
    let flow: Vec<String> = args.iter().map(|a| yaml_scalar(a)).collect();
    vec![
        format!("{sp}{YAML_MARKER}"),
        format!("{sp}{key}:"),
        format!("{sp}  command: {}", yaml_scalar(command)),
        format!("{sp}  args: [{}]", flow.join(", ")),
    ]
}

/// `[hdr+1, end)`: the section body after the header at `hdr`, trimmed back to
/// its last content line; plus whether any content line exists.
fn yaml_body_end(lines: &[String], hdr: usize) -> usize {
    let mut end = hdr + 1;
    let mut last = hdr + 1;
    while end < lines.len() && (yaml_indent(&lines[end]) > 0 || yaml_blank_or_comment(&lines[end]))
    {
        if !yaml_blank_or_comment(&lines[end]) {
            last = end + 1;
        }
        end += 1;
    }
    last
}

/// [`apply_named`] for a YAML settings text whose server entries live under a
/// top-level block mapping `container` (Hermes: `mcp_servers`). A text editor,
/// not a YAML serializer, so every user comment, key order and quoting survives
/// byte for byte; the same guarantees hold: argv only, marked by
/// [`YAML_MARKER`], idempotent, and an entry of that name thegn did not mark is
/// never overwritten or removed. Shapes it cannot edit safely (CRLF text, a
/// flow or duplicate `container`, a sequence body, tab indentation) are
/// refused. Returns the new text and the outcome.
pub fn apply_yaml(
    text: &str,
    container: &str,
    key: &str,
    command: &str,
    args: &[&str],
    remove: bool,
) -> Result<(String, WireOutcome), String> {
    if text.contains('\r') {
        return Err("settings use CRLF line endings — refusing to rewrite; edit by hand".into());
    }
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let mut header: Option<(usize, String)> = None;
    for (i, l) in lines.iter().enumerate() {
        if l.starts_with(' ') || l.starts_with('\t') || yaml_blank_or_comment(l) {
            continue;
        }
        if let Some((k, rest)) = yaml_key_of(l)
            && k == container
        {
            if header.is_some() {
                return Err(format!("`{container}` appears twice — refusing to edit"));
            }
            header = Some((i, rest));
        }
    }
    let finish = |lines: Vec<String>| {
        if lines.is_empty() {
            String::new()
        } else {
            format!("{}\n", lines.join("\n"))
        }
    };
    let Some((hdr, rest)) = header else {
        if remove {
            return Ok((text.to_string(), WireOutcome::NothingToRemove));
        }
        if lines.last().is_some_and(|l| !l.trim().is_empty()) {
            lines.push(String::new());
        }
        lines.push(format!("{container}:"));
        lines.extend(yaml_entry_lines(2, key, command, args));
        return Ok((finish(lines), WireOutcome::Added));
    };
    let inline = rest.split(" #").next().unwrap_or("").trim();
    if !inline.is_empty() && !inline.starts_with('#') {
        return Err(format!(
            "`{container}` is not a block mapping — refusing to rewrite it; edit it by hand"
        ));
    }
    let end = yaml_body_end(&lines, hdr);
    let content: Vec<usize> = (hdr + 1..end)
        .filter(|i| !yaml_blank_or_comment(&lines[*i]))
        .collect();
    if content.iter().any(|i| lines[*i].starts_with('\t')) {
        return Err("tab indentation — refusing to edit".into());
    }
    if content
        .iter()
        .any(|i| lines[*i].trim_start().starts_with("- "))
    {
        return Err(format!("`{container}` is a sequence — refusing to edit"));
    }
    let child = content.first().map_or(2, |i| yaml_indent(&lines[*i]));

    let mut found: Option<usize> = None;
    for &i in &content {
        if yaml_indent(&lines[i]) == child && yaml_key_of(&lines[i]).is_some_and(|(k, _)| k == key)
        {
            found = Some(i);
            break;
        }
    }
    let marker_above = |i: usize| {
        i > hdr + 1
            && yaml_indent(&lines[i - 1]) == child
            && lines[i - 1].trim_start().starts_with('#')
            && lines[i - 1].contains(TOML_MARKER_TAG)
    };
    let new_block = yaml_entry_lines(child, key, command, args);

    let Some(i) = found else {
        if remove {
            return Ok((text.to_string(), WireOutcome::NothingToRemove));
        }
        lines.splice(end..end, new_block);
        return Ok((finish(lines), WireOutcome::Added));
    };
    if !marker_above(i) {
        let verb = if remove { "remove" } else { "overwrite" };
        return Err(format!(
            "a `{container}.{key}` entry exists but was not created by thegn \
             (no `{TOML_MARKER_TAG}` comment) — refusing to {verb} it"
        ));
    }
    let start = i - 1;
    let mut stop = i + 1;
    let mut last = i + 1;
    while stop < end && (yaml_blank_or_comment(&lines[stop]) || yaml_indent(&lines[stop]) > child) {
        if !yaml_blank_or_comment(&lines[stop]) {
            last = stop + 1;
        }
        stop += 1;
    }
    if remove {
        lines.drain(start..last);
        if yaml_body_end(&lines, hdr) == hdr + 1 {
            // Nothing else lives under the container: drop the header too (and
            // the one blank line wiring put above it).
            lines.remove(hdr);
            if hdr > 0 && lines[hdr - 1].trim().is_empty() {
                lines.remove(hdr - 1);
            }
        }
        return Ok((finish(lines), WireOutcome::Removed));
    }
    if lines[start..last] == new_block[..] {
        return Ok((text.to_string(), WireOutcome::Unchanged));
    }
    lines.splice(start..last, new_block);
    Ok((finish(lines), WireOutcome::Updated))
}

// ── Format dispatch shared by the host command and remote environments ──────

/// How an agent stores its MCP servers on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireFormat {
    /// A JSON settings file; the container is the JSON path to the server map.
    Json,
    /// A TOML settings file; the first container segment is the top-level
    /// server table (Codex: `[mcp_servers.<name>]`).
    Toml,
    /// A YAML settings file; the first container segment is the top-level
    /// block mapping (Hermes: `mcp_servers:`).
    Yaml,
}

/// The rewritten config body and each entry's outcome.
type WiredBody = (String, Vec<(&'static str, WireOutcome)>);
/// Apply every [`WIRED_ENTRIES`] row to one settings text. `prefix` is put
/// before each entry's own args (empty on the host; the in-sandbox resolver for
/// a remote environment, see [`SANDBOX_COMMAND`]). Returns the new text and the
/// per-key outcomes; a foreign entry or unparseable text fails the whole text
/// before anything is returned.
pub fn apply_all(
    format: WireFormat,
    body: &str,
    container: &[&str],
    command: &str,
    prefix: &[&str],
    remove: bool,
) -> Result<WiredBody, String> {
    let mut outcomes = Vec::new();
    let full = |args: &[&str]| -> Vec<String> {
        prefix
            .iter()
            .chain(args.iter())
            .map(|s| s.to_string())
            .collect()
    };
    let rendered = match format {
        WireFormat::Json => {
            let mut settings: Value = if body.trim().is_empty() {
                Value::Object(Map::new())
            } else {
                serde_json::from_str(body).map_err(|e| format!("not valid JSON: {e}"))?
            };
            for (key, args) in WIRED_ENTRIES {
                let argv = full(args);
                let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
                let entry = wired_entry(command, &refs);
                outcomes.push((
                    *key,
                    apply_named(&mut settings, container, key, entry, remove)?,
                ));
            }
            serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())?
        }
        WireFormat::Toml => {
            let mut doc: toml_edit::DocumentMut =
                body.parse().map_err(|e| format!("not valid TOML: {e}"))?;
            let top = container.first().copied().unwrap_or("mcp_servers");
            for (key, args) in WIRED_ENTRIES {
                let argv = full(args);
                let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
                outcomes.push((
                    *key,
                    apply_toml(&mut doc, top, key, command, &refs, remove)?,
                ));
            }
            doc.to_string()
        }
        WireFormat::Yaml => {
            let top = container.first().copied().unwrap_or("mcp_servers");
            let mut text = body.to_string();
            for (key, args) in WIRED_ENTRIES {
                let argv = full(args);
                let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
                let (next, outcome) = apply_yaml(&text, top, key, command, &refs, remove)?;
                text = next;
                outcomes.push((*key, outcome));
            }
            text
        }
    };
    Ok((rendered, outcomes))
}

/// The command a wired entry runs inside a sandbox / remote environment. The
/// host's own `thegn` path means nothing there, and the resident bridge path is
/// content-addressed (it changes with the pushed bytes and arrives after
/// provisioning), so the entry is a tiny argv-only resolver: `thegn` on `PATH`,
/// else the resident bridge's `thegn`, else a clear error. Still secret-free.
pub const SANDBOX_COMMAND: &str = "/bin/sh";

/// The resolver script behind [`SANDBOX_COMMAND`] (`sh -c <script> thegn <args>`).
pub const SANDBOX_RESOLVER: &str = "t=$(command -v thegn 2>/dev/null); \
     [ -n \"$t\" ] || t=$(ls -d /var/tmp/thegn-bridge/*/thegn 2>/dev/null | head -n 1); \
     [ -n \"$t\" ] || { echo 'thegn: no thegn binary in this environment' >&2; exit 127; }; \
     exec \"$t\" \"$@\"";

/// Args placed before each entry's own args for a sandbox entry.
pub const SANDBOX_PREFIX: &[&str] = &["-c", SANDBOX_RESOLVER, "thegn"];

// ── The Lead's private MCP config (THE-2188) ────────────────────────────────

/// The Lead's `thegn mcp serve` entry key in its private `mcp.json`.
pub const LEAD_SERVE_KEY: &str = "thegn";
/// The Lead's `thegn mcp proxy --profile` hub entry key.
pub const LEAD_HUB_KEY: &str = "thegn-lead-hub";

/// What one Lead tab's private MCP config wires: the scoped `thegn mcp serve`
/// (carrying the host-minted, scoped control token) and, when the profile names
/// at least one server, the Lead's own `thegn mcp proxy --profile` hub.
#[derive(Debug, Clone, Copy)]
pub struct LeadEntries<'a> {
    /// The thegn binary (`current_exe`).
    pub command: &'a str,
    /// The clamped scope csv (`"none"` when empty: docs tools only).
    pub scopes_csv: &'a str,
    /// `127.0.0.1:<port>` of the daemon's Lead listener.
    pub control_addr: &'a str,
    /// The scoped bearer token (the only secret in the file; carried by the
    /// serve and hub entries).
    pub control_token: &'a str,
    /// Audit-only caller label (`THEGN_CALLER`); never used for authorization.
    pub caller: &'a str,
    /// `[mcp_proxy.profiles]` entry for the hub; `None` = no hub entry.
    pub hub_profile: Option<&'a str>,
}

/// Build the Lead's claude-shaped `{"mcpServers": {...}}` document.
///
/// Unlike [`proxy_entry`] (written into a user's agent settings, so never any
/// `env`), this goes to a private owner-only file only the Lead's agent reads,
/// and its single secret is the scoped Lead token, on the `thegn` entry and the
/// hub entry. The hub fetches its upstreams' env from the daemon with that
/// token (`mcp_proxy.profile_env`).
pub fn lead_entries(e: &LeadEntries<'_>) -> Value {
    let scopes = if e.scopes_csv.is_empty() {
        "none"
    } else {
        e.scopes_csv
    };
    let mut servers = Map::new();
    servers.insert(
        LEAD_SERVE_KEY.to_string(),
        json!({
            "command": e.command,
            // safe for the Lead: the daemon confines Lead input to sessions the Lead opened
            "args": ["mcp", "serve", "--scopes", scopes, "--allow-session-input"],
            "env": {
                "THEGN_CONTROL_ADDR": e.control_addr,
                "THEGN_CONTROL_TOKEN": e.control_token,
                "THEGN_CALLER": e.caller,
            },
            MARKER_KEY: true,
        }),
    );
    if let Some(profile) = e.hub_profile {
        // The profile's server secrets come from the daemon, bound to this
        // Lead token (`mcp_proxy.profile_env`, b155 THE-2222): the hub holds
        // the same scoped credential as the serve entry, never the secrets.
        servers.insert(
            LEAD_HUB_KEY.to_string(),
            json!({
                "command": e.command,
                "args": ["mcp", "proxy", "--profile", profile],
                "env": {
                    "THEGN_CONTROL_ADDR": e.control_addr,
                    "THEGN_CONTROL_TOKEN": e.control_token,
                    "THEGN_CALLER": e.caller,
                },
                MARKER_KEY: true,
            }),
        );
    }
    json!({ "mcpServers": Value::Object(servers) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lead(hub: Option<&'static str>) -> Value {
        lead_entries(&LeadEntries {
            command: "/bin/thegn",
            scopes_csv: "read,write",
            control_addr: "127.0.0.1:4242",
            control_token: "tgc_secret",
            caller: "lead",
            hub_profile: hub,
        })
    }

    #[test]
    fn lead_entries_shape_carries_only_the_scoped_token_env() {
        let doc = lead(Some("lead"));
        let servers = doc["mcpServers"].as_object().unwrap();
        assert_eq!(servers.len(), 2);
        let serve = &servers[LEAD_SERVE_KEY];
        assert_eq!(serve["command"], "/bin/thegn");
        assert_eq!(
            serve["args"],
            json!([
                "mcp",
                "serve",
                "--scopes",
                "read,write",
                "--allow-session-input"
            ])
        );
        let env = serve["env"].as_object().unwrap();
        let mut keys: Vec<&str> = env.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["THEGN_CALLER", "THEGN_CONTROL_ADDR", "THEGN_CONTROL_TOKEN"]
        );
        assert_eq!(env["THEGN_CONTROL_TOKEN"], "tgc_secret");
        let hub = &servers[LEAD_HUB_KEY];
        assert_eq!(hub["args"], json!(["mcp", "proxy", "--profile", "lead"]));
        // The hub carries the same scoped Lead token (it fetches the
        // profile's env from the daemon), and nothing else secret.
        assert_eq!(hub["env"], serve["env"]);
        assert_eq!(doc.to_string().matches("tgc_secret").count(), 2);
    }

    #[test]
    fn lead_entries_omits_the_hub_and_maps_empty_scopes_to_none() {
        let doc = lead_entries(&LeadEntries {
            command: "thegn",
            scopes_csv: "",
            control_addr: "127.0.0.1:1",
            control_token: "t",
            caller: "lead",
            hub_profile: None,
        });
        let servers = doc["mcpServers"].as_object().unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(
            servers[LEAD_SERVE_KEY]["args"],
            json!(["mcp", "serve", "--scopes", "none", "--allow-session-input"])
        );
        assert!(lead(None)["mcpServers"].get(LEAD_HUB_KEY).is_none());
    }

    fn entry() -> Value {
        proxy_entry("thegn")
    }

    #[test]
    fn proxy_entry_is_secret_free() {
        let e = entry();
        assert_eq!(e["command"], "thegn");
        assert_eq!(e["args"], json!(["mcp", "proxy"]));
        assert_eq!(e[MARKER_KEY], true);
        // The credential-custody invariant: never an env block.
        assert!(e.get("env").is_none(), "wired entry must carry no env");
    }

    #[test]
    fn adds_into_empty_settings() {
        let mut s = json!({});
        let out = apply(&mut s, &["mcpServers"], entry(), false).unwrap();
        assert_eq!(out, WireOutcome::Added);
        assert_eq!(s["mcpServers"]["thegn"]["command"], "thegn");
        assert!(s["mcpServers"]["thegn"].get("env").is_none());
    }

    #[test]
    fn creates_missing_container_path() {
        let mut s = json!({ "other": 1 });
        apply(&mut s, &["mcp", "servers"], entry(), false).unwrap();
        assert_eq!(s["mcp"]["servers"]["thegn"]["command"], "thegn");
        assert_eq!(s["other"], 1, "unrelated keys survive");
    }

    #[test]
    fn null_root_becomes_object() {
        let mut s = Value::Null;
        apply(&mut s, &["mcpServers"], entry(), false).unwrap();
        assert!(s["mcpServers"]["thegn"].is_object());
    }

    #[test]
    fn wire_twice_is_one_entry_and_idempotent() {
        let mut s = json!({});
        assert_eq!(
            apply(&mut s, &["mcpServers"], entry(), false).unwrap(),
            WireOutcome::Added
        );
        assert_eq!(
            apply(&mut s, &["mcpServers"], entry(), false).unwrap(),
            WireOutcome::Unchanged
        );
        assert_eq!(s["mcpServers"].as_object().unwrap().len(), 1);
    }

    #[test]
    fn updates_when_argv_changes() {
        let mut s = json!({});
        apply(&mut s, &["mcpServers"], proxy_entry("thegn"), false).unwrap();
        let out = apply(&mut s, &["mcpServers"], proxy_entry("tg"), false).unwrap();
        assert_eq!(out, WireOutcome::Updated);
        assert_eq!(s["mcpServers"]["thegn"]["command"], "tg");
    }

    #[test]
    fn preserves_user_entries() {
        let mut s = json!({ "mcpServers": { "git": { "command": "git-mcp" } } });
        apply(&mut s, &["mcpServers"], entry(), false).unwrap();
        assert_eq!(s["mcpServers"]["git"]["command"], "git-mcp");
        assert_eq!(s["mcpServers"]["thegn"]["command"], "thegn");
    }

    #[test]
    fn refuses_to_overwrite_a_foreign_thegn_entry() {
        let mut s = json!({ "mcpServers": { "thegn": { "command": "not-ours" } } });
        let err = apply(&mut s, &["mcpServers"], entry(), false).unwrap_err();
        assert!(err.contains("refusing to overwrite"), "{err}");
        assert_eq!(s["mcpServers"]["thegn"]["command"], "not-ours");
    }

    #[test]
    fn remove_only_touches_marked_entries() {
        let mut s = json!({
            "mcpServers": {
                "git": { "command": "git-mcp" },
                "thegn": proxy_entry("thegn"),
            }
        });
        let out = apply(&mut s, &["mcpServers"], Value::Null, true).unwrap();
        assert_eq!(out, WireOutcome::Removed);
        assert!(s["mcpServers"].get("thegn").is_none());
        // The user's own entry is untouched.
        assert_eq!(s["mcpServers"]["git"]["command"], "git-mcp");
    }

    #[test]
    fn remove_refuses_foreign_thegn_entry() {
        let mut s = json!({ "mcpServers": { "thegn": { "command": "not-ours" } } });
        let err = apply(&mut s, &["mcpServers"], Value::Null, true).unwrap_err();
        assert!(err.contains("refusing to remove"), "{err}");
        assert_eq!(s["mcpServers"]["thegn"]["command"], "not-ours");
    }

    #[test]
    fn remove_when_absent_is_nothing_to_remove() {
        let mut s = json!({ "mcpServers": {} });
        assert_eq!(
            apply(&mut s, &["mcpServers"], Value::Null, true).unwrap(),
            WireOutcome::NothingToRemove
        );
    }

    #[test]
    fn is_thegn_managed_reads_marker() {
        assert!(is_thegn_managed(&proxy_entry("thegn")));
        assert!(!is_thegn_managed(&json!({ "command": "x" })));
    }

    #[test]
    fn outcome_labels() {
        assert_eq!(WireOutcome::Added.as_str(), "added");
        assert_eq!(WireOutcome::NothingToRemove.as_str(), "nothing to remove");
    }

    // ── serve entry + named apply ───────────────────────────────────────────

    #[test]
    fn wired_entries_are_the_proxy_and_the_serve_server_secret_free() {
        let keys: Vec<&str> = WIRED_ENTRIES.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, vec![ENTRY_KEY, SERVE_ENTRY_KEY]);
        assert_eq!(
            wired_entry("thegn", WIRED_ENTRIES[0].1),
            proxy_entry("thegn")
        );
        assert_eq!(
            wired_entry("thegn", WIRED_ENTRIES[1].1),
            serve_entry("thegn")
        );
        let serve = serve_entry("/bin/thegn");
        assert_eq!(serve["args"], json!(["mcp", "serve"]));
        assert!(serve.get("env").is_none(), "wired entry must carry no env");
        assert!(is_thegn_managed(&serve));
    }

    #[test]
    fn named_apply_keeps_both_entries_and_removes_each_independently() {
        let mut s = json!({ "mcpServers": { "git": { "command": "git-mcp" } } });
        for (key, args) in WIRED_ENTRIES {
            let out = apply_named(
                &mut s,
                &["mcpServers"],
                key,
                wired_entry("thegn", args),
                false,
            )
            .unwrap();
            assert_eq!(out, WireOutcome::Added, "{key}");
        }
        assert_eq!(s["mcpServers"].as_object().unwrap().len(), 3);
        let again = apply_named(
            &mut s,
            &["mcpServers"],
            SERVE_ENTRY_KEY,
            serve_entry("thegn"),
            false,
        )
        .unwrap();
        assert_eq!(again, WireOutcome::Unchanged);
        let out = apply_named(&mut s, &["mcpServers"], SERVE_ENTRY_KEY, Value::Null, true).unwrap();
        assert_eq!(out, WireOutcome::Removed);
        assert!(s["mcpServers"].get(SERVE_ENTRY_KEY).is_none());
        assert!(s["mcpServers"].get(ENTRY_KEY).is_some());
        assert_eq!(s["mcpServers"]["git"]["command"], "git-mcp");
    }

    #[test]
    fn named_apply_refuses_a_foreign_serve_entry() {
        let mut s = json!({ "mcpServers": { "thegn-serve": { "command": "mine" } } });
        let err = apply_named(
            &mut s,
            &["mcpServers"],
            SERVE_ENTRY_KEY,
            serve_entry("thegn"),
            false,
        )
        .unwrap_err();
        assert!(err.contains("`thegn-serve`"), "{err}");
        assert_eq!(s["mcpServers"]["thegn-serve"]["command"], "mine");
    }

    #[test]
    fn remove_never_creates_an_empty_container() {
        let mut s = json!({ "theme": "dark" });
        let out = apply(&mut s, &["mcpServers"], Value::Null, true).unwrap();
        assert_eq!(out, WireOutcome::NothingToRemove);
        assert_eq!(s, json!({ "theme": "dark" }));
    }

    // ── TOML (Codex config.toml) ────────────────────────────────────────────

    fn toml_doc(body: &str) -> toml_edit::DocumentMut {
        body.parse().expect("fixture parses")
    }

    #[test]
    fn toml_wire_adds_marked_argv_only_tables_and_keeps_user_content() {
        let mut doc = toml_doc(
            "# my codex config\nmodel = \"gpt-5\"\n\n[mcp_servers.git]\ncommand = \"git-mcp\"\n",
        );
        for (key, args) in WIRED_ENTRIES {
            let out = apply_toml(&mut doc, "mcp_servers", key, "/bin/thegn", args, false).unwrap();
            assert_eq!(out, WireOutcome::Added, "{key}");
        }
        let text = doc.to_string();
        assert!(
            text.starts_with("# my codex config\nmodel = \"gpt-5\""),
            "{text}"
        );
        assert!(
            text.contains("[mcp_servers.git]\ncommand = \"git-mcp\""),
            "{text}"
        );
        assert!(text.contains(TOML_MARKER), "{text}");
        assert!(!text.contains("env"), "no env block ever: {text}");

        // What Codex will read back.
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        let thegn = &parsed["mcp_servers"]["thegn"];
        assert_eq!(thegn["command"].as_str(), Some("/bin/thegn"));
        assert_eq!(
            thegn["args"].as_array().unwrap().len(),
            2,
            "args = [\"mcp\", \"proxy\"]"
        );
        let serve = &parsed["mcp_servers"]["thegn-serve"];
        assert_eq!(serve["args"][1].as_str(), Some("serve"));
        assert_eq!(parsed["model"].as_str(), Some("gpt-5"));
    }

    #[test]
    fn toml_wire_is_idempotent_across_a_reparse_and_updates_on_change() {
        let mut doc = toml_doc("");
        apply_toml(
            &mut doc,
            "mcp_servers",
            ENTRY_KEY,
            "thegn",
            &["mcp", "proxy"],
            false,
        )
        .unwrap();
        let mut reparsed = toml_doc(&doc.to_string());
        assert_eq!(
            apply_toml(
                &mut reparsed,
                "mcp_servers",
                ENTRY_KEY,
                "thegn",
                &["mcp", "proxy"],
                false
            )
            .unwrap(),
            WireOutcome::Unchanged
        );
        assert_eq!(reparsed.to_string(), doc.to_string(), "byte-identical");
        assert_eq!(
            apply_toml(
                &mut reparsed,
                "mcp_servers",
                ENTRY_KEY,
                "tg",
                &["mcp", "proxy"],
                false
            )
            .unwrap(),
            WireOutcome::Updated
        );
        assert_eq!(
            reparsed.to_string().matches("[mcp_servers.thegn]").count(),
            1
        );
    }

    #[test]
    fn toml_wire_refuses_foreign_tables_and_non_table_containers() {
        let body = "[mcp_servers.thegn]\ncommand = \"mine\"\n";
        let mut doc = toml_doc(body);
        let err = apply_toml(
            &mut doc,
            "mcp_servers",
            ENTRY_KEY,
            "thegn",
            &["mcp", "proxy"],
            false,
        )
        .unwrap_err();
        assert!(err.contains("refusing to overwrite"), "{err}");
        let err = apply_toml(&mut doc, "mcp_servers", ENTRY_KEY, "", &[], true).unwrap_err();
        assert!(err.contains("refusing to remove"), "{err}");
        assert_eq!(doc.to_string(), body);

        let inline = "mcp_servers = { git = { command = \"git-mcp\" } }\n";
        let mut doc = toml_doc(inline);
        let err = apply_toml(
            &mut doc,
            "mcp_servers",
            ENTRY_KEY,
            "thegn",
            &["mcp", "proxy"],
            false,
        )
        .unwrap_err();
        assert!(err.contains("not a TOML table"), "{err}");
        assert_eq!(doc.to_string(), inline);
    }

    #[test]
    fn toml_remove_restores_the_pre_wire_file() {
        let body = "model = \"gpt-5\"\n";
        let mut doc = toml_doc(body);
        assert_eq!(
            apply_toml(&mut doc, "mcp_servers", ENTRY_KEY, "", &[], true).unwrap(),
            WireOutcome::NothingToRemove
        );
        assert_eq!(
            doc.to_string(),
            body,
            "remove on an unwired file is a no-op"
        );
        for (key, args) in WIRED_ENTRIES {
            apply_toml(&mut doc, "mcp_servers", key, "thegn", args, false).unwrap();
        }
        let mut wired = toml_doc(&doc.to_string());
        for (key, _) in WIRED_ENTRIES {
            assert_eq!(
                apply_toml(&mut wired, "mcp_servers", key, "", &[], true).unwrap(),
                WireOutcome::Removed,
                "{key}"
            );
        }
        let parsed: toml::Value = toml::from_str(&wired.to_string()).unwrap();
        assert!(
            parsed
                .get("mcp_servers")
                .and_then(|v| v.as_table())
                .is_none_or(|t| t.is_empty()),
            "{wired}"
        );
        assert_eq!(parsed["model"].as_str(), Some("gpt-5"));
    }

    #[test]
    fn claude_hooks_are_marked_idempotent_reversible_and_keep_user_groups() {
        let user = json!({
            "theme": "dark",
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "echo mine"}]}]}
        });
        let mut s = user.clone();
        assert_eq!(
            apply_claude_hooks(&mut s, "/bin/thegn", false),
            Ok(WireOutcome::Added)
        );
        assert_eq!(s["theme"], "dark");
        assert_eq!(s["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert_eq!(s["hooks"]["Stop"][0]["hooks"][0]["command"], "echo mine");
        assert_eq!(
            s["hooks"]["Notification"][0]["hooks"][0]["command"],
            "/bin/thegn agent hook claude attention # x-thegn-managed"
        );
        let before = s.clone();
        assert_eq!(
            apply_claude_hooks(&mut s, "/bin/thegn", false),
            Ok(WireOutcome::Unchanged)
        );
        assert_eq!(s, before);
        assert_eq!(
            apply_claude_hooks(&mut s, "/opt/other/thegn", false),
            Ok(WireOutcome::Updated)
        );
        assert_eq!(
            apply_claude_hooks(&mut s, "/x", true),
            Ok(WireOutcome::Removed)
        );
        assert_eq!(s, user);
    }

    #[test]
    fn claude_hooks_remove_on_unwired_is_a_noop_and_wrong_types_are_refused() {
        let mut s = json!({"theme": "dark"});
        assert_eq!(
            apply_claude_hooks(&mut s, "t", true),
            Ok(WireOutcome::NothingToRemove)
        );
        assert_eq!(s, json!({"theme": "dark"}));
        let mut bad = json!({"hooks": {"Stop": "nope"}});
        assert!(apply_claude_hooks(&mut bad, "t", false).is_err());
        assert_eq!(bad, json!({"hooks": {"Stop": "nope"}}));
        let mut bad = json!({"hooks": []});
        assert!(apply_claude_hooks(&mut bad, "t", false).is_err());
    }

    #[test]
    fn claude_hook_command_quotes_a_path_with_spaces() {
        assert_eq!(
            claude_hook_command("/a b/thegn", "stop"),
            "'/a b/thegn' agent hook claude stop # x-thegn-managed"
        );
    }

    #[test]
    fn codex_notify_is_written_once_and_never_overwrites_a_user_notifier() {
        let mut doc: toml_edit::DocumentMut = "# mine\nmodel = \"x\"\n".parse().unwrap();
        assert_eq!(
            apply_codex_notify(&mut doc, "/bin/thegn", false),
            Ok(WireOutcome::Added)
        );
        let text = doc.to_string();
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(parsed["notify"][3].as_str(), Some("codex"));
        assert!(text.starts_with("# mine\n"));
        assert_eq!(
            apply_codex_notify(&mut doc, "/bin/thegn", false),
            Ok(WireOutcome::Unchanged)
        );
        assert_eq!(doc.to_string(), text);
        assert_eq!(
            apply_codex_notify(&mut doc, "/bin/thegn", true),
            Ok(WireOutcome::Removed)
        );
        assert_eq!(doc.to_string(), "# mine\nmodel = \"x\"\n");

        let mut theirs: toml_edit::DocumentMut = "notify = [\"my-notifier\"]\n".parse().unwrap();
        assert!(apply_codex_notify(&mut theirs, "/bin/thegn", false).is_err());
        assert!(apply_codex_notify(&mut theirs, "/bin/thegn", true).is_err());
        assert_eq!(theirs.to_string(), "notify = [\"my-notifier\"]\n");
        let mut none: toml_edit::DocumentMut = "".parse().unwrap();
        assert_eq!(
            apply_codex_notify(&mut none, "t", true),
            Ok(WireOutcome::NothingToRemove)
        );
    }

    // --- sandbox hooks ------------------------------------------------------

    #[test]
    fn sandbox_claude_hooks_replace_host_path_and_are_idempotent() {
        let mut s = json!({"theme": "dark"});
        assert_eq!(
            apply_claude_hooks(&mut s, "/home/u/thegn", false),
            Ok(WireOutcome::Added)
        );
        assert_eq!(
            apply_sandbox_claude_hooks(&mut s, false),
            Ok(WireOutcome::Updated)
        );
        assert!(!s.to_string().contains("/home/u"), "{s}");
        assert!(s.to_string().contains(HOOK_MARKER));
        assert_eq!(
            apply_sandbox_claude_hooks(&mut s, false),
            Ok(WireOutcome::Unchanged)
        );
        assert_eq!(
            apply_sandbox_claude_hooks(&mut s, true),
            Ok(WireOutcome::Removed)
        );
        assert_eq!(s, json!({"theme": "dark"}));
    }

    #[test]
    fn sandbox_codex_notify_replaces_host_path_and_refuses_user_notify() {
        let mut doc: toml_edit::DocumentMut = "".parse().unwrap();
        apply_codex_notify(&mut doc, "/home/u/thegn", false).unwrap();
        assert_eq!(
            apply_sandbox_codex_notify(&mut doc, false),
            Ok(WireOutcome::Updated)
        );
        assert!(!doc.to_string().contains("/home/u"), "{doc}");
        assert_eq!(
            apply_sandbox_codex_notify(&mut doc, false),
            Ok(WireOutcome::Unchanged)
        );
        let mut mine: toml_edit::DocumentMut = "notify = [\"mine\"]\n".parse().unwrap();
        assert!(apply_sandbox_codex_notify(&mut mine, false).is_err());
        assert_eq!(mine.to_string(), "notify = [\"mine\"]\n");
    }

    // --- YAML (Hermes) ------------------------------------------------------

    fn yaml_wire(text: &str) -> String {
        let (out, outcomes) = apply_all(
            WireFormat::Yaml,
            text,
            &["mcp_servers"],
            "/bin/thegn",
            &[],
            false,
        )
        .unwrap();
        assert!(
            outcomes.iter().all(|(_, o)| *o == WireOutcome::Added),
            "{outcomes:?}"
        );
        out
    }

    #[test]
    fn yaml_wire_creates_the_section_and_removes_it_again() {
        let user = "model: hermes-3\n# my note\nagent:\n  verbose: true\n";
        let wired = yaml_wire(user);
        assert!(wired.starts_with(user), "{wired}");
        assert!(wired.contains("\nmcp_servers:\n"), "{wired}");
        assert!(
            wired.contains("  thegn-serve:\n    command: \"/bin/thegn\"\n"),
            "{wired}"
        );
        assert!(wired.contains("args: [\"mcp\", \"proxy\"]"), "{wired}");
        assert!(!wired.contains("env"), "{wired}");
        let (back, outcomes) =
            apply_all(WireFormat::Yaml, &wired, &["mcp_servers"], "x", &[], true).unwrap();
        assert!(
            outcomes.iter().all(|(_, o)| *o == WireOutcome::Removed),
            "{outcomes:?}"
        );
        assert_eq!(back, user);
    }

    #[test]
    fn yaml_wire_is_idempotent_and_updates_a_changed_command() {
        let wired = yaml_wire("");
        let (again, outcomes) = apply_all(
            WireFormat::Yaml,
            &wired,
            &["mcp_servers"],
            "/bin/thegn",
            &[],
            false,
        )
        .unwrap();
        assert!(
            outcomes.iter().all(|(_, o)| *o == WireOutcome::Unchanged),
            "{outcomes:?}"
        );
        assert_eq!(again, wired);
        let (moved, outcomes) = apply_all(
            WireFormat::Yaml,
            &wired,
            &["mcp_servers"],
            "/opt/thegn",
            &[],
            false,
        )
        .unwrap();
        assert!(
            outcomes.iter().all(|(_, o)| *o == WireOutcome::Updated),
            "{outcomes:?}"
        );
        assert!(
            moved.contains("/opt/thegn") && !moved.contains("/bin/thegn"),
            "{moved}"
        );
        assert_eq!(moved.matches("x-thegn-managed").count(), 2, "{moved}");
    }

    #[test]
    fn yaml_wire_keeps_user_servers_and_trailing_keys() {
        let user =
            "mcp_servers:\n  git:\n    command: \"git-mcp\"\n    args: []\n# tail\nother: 1\n";
        let wired = yaml_wire(user);
        assert!(
            wired.contains("  git:\n    command: \"git-mcp\"\n    args: []\n"),
            "{wired}"
        );
        assert!(wired.ends_with("# tail\nother: 1\n"), "{wired}");
        let thegn = wired.find("  thegn:").unwrap();
        assert!(thegn < wired.find("# tail").unwrap(), "{wired}");
        let (back, _) =
            apply_all(WireFormat::Yaml, &wired, &["mcp_servers"], "x", &[], true).unwrap();
        assert_eq!(back, user);
    }

    #[test]
    fn yaml_refuses_foreign_entries_and_unsafe_shapes_without_changes() {
        let foreign = "mcp_servers:\n  thegn:\n    command: mine\n";
        let err = apply_yaml(foreign, "mcp_servers", "thegn", "t", &[], false).unwrap_err();
        assert!(err.contains("refusing to overwrite"), "{err}");
        let err = apply_yaml(foreign, "mcp_servers", "thegn", "t", &[], true).unwrap_err();
        assert!(err.contains("refusing to remove"), "{err}");
        for bad in [
            "mcp_servers: {}\n",
            "mcp_servers:\n  - a\n",
            "a: 1\r\nb: 2\r\n",
        ] {
            assert!(
                apply_yaml(bad, "mcp_servers", "thegn", "t", &[], false).is_err(),
                "{bad}"
            );
        }
        let (text, out) = apply_yaml("a: 1\n", "mcp_servers", "thegn", "t", &[], true).unwrap();
        assert_eq!(
            (text.as_str(), out),
            ("a: 1\n", WireOutcome::NothingToRemove)
        );
    }

    #[test]
    fn sandbox_prefix_yields_a_resolver_entry_in_every_format() {
        for (format, container) in [
            (WireFormat::Json, &["mcpServers"][..]),
            (WireFormat::Toml, &["mcp_servers"][..]),
            (WireFormat::Yaml, &["mcp_servers"][..]),
        ] {
            let (text, outcomes) = apply_all(
                format,
                "",
                container,
                SANDBOX_COMMAND,
                SANDBOX_PREFIX,
                false,
            )
            .unwrap();
            assert!(outcomes.iter().all(|(_, o)| *o == WireOutcome::Added));
            assert!(text.contains("/bin/sh"), "{text}");
            assert!(text.contains("/var/tmp/thegn-bridge"), "{text}");
            assert!(
                !text.contains("/home/"),
                "no host path in a sandbox entry: {text}"
            );
            let (again, outcomes) = apply_all(
                format,
                &text,
                container,
                SANDBOX_COMMAND,
                SANDBOX_PREFIX,
                false,
            )
            .unwrap();
            assert!(
                outcomes.iter().all(|(_, o)| *o == WireOutcome::Unchanged),
                "{format:?}"
            );
            assert_eq!(again, text);
        }
    }

    #[test]
    fn host_entry_is_rewritten_to_the_sandbox_resolver() {
        // A host-wired settings file carried into a sandbox keeps the host path
        // until the sandbox pass rewrites the thegn-marked entries in place.
        let (host, _) = apply_all(
            WireFormat::Json,
            "{}",
            &["mcpServers"],
            "/home/u/thegn",
            &[],
            false,
        )
        .unwrap();
        let (sandbox, outcomes) = apply_all(
            WireFormat::Json,
            &host,
            &["mcpServers"],
            SANDBOX_COMMAND,
            SANDBOX_PREFIX,
            false,
        )
        .unwrap();
        assert!(outcomes.iter().all(|(_, o)| *o == WireOutcome::Updated));
        assert!(!sandbox.contains("/home/u"), "{sandbox}");
    }
}
