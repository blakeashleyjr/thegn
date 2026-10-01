//! `thegn config <action>` — inspect/edit the effective (layered) config.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Command;
use thegn_core::config::Config;
use thegn_core::{msg, outln, util};

/// The committed example, seeded on first `config edit`.
const EXAMPLE: &str = include_str!("../../../../config/config.toml.example");

/// Config subcommands, mirroring the legacy `ConfigAction`.
#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// Print the path to the config file.
    Path,
    /// Print the effective merged config (defaults < file < env < flags).
    Show {
        #[arg(long)]
        json: bool,
    },
    /// Print a single value by dotted key (bare value; for scripts).
    Get {
        key: String,
        #[arg(long)]
        json: bool,
    },
    /// Open the config file in $EDITOR (seeds from the example if missing).
    Edit,
    /// Set one dotted key (`config set sandbox.backend docker`) in the config
    /// file, preserving comments/formatting. The write counterpart to `get`.
    Set { key: String, value: String },
    /// Strictly validate the config file and active overlays; non-zero exit on
    /// any problem.
    Validate {
        /// Validate the repo-local overlay for this repository instead of the
        /// repository containing the current directory.
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Print the JSON schema for editor autocomplete and validation.
    Schema,
    /// Explain how a key resolves: effective value, which layer set it, and (for
    /// `sandbox.*` with `--repo`) the trust clamp trace (denials + pending).
    Explain {
        key: String,
        /// Also show the repo `.thegn.*` clamp trace for this repo path.
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

pub fn run(
    cfg: &Config,
    action: Action,
    path: PathBuf,
    repo_context: Option<PathBuf>,
) -> Result<()> {
    match action {
        Action::Path => print_path(&path),
        Action::Show { json } => show(cfg, json)?,
        Action::Get { key, json } => get(cfg, &key, json, &path)?,
        Action::Edit => edit(cfg, &path)?,
        Action::Set { key, value } => {
            // Capture the prior file so a bad write can be rolled back: a mistyped
            // value for a typed field would otherwise make the WHOLE config
            // unparseable, silently reverting every setting to defaults on the
            // next load. Re-validate after writing and restore on failure.
            let prior = std::fs::read(&path).ok(); // best-effort: optional input: a missing file just means nothing to roll back
            thegn_core::config_write::set_key(&path, &key, &value)
                .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
            let written = std::fs::read_to_string(&path).unwrap_or_default();
            // Runtime loading normalizes accepted aliases/collisions before
            // serde. Raw serde here would reject an otherwise valid canonical-
            // wins collision, even for an unrelated edit.
            let parse_err = thegn_core::config_compat::normalize(&written)
                .and_then(|normalized| {
                    toml::from_str::<Config>(&normalized.body).map_err(|e| e.to_string())
                })
                .err();
            let diagnostics = thegn_core::config_validate::validate_diagnostics(&written);
            let enum_errs: Vec<String> = diagnostics
                .iter()
                .filter(|d| d.severity == thegn_core::config_validate::ValidationSeverity::Error)
                .map(|d| d.message.clone())
                .collect();
            // Only enum errors this write INTRODUCED should roll it back. A stale
            // bad value in some OTHER (now-covered) key was already
            // warn-defaulting on every load — refusing to set an unrelated key
            // because of it (and blaming the key just set) is a false rejection.
            // Diff against the prior file's errors so pre-existing problems don't
            // block an unrelated `config set`.
            let prior_errs: std::collections::HashSet<String> = prior
                .as_deref()
                .and_then(|b| std::str::from_utf8(b).ok())
                .map(thegn_core::config::validate_str)
                .unwrap_or_default()
                .into_iter()
                .collect();
            let new_enum_errs: Vec<&String> = enum_errs
                .iter()
                .filter(|e| !prior_errs.contains(*e))
                .collect();
            if parse_err.is_some() || !new_enum_errs.is_empty() {
                // Roll back to exactly the prior state (bytes, or remove the file
                // if we created it) so the user's config is never left broken.
                match &prior {
                    Some(bytes) => {
                        let _ = std::fs::write(&path, bytes); // best-effort: rollback after a failed validation; the original error is reported below
                    }
                    None => {
                        let _ = std::fs::remove_file(&path); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
                    }
                }
                if let Some(e) = parse_err {
                    anyhow::bail!(
                        "{}: {key} = {value:?} would make the config unparseable ({e}); not written",
                        path.display()
                    );
                }
                for e in &new_enum_errs {
                    msg::error(&format!("{}: {e}", path.display()));
                }
                anyhow::bail!(
                    "{}: {key} = {value:?} is invalid ({} problem(s)); not written",
                    path.display(),
                    new_enum_errs.len(),
                );
            }
            for diagnostic in diagnostics
                .iter()
                .filter(|d| d.severity == thegn_core::config_validate::ValidationSeverity::Warning)
            {
                msg::warn(&format!("{}: {}", path.display(), diagnostic.message));
            }
            // Echo what was actually WRITTEN, not the raw argument: an array
            // argument is now written as a real TOML array, and debug-quoting it
            // made the confirmation look like it had been stored as a string.
            let written_value = written
                .lines()
                .rev()
                .find_map(|l| {
                    let (k, v) = l.split_once('=')?;
                    (k.trim() == key.rsplit('.').next().unwrap_or(key.as_str()))
                        .then(|| v.trim().to_string())
                })
                .unwrap_or_else(|| format!("{value:?}"));
            outln!("set {key} = {written_value} in {}", path.display());
            // The write is valid, but the file still carries pre-existing bad
            // values in other keys — surface them so they don't linger unnoticed
            // (they were already warn-defaulting on every load; not the fault of
            // this set, so they don't block it).
            if !enum_errs.is_empty() {
                msg::warn(&format!(
                    "note: {} pre-existing problem(s) remain in {} — run `thegn config validate`",
                    enum_errs.len(),
                    path.display()
                ));
            }
        }
        Action::Validate { repo } => validate(
            &path,
            repo.or_else(|| repo_context.as_deref().map(Path::to_path_buf)),
        )?,
        Action::Schema => print_schema(),
        Action::Explain { key, repo, json } => explain(cfg, &key, repo, json, path)?,
    }
    Ok(())
}

/// Format a path without opening, canonicalizing or loading its configuration.
pub(crate) fn print_path(path: &Path) {
    outln!("{}", path.display());
}

/// Generate the schema from the type, without constructing effective configuration.
pub(crate) fn print_schema() {
    let schema = schemars::schema_for!(Config);
    outln!("{}", serde_json::to_string_pretty(&schema).unwrap());
}

fn explain(cfg: &Config, key: &str, repo: Option<String>, json: bool, path: PathBuf) -> Result<()> {
    use thegn_core::config::ProcessEnv;
    use thegn_core::config_resolve;
    let mut origin = config_resolve::explain(&ProcessEnv, &[], Some(path), key)
        .map_err(|error| anyhow::anyhow!("config explain: {error}"))?;
    // Every value this verb prints (effective, each trace layer, the workspace
    // layer) is redacted by the same policy as `config get`.
    redact_origin(&mut origin);
    // The per-repo layers are NOT part of the preference cascade `explain`
    // replays, so without this the trace confidently reported the global value
    // for a key a `[workspace.<slug>]` block had already overridden — the probe
    // lied. Default the repo to the cwd's, so running `config explain` inside a
    // repo tells the truth without needing to remember `--repo`.
    let repo_root = repo
        .clone()
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .and_then(|p| thegn_core::repo::main_worktree(&p));
    let ws = repo_root
        .as_ref()
        .and_then(|root| workspace_layer(cfg, root, key))
        .map(|(slug, v)| (slug, redact_at(&origin.key, v)));
    // THE-515: an ambiguous trusted overlay is refused, never silently
    // skipped — explain must say which block and why.
    let refused = repo_root
        .as_ref()
        .and_then(|root| cfg.workspace_overlay_refusal(root));
    if json {
        let mut obj = serde_json::json!({
            "key": origin.key,
            "value": ws.as_ref().map_or(origin.value.clone(), |(_, v)| v.clone()),
            "origin": ws.as_ref().map_or(origin.origin.as_str().to_string(), |(s, _)| {
                format!("project [project.{s}]")
            }),
            "cascade_value": origin.value,
        });
        if let Some(refusal) = &refused {
            obj["workspace_refused"] = serde_json::json!(refusal.to_string());
        }
        if let Some(repo) = &repo {
            let (events, pending) = repo_clamp(cfg, repo, key);
            obj["clamped"] = serde_json::json!(events);
            obj["pending"] = serde_json::json!(pending);
        }
        outln!("{}", serde_json::to_string_pretty(&obj)?);
        return Ok(());
    }
    match &ws {
        Some((slug, v)) => {
            outln!("{} = {v}", origin.key);
            outln!("  set by: project `[project.{slug}]`");
        }
        None => {
            outln!("{} = {}", origin.key, origin.value);
            outln!("  set by: {}", origin.origin.as_str());
        }
    }
    for (layer, val) in &origin.trace {
        outln!("    {}: {val}", layer.as_str());
    }
    if let Some(refusal) = &refused {
        outln!("  REFUSED {refusal}");
    }
    if let (Some((slug, v)), Some(root)) = (&ws, &repo_root) {
        outln!(
            "    project `[project.{slug}]`: {v}   (for {})",
            root.display()
        );
    }
    if let Some(repo) = &repo {
        let (events, pending) = repo_clamp(cfg, repo, key);
        if !events.is_empty() || !pending.is_empty() {
            outln!("  repo `.thegn.*` clamp ({repo}):");
            for line in events {
                outln!("    {line}");
            }
            for p in pending {
                outln!("    pending: {p}");
            }
        }
    }
    Ok(())
}

/// The `[workspace.<slug>]` layer's value for `key` in this repo, when it
/// differs from the plain cascade — plus the slug, so the trace can name the
/// exact block the user has to edit.
///
/// Only the two queue families (`merge_queue.*` / `pr_queue.*`) are carried by
/// that layer today; other keys return `None` and explain as before.
fn workspace_layer(
    cfg: &Config,
    repo_root: &std::path::Path,
    key: &str,
) -> Option<(String, serde_json::Value)> {
    let thegn_core::workspace_overlay::WorkspaceOverlay::Selected {
        key: slug,
        overlay: ws,
    } = cfg.workspace_overlay(repo_root)
    else {
        return None;
    };

    // Each arm: the sub-key, whether this repo overlays that family at all, and
    // the resolved-vs-global pair to diff.
    let (sub, resolved, global) = if let Some(sub) = key.strip_prefix("merge_queue.") {
        (!ws.merge_queue.is_empty()).then_some(())?;
        (
            sub,
            serde_json::to_value(cfg.repo_merge_queue(repo_root)).ok()?,
            serde_json::to_value(&cfg.merge_queue).ok()?,
        )
    } else {
        // `?` rather than an `else { return None }` arm: a key in neither family
        // simply isn't carried by this layer.
        let sub = key.strip_prefix("pr_queue.")?;
        (!ws.pr_queue.is_empty()).then_some(())?;
        (
            sub,
            serde_json::to_value(cfg.repo_pr_queue(repo_root)).ok()?,
            serde_json::to_value(&cfg.pr_queue).ok()?,
        )
    };

    let v = resolved.get(sub)?;
    (v != global.get(sub)?).then(|| (slug.to_string(), v.clone()))
}

/// Repo-overlay clamp events + pending summaries filtered to a key prefix, using
/// the persisted trust approvals.
fn repo_clamp(cfg: &Config, repo: &str, key: &str) -> (Vec<String>, Vec<String>) {
    use thegn_core::config_resolve::{Approvals, summarize_events};
    use thegn_core::db::Db;
    use thegn_core::store::RepoTrustStore;
    let root = thegn_core::repo::main_worktree(std::path::Path::new(repo))
        .unwrap_or_else(|| PathBuf::from(repo));
    let approvals = Db::open()
        .ok()
        .and_then(|db| db.repo_trust_approved(&root.to_string_lossy()).ok())
        .map(Approvals::from_canonical)
        .unwrap_or_else(Approvals::deny_all);
    let resolved = cfg.repo_sandbox_resolved(&root, &approvals);
    let events = summarize_events(&resolved.events)
        .into_iter()
        .filter(|l| l.contains(key) || key == "sandbox")
        .collect();
    let pending = resolved
        .pending
        .into_iter()
        .filter(|p| p.key.contains(key) || key == "sandbox")
        .map(|p| format!("{}: {}", p.key, p.summary))
        .collect();
    (events, pending)
}

fn show(cfg: &Config, json: bool) -> Result<()> {
    thegn_core::out!("{}", show_text(cfg, json)?);
    Ok(())
}

/// The effective config as printed by `config show`, with every secret masked
/// by the canonical redactor (tracker / forge / model-proxy credentials all
/// survive into the effective config, so this must never serialize `cfg`
/// directly).
fn show_text(cfg: &Config, json: bool) -> Result<String> {
    if json {
        let mut value = serde_json::to_value(cfg)?;
        thegn_core::redact::redact_json(&mut value);
        return Ok(format!("{}\n", serde_json::to_string_pretty(&value)?));
    }
    // TOML text is redacted on the toml_edit tree (not via a JSON round-trip)
    // so the typed `Config`'s field order — which the maps in serde_json /
    // toml::Value would sort alphabetically — is preserved.
    let mut doc: toml_edit::DocumentMut = toml::to_string_pretty(cfg)?.parse()?;
    redact_toml_table(doc.as_table_mut());
    Ok(doc.to_string())
}

/// The same policy as [`thegn_core::redact::redact_json`] (strings and arrays
/// of strings under a sensitive key become the placeholder; tables recurse),
/// using the canonical `is_sensitive` predicate.
fn redact_toml_table(table: &mut toml_edit::Table) {
    for (key, item) in table.iter_mut() {
        redact_toml_item(item, thegn_core::redact::is_sensitive(key.get()));
    }
}

fn redact_toml_item(item: &mut toml_edit::Item, masked: bool) {
    match item {
        toml_edit::Item::Table(t) => redact_toml_table(t),
        toml_edit::Item::ArrayOfTables(a) => a.iter_mut().for_each(redact_toml_table),
        toml_edit::Item::Value(v) => redact_toml_value(v, masked),
        toml_edit::Item::None => {}
    }
}

fn redact_toml_value(value: &mut toml_edit::Value, masked: bool) {
    match value {
        toml_edit::Value::String(_) if masked => {
            *value = toml_edit::Value::from(thegn_core::redact::PLACEHOLDER);
        }
        toml_edit::Value::Array(a) => a.iter_mut().for_each(|v| redact_toml_value(v, masked)),
        toml_edit::Value::InlineTable(t) => {
            for (key, v) in t.iter_mut() {
                redact_toml_value(v, thegn_core::redact::is_sensitive(key.get()));
            }
        }
        _ => {}
    }
}

/// Mask `value` as the value of config key `key`: a sensitive final segment
/// masks the strings under it; objects are recursed.
fn redact_at(key: &str, value: serde_json::Value) -> serde_json::Value {
    let last = key.rsplit('.').next().unwrap_or(key);
    let mut wrapped = serde_json::json!({ last: value });
    thegn_core::redact::redact_json(&mut wrapped);
    wrapped
        .get_mut(last)
        .map(serde_json::Value::take)
        .unwrap_or(serde_json::Value::Null)
}

fn redact_origin(origin: &mut thegn_core::config_resolve::KeyOrigin) {
    let key = origin.key.clone();
    origin.value = redact_at(&key, origin.value.take());
    for (_, v) in &mut origin.trace {
        *v = redact_at(&key, v.take());
    }
}

fn get(cfg: &Config, key: &str, json: bool, path: &Path) -> Result<()> {
    if let Some(notice) = clamp_notice(key, &crate::channel_state::clamped_features()) {
        // Keep stdout machine-readable: this notice is intentionally stderr-only.
        msg::warn(&notice);
    }
    if json {
        // Emit the value's REAL type (number, bool, array, table) rather than a
        // stringified scalar, so `config get --json` composes with `jq`.
        return match redacted_value_at(cfg, key) {
            Some(v) => {
                outln!("{}", serde_json::to_string(&v)?);
                Ok(())
            }
            None => anyhow::bail!(
                "unknown config key: {key} (effective config: {})",
                path.display()
            ),
        };
    }
    let plain = get_text(cfg, key);
    match plain {
        Some(v) => {
            outln!("{v}");
            Ok(())
        }
        None => anyhow::bail!(
            "unknown config key: {key} (effective config: {})",
            path.display()
        ),
    }
}

/// The text-mode rendering of `config get <key>`. Anything structured (a table
/// or array) and anything under a sensitive key goes through the redactor; the
/// hand-mapped `get_dotted` renderer (display-form enums, `repo_roots` joins,
/// computed defaults such as `log.dir`) is used only for non-sensitive scalars
/// and for keys the JSON tree does not carry.
fn get_text(cfg: &Config, key: &str) -> Option<String> {
    let canonical = thegn_core::config_compat::canonical_key(key);
    match redacted_value_at(cfg, &canonical) {
        Some(v @ (serde_json::Value::Object(_) | serde_json::Value::Array(_))) => {
            Some(render_config_value(v))
        }
        Some(v) => {
            let last = canonical.rsplit('.').next().unwrap_or(&canonical);
            if thegn_core::redact::is_sensitive(last) {
                Some(render_config_value(v))
            } else {
                cfg.get_dotted(&canonical)
                    .or_else(|| Some(render_config_value(v)))
            }
        }
        None => {
            let last = canonical.rsplit('.').next().unwrap_or(&canonical);
            if thegn_core::redact::is_sensitive(last) {
                None
            } else {
                cfg.get_dotted(&canonical)
            }
        }
    }
}

/// Resolve a config value for user-facing output and mask secret-bearing keys.
/// Redact the selected subtree (for `config get issues`) and also inspect the
/// final dotted key (for `config get issues.issue_accounts.0.token`).
fn redacted_value_at(cfg: &Config, key: &str) -> Option<serde_json::Value> {
    let canonical = thegn_core::config_compat::canonical_key(key);
    Some(redact_at(&canonical, cfg.value_at(key)?))
}

fn render_config_value(value: serde_json::Value) -> String {
    match value {
        serde_json::Value::String(value) => value,
        serde_json::Value::Null => String::new(),
        serde_json::Value::Array(values) => values
            .into_iter()
            .map(render_config_value)
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    }
}

fn clamp_notice(key: &str, clamped: &[thegn_core::channel::Feature]) -> Option<String> {
    use thegn_core::channel::Feature;
    let mut parts = key.split('.');
    let root = parts.next()?;
    let feature = match (root, parts.next()) {
        ("sandbox", Some("remote")) => Feature::Remote,
        ("host", _) => Feature::Providers,
        ("observe", _) => Feature::Observe,
        ("placement", _) => Feature::Placement,
        ("voice", _) => Feature::Voice,
        _ => return None,
    };
    clamped.contains(&feature).then(|| {
        format!(
            "configuration for [{}] was disabled by the stable channel; set THEGN_CHANNEL=dev to enable {}",
            if feature == Feature::Remote { "sandbox.remote" } else { root },
            feature.id()
        )
    })
}

fn edit(cfg: &Config, path: &PathBuf) -> Result<()> {
    if !path.exists() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, EXAMPLE)?;
        msg::info(&format!("seeded {} from the example", path.display()));
    }
    // The editor seam: `[editor] command` → `[[tools]] editor` → $VISUAL/$EDITOR
    // → vi.
    let path_str = path.to_string_lossy();
    let req = thegn_core::editor::OpenRequest {
        path: &path_str,
        line: None,
        col: None,
    };
    let launch = thegn_core::editor::editor_for(cfg)
        .open(&req)
        .unwrap_or_else(|_| thegn_core::editor::launch_line("vi", &req));
    // CLI path: `thegn config edit` hands the terminal to the editor, no event loop.
    #[expect(clippy::disallowed_methods)]
    let status = Command::new(util::shell())
        .arg("-lc")
        .arg(&launch.command)
        .status()?;
    if !status.success() {
        anyhow::bail!("editor exited with status {status}");
    }
    Ok(())
}

fn validate(path: &Path, repo_context: Option<PathBuf>) -> Result<()> {
    let health = super::config_health::collect(path, repo_context.as_deref());
    super::config_health::render_findings(&health);

    if !health.main_present {
        outln!("no config file at {} — using defaults (ok)", path.display());
    } else if health.main_problems == 0 {
        outln!("{} ok", path.display());
    }
    if let Some(profile) = &health.profile_path
        && health.profile_problems == 0
    {
        outln!("{} ok", profile.display());
    }
    if let Some(repo) = &health.repo_path
        && health.repo_problems == 0
    {
        outln!("{} ok", repo.display());
    }
    if health.problems() == 0 {
        Ok(())
    } else {
        anyhow::bail!("{} problem(s) in configuration layers", health.problems());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_accepts_advisories_without_rewriting_and_rejects_real_errors() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("private-config");
        let _env = thegn_core::testenv::EnvGuard::set(&[(
            "XDG_CONFIG_HOME",
            config_dir.to_str().unwrap(),
        )]);
        let path = dir.path().join("config.toml");
        let body = "workspaces_dir = '/legacy'\n[workspace.repo]\n";
        std::fs::write(&path, body).unwrap();
        assert!(validate(&path, Some(dir.path().to_owned())).is_ok());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
        std::fs::write(&path, "workspaces_dir = '/legacy'\npicker = 42\n").unwrap();
        let error = validate(&path, Some(dir.path().to_owned())).unwrap_err();
        assert!(error.to_string().contains("1 problem(s)"), "{error}");
    }

    #[test]
    fn config_set_does_not_count_compatibility_warnings_as_new_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "projects_dir = '/canonical'\nworkspaces_dir = '/legacy'\n",
        )
        .unwrap();
        run(
            &Config::default(),
            Action::Set {
                key: "picker".into(),
                value: "auto".into(),
            },
            path.clone(),
            Some(dir.path().to_owned()),
        )
        .unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(thegn_core::config::validate_str(&written).is_empty());
        let normalized = thegn_core::config_compat::normalize(&written).unwrap();
        let config: Config = toml::from_str(&normalized.body).unwrap();
        assert_eq!(config.workspaces_dir, "/canonical");
        assert!(
            run(
                &Config::default(),
                Action::Set {
                    key: "picker".into(),
                    value: "42".into()
                },
                path.clone(),
                Some(dir.path().to_owned()),
            )
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), written);
    }

    #[test]
    fn show_outputs_toml_and_json_without_panicking() {
        let cfg = Config::default();
        assert!(show(&cfg, false).is_ok());
        assert!(show(&cfg, true).is_ok());
    }

    #[test]
    fn get_known_and_unknown_keys() {
        let cfg = Config::default();
        let path = PathBuf::from("/tmp/config.toml");
        assert!(get(&cfg, "picker", false, &path).is_ok());
        assert!(get(&cfg, "picker", true, &path).is_ok());
        assert!(get(&cfg, "nonexistent.key", false, &path).is_err());
        assert!(get(&cfg, "nonexistent.key", true, &path).is_err());
    }

    #[test]
    fn get_reaches_nested_keys_the_allowlist_never_listed() {
        // The regression: every `[merge_queue]` key (and the whole nested
        // surface) reported "unknown config key" while `config explain`
        // resolved the same dotted path fine.
        let cfg = Config::default();
        for key in [
            "merge_queue.on_landed",
            "merge_queue.gate_command",
            "merge_queue.auto_land",
            "merge_queue.regenerate_paths",
            "ui.language",
        ] {
            assert!(
                get(&cfg, key, false, &PathBuf::from("/tmp/config.toml")).is_ok(),
                "config get {key} should resolve"
            );
            assert!(
                get(&cfg, key, true, &PathBuf::from("/tmp/config.toml")).is_ok(),
                "config get --json {key}"
            );
        }
    }

    #[test]
    fn config_get_notice_is_section_scoped_and_keeps_json_value_shape() {
        // The caller emits this notice through msg::warn (stderr); the value
        // continues through the existing serializer unchanged on stdout.
        use thegn_core::channel::Feature;
        let clamped = [Feature::Remote, Feature::Providers];
        assert!(clamp_notice("host.gpu.command", &clamped).is_some());
        assert!(clamp_notice("sandbox.remote.host", &clamped).is_some());
        assert_eq!(clamp_notice("ui.language", &clamped), None);
        assert_eq!(clamp_notice("issues.provider", &clamped), None);
        let cfg = Config::default();
        let json = serde_json::to_value(cfg.value_at("issues").unwrap()).unwrap();
        assert!(json.is_object());
        assert_eq!(json["provider"], "none");
    }

    #[test]
    fn config_get_redacts_issue_account_tokens_in_nested_and_direct_values() {
        let mut cfg = Config::default();
        cfg.issues
            .issue_accounts
            .push(thegn_core::config::IssueAccount {
                name: "linear-work".into(),
                provider: thegn_core::config::IssueProviderKind::Linear,
                token: "THE695_SYNTHETIC_CANARY".into(),
                ..Default::default()
            });
        let issues = redacted_value_at(&cfg, "issues").unwrap();
        assert_eq!(
            issues["issue_accounts"][0]["token"],
            thegn_core::redact::PLACEHOLDER
        );
        assert!(!issues.to_string().contains("THE695_SYNTHETIC_CANARY"));

        let token = redacted_value_at(&cfg, "issues.issue_accounts.0.token").unwrap();
        assert_eq!(token, thegn_core::redact::PLACEHOLDER);
        assert!(!token.to_string().contains("THE695_SYNTHETIC_CANARY"));
    }

    /// The pipeline org chart is read WHOLE by the supervising agent
    /// (`thegn config get pipeline --json` → the structure it executes), so the
    /// table itself — not just its leaves — has to resolve.
    #[test]
    fn get_reaches_the_pipeline_structure_as_one_document() {
        use thegn_core::config::PipelineStage;
        let mut cfg = Config::default();
        cfg.pipeline.stages.push(PipelineStage {
            name: "architect".into(),
            agent: "claude".into(),
            prompt: "design {issue_title}".into(),
            next: Some("code".into()),
            ..Default::default()
        });
        for key in [
            "pipeline",
            "pipeline.stages",
            "pipeline.stages.0.name",
            "pipeline.stages.0.concurrency",
            "pipeline.stages.0.on_blocked",
        ] {
            assert!(
                get(&cfg, key, true, &PathBuf::from("/tmp/config.toml")).is_ok(),
                "config get --json {key}"
            );
        }
        // The JSON form is the real shape (an object with an array), not a
        // stringified scalar — that is what makes it consumable by an agent.
        let v = cfg.value_at("pipeline").expect("pipeline resolves");
        let stages = v["stages"].as_array().expect("stages is an array");
        assert_eq!(stages.len(), 1);
        assert_eq!(stages[0]["name"], "architect");
        assert_eq!(stages[0]["concurrency"], 1);
        assert_eq!(stages[0]["timeout_secs"], 3600);
        assert_eq!(stages[0]["on_blocked"], "park");
        // An empty pipeline still resolves (an inert section, not an error).
        assert!(
            get(
                &Config::default(),
                "pipeline",
                true,
                &PathBuf::from("/tmp/config.toml")
            )
            .is_ok()
        );
        assert!(
            get(
                &cfg,
                "pipeline.nope",
                true,
                &PathBuf::from("/tmp/config.toml")
            )
            .is_err()
        );
    }

    #[test]
    fn get_reaches_the_pr_queue_surface() {
        let cfg = Config::default();
        for key in [
            "pr_queue.enabled",
            "pr_queue.merge_mode",
            "pr_queue.watch",
            "pr_queue.own_prs_only",
            "pr_queue.prompts.ci_failure",
        ] {
            assert!(
                get(&cfg, key, false, &PathBuf::from("/tmp/config.toml")).is_ok(),
                "config get {key}"
            );
            assert!(
                get(&cfg, key, true, &PathBuf::from("/tmp/config.toml")).is_ok(),
                "config get --json {key}"
            );
        }
    }

    #[test]
    fn workspace_layer_reports_both_queue_families() {
        use thegn_core::config::{MergeQueueOverlay, PrMergeMode, PrQueueOverlay, WorkspaceConfig};
        let dir = std::env::temp_dir().join(format!("thegn-wslayer-{}", std::process::id()));
        let repo = dir.join("DataHub");
        std::fs::create_dir_all(&repo).unwrap();
        let slug = thegn_core::config::workspace_slug(&repo);

        let mut cfg = Config::default();
        cfg.workspace.insert(
            slug.clone(),
            WorkspaceConfig {
                merge_queue: MergeQueueOverlay {
                    gate_command: Some("pnpm test".into()),
                    ..MergeQueueOverlay::default()
                },
                pr_queue: PrQueueOverlay {
                    merge_mode: Some(PrMergeMode::Thegn),
                    ..PrQueueOverlay::default()
                },
                ..WorkspaceConfig::default()
            },
        );

        // Both families resolve through the per-repo layer, naming the block.
        let (got_slug, v) = workspace_layer(&cfg, &repo, "merge_queue.gate_command").unwrap();
        assert_eq!(got_slug, slug);
        assert_eq!(v.as_str(), Some("pnpm test"));
        let (_, v) = workspace_layer(&cfg, &repo, "pr_queue.merge_mode").unwrap();
        assert_eq!(v.as_str(), Some("thegn"));

        // A key the overlay leaves alone is not attributed to the layer...
        assert!(workspace_layer(&cfg, &repo, "pr_queue.own_prs_only").is_none());
        // ...and neither is a family outside the two carried here.
        assert!(workspace_layer(&cfg, &repo, "theme.accent").is_none());

        // THE-515: a second block spelled `DataHub` makes the choice
        // ambiguous — explain attributes nothing to either block and the
        // refusal is reported instead of a first-match winner.
        cfg.workspace
            .insert("DataHub".into(), WorkspaceConfig::default());
        assert!(workspace_layer(&cfg, &repo, "merge_queue.gate_command").is_none());
        let refusal = cfg.workspace_overlay_refusal(&repo).expect("refused");
        assert!(refusal.to_string().contains("`DataHub`"), "{refusal}");
        assert!(!cfg.repo_merge_queue(&repo).enabled);

        let _ = std::fs::remove_dir_all(&dir); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    // ---- secret-leak regression tests (synthetic canaries only) -----------

    const CANARIES: &[&str] = &[
        "CANARY-LINEAR-KEY",
        "CANARY-JIRA-TOKEN",
        "CANARY-KANEO-KEY",
        "CANARY-ACCT-TOKEN",
        "CANARY-MP-KEY-1",
        "CANARY-MP-KEY-2",
        "CANARY-MP-KEY",
        "CANARY-GH-TOKEN",
    ];

    fn canary_config() -> Config {
        toml::from_str(
            r#"
[issues.linear]
api_key = "CANARY-LINEAR-KEY"
team_id = "TEAM1"

[issues.jira]
base_url = "https://jira.example.test"
api_token = "CANARY-JIRA-TOKEN"
project_key = "PROJ"

[issues.kaneo]
base_url = "https://kaneo.example.test"
api_key = "CANARY-KANEO-KEY"

[[issues.issue_accounts]]
name = "work"
provider = "linear"
token = "CANARY-ACCT-TOKEN"

[[forges]]
name = "ghe"
token = "CANARY-GH-TOKEN"

[[model_proxy.providers]]
name = "p1"
api_key = "CANARY-MP-KEY"
api_keys = ["CANARY-MP-KEY-1", "CANARY-MP-KEY-2"]

[[model_proxy.routes]]
name = "small"
auto_max_tokens = 4096

[env.e1.provider]
provider = "daytona"
api_key_env = "DAYTONA_API_KEY"
binary_cache_key = "cache.example.test-1:PUBLICKEY"
"#,
        )
        .expect("canary config parses")
    }

    fn assert_clean(label: &str, out: &str) {
        for c in CANARIES {
            assert!(!out.contains(c), "{label} leaked {c}:\n{out}");
        }
    }

    #[test]
    fn config_get_never_prints_a_secret() {
        let cfg = canary_config();
        for key in [
            "issues",
            "issues.linear",
            "issues.jira",
            "issues.kaneo",
            "issues.issue_accounts",
            "issues.issue_accounts.0",
            "issues.linear.api_key",
            "issues.jira.api_token",
            "forges",
            "forges.0",
            "forges.0.token",
            "model_proxy",
            "model_proxy.providers",
            "model_proxy.providers.0.api_keys",
        ] {
            let text = get_text(&cfg, key).unwrap_or_else(|| panic!("{key} should resolve"));
            assert_clean(&format!("get {key}"), &text);
            let json = redacted_value_at(&cfg, key).expect(key).to_string();
            assert_clean(&format!("get --json {key}"), &json);
        }
        // The secret leaf itself is the placeholder, not empty.
        assert_eq!(
            get_text(&cfg, "issues.linear.api_key").as_deref(),
            Some(thegn_core::redact::PLACEHOLDER)
        );
    }

    #[test]
    fn config_get_keeps_hand_mapped_scalars_and_non_secrets() {
        let cfg = canary_config();
        // get_dotted-rendered scalar still works.
        assert_eq!(
            get_text(&cfg, "picker"),
            Config::default().get_dotted("picker")
        );
        assert!(get_text(&cfg, "no.such.key").is_none());
        assert_eq!(
            get_text(&cfg, "issues.jira.project_key").as_deref(),
            Some("PROJ")
        );
        assert_eq!(
            get_text(&cfg, "env.e1.provider.api_key_env").as_deref(),
            Some("DAYTONA_API_KEY")
        );
        assert_eq!(
            redacted_value_at(&cfg, "model_proxy.routes.0.auto_max_tokens"),
            Some(serde_json::json!(4096))
        );
    }

    #[test]
    fn config_show_never_prints_a_secret_and_keeps_non_secrets() {
        let cfg = canary_config();
        let toml_out = show_text(&cfg, false).expect("toml");
        let json_out = show_text(&cfg, true).expect("json");
        assert_clean("show", &toml_out);
        assert_clean("show --json", &json_out);
        assert!(toml_out.contains("PROJ"), "{toml_out}");
        assert!(toml_out.contains("DAYTONA_API_KEY"));
        assert!(toml_out.contains("cache.example.test-1:PUBLICKEY"));
        assert!(toml_out.contains(thegn_core::redact::PLACEHOLDER));
        let parsed: serde_json::Value = serde_json::from_str(&json_out).expect("valid json");
        assert_eq!(parsed["issues"]["jira"]["project_key"], "PROJ");
        assert_eq!(
            parsed["env"]["e1"]["provider"]["api_key_env"],
            "DAYTONA_API_KEY"
        );
        assert_eq!(
            parsed["env"]["e1"]["provider"]["binary_cache_key"],
            "cache.example.test-1:PUBLICKEY"
        );
        assert_eq!(
            parsed["model_proxy"]["routes"][0]["auto_max_tokens"],
            serde_json::json!(4096),
            "a number must stay a number"
        );
        assert_eq!(
            parsed["model_proxy"]["providers"][0]["api_keys"][0],
            thegn_core::redact::PLACEHOLDER
        );
        // Struct field order is preserved (test/smoke.sh greps `[observe]`
        // followed by its first key).
        let observe: Vec<&str> = toml_out
            .lines()
            .skip_while(|l| *l != "[observe]")
            .take(2)
            .collect();
        assert!(
            observe.get(1).is_some_and(|l| l.starts_with("enabled = ")),
            "{observe:?}"
        );
        // The TOML rendering is itself valid TOML.
        toml_out.parse::<toml::Table>().expect("valid toml");
    }

    #[test]
    fn config_explain_never_prints_a_secret() {
        use thegn_core::config::ProcessEnv;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[issues.linear]\napi_key = \"CANARY-LINEAR-KEY\"\n[issues.jira]\napi_token = \"CANARY-JIRA-TOKEN\"\n",
        )
        .unwrap();
        for key in ["issues.linear.api_key", "issues.linear", "issues"] {
            let mut origin =
                thegn_core::config_resolve::explain(&ProcessEnv, &[], Some(path.clone()), key)
                    .expect("explain");
            // Precondition: the raw origin does carry the canary (so the test
            // would fail if redaction were removed).
            let raw = format!("{} {:?}", origin.value, origin.trace);
            assert!(raw.contains("CANARY-LINEAR-KEY"), "{key}: precondition");
            redact_origin(&mut origin);
            let shown = format!("{} {:?}", origin.value, origin.trace);
            assert_clean(&format!("explain {key}"), &shown);
        }
    }
}
