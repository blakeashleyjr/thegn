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
        Action::Path => outln!("{}", path.display()),
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
            let parse_err = toml::from_str::<Config>(&written)
                .err()
                .map(|e| e.to_string());
            let enum_errs = thegn_core::config::validate_str(&written);
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
        Action::Schema => {
            let schema = schemars::schema_for!(Config);
            outln!("{}", serde_json::to_string_pretty(&schema).unwrap());
        }
        Action::Explain { key, repo, json } => explain(cfg, &key, repo, json, path)?,
    }
    Ok(())
}

fn explain(cfg: &Config, key: &str, repo: Option<String>, json: bool, path: PathBuf) -> Result<()> {
    use thegn_core::config::ProcessEnv;
    use thegn_core::config_resolve;
    let origin = config_resolve::explain(&ProcessEnv, &[], Some(path), key);
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
        .and_then(|root| workspace_layer(cfg, root, key));
    if json {
        let mut obj = serde_json::json!({
            "key": origin.key,
            "value": ws.as_ref().map_or(origin.value.clone(), |(_, v)| v.clone()),
            "origin": ws.as_ref().map_or(origin.origin.as_str().to_string(), |(s, _)| {
                format!("workspace [workspace.{s}]")
            }),
            "cascade_value": origin.value,
        });
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
            outln!("  set by: workspace `[workspace.{slug}]`");
        }
        None => {
            outln!("{} = {}", origin.key, origin.value);
            outln!("  set by: {}", origin.origin.as_str());
        }
    }
    for (layer, val) in &origin.trace {
        outln!("    {}: {val}", layer.as_str());
    }
    if let (Some((slug, v)), Some(root)) = (&ws, &repo_root) {
        outln!(
            "    workspace `[workspace.{slug}]`: {v}   (for {})",
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
    let slug = thegn_core::config::workspace_slug(repo_root);
    let ws = cfg.workspace.get(&slug)?;

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
    (v != global.get(sub)?).then(|| (slug, v.clone()))
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
    if json {
        outln!("{}", serde_json::to_string_pretty(cfg)?);
    } else {
        thegn_core::out!("{}", toml::to_string_pretty(cfg)?);
    }
    Ok(())
}

fn get(cfg: &Config, key: &str, json: bool, path: &Path) -> Result<()> {
    if json {
        // Emit the value's REAL type (number, bool, array, table) rather than a
        // stringified scalar, so `config get --json` composes with `jq`.
        return match cfg.value_at(key) {
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
    match cfg.get_dotted(key) {
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

        let _ = std::fs::remove_dir_all(&dir); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }
}
