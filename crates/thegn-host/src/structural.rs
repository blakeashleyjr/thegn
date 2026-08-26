//! The `ast-grep` structural-search seam implementation (THE-5).
//!
//! This is the ONE file the ast-grep vendor CLI is allowed to live in (the
//! seams-not-vendors rule). It implements [`StructuralSearch`] by invoking
//! `ast-grep`/`sg` **argv-only** (never a shell), with `--json` output parsed
//! by the defensive core parser, and — critically — **never** with a write flag
//! (`--update-all`): ast-grep only *computes* matches and replacement text; the
//! guarded apply path in [`crate::search_apply`] is the sole writer.
//!
//! The subprocess is memory-capped via [`thegn_core::sandbox_cpucap::wrap_background_argv`]
//! (the shared `thegn.slice` ceiling) and must run off the event loop — the
//! search worker calls it from `spawn_blocking`.

use std::path::Path;
use std::process::Command;

use thegn_core::config::StructuralKind;
use thegn_core::sandbox_cpucap::wrap_background_argv;
use thegn_core::search_replace::{
    StructuralCaps, StructuralError, StructuralMatch, StructuralSearch, StructuralSpec,
    parse_ast_grep_json,
};

/// Upper bound on structural matches parsed from one invocation — bounds memory
/// against a pathological pattern.
const MAX_STRUCTURAL_MATCHES: usize = 5_000;

/// The ast-grep provider. Resolves the vendor binary (`ast-grep`, else `sg`) at
/// call time so a mid-session install is picked up without a restart.
pub struct AstGrep;

impl AstGrep {
    /// The resolved vendor binary, or `None` when neither name is on PATH.
    fn binary() -> Option<String> {
        thegn_core::util::which_path("ast-grep")
            .map(|_| "ast-grep".to_string())
            .or_else(|| thegn_core::util::which_path("sg").map(|_| "sg".to_string()))
    }

    /// Build the argv for a search/rewrite. `--json` (never `--update-all`), the
    /// pattern and rewrite passed as single argv elements (no shell), the
    /// worktree searched from `root` as cwd. Wrapped in the background cap.
    fn build_argv(bin: &str, spec: &StructuralSpec) -> Vec<String> {
        let mut argv = vec![
            bin.to_string(),
            "run".to_string(),
            "--pattern".to_string(),
            spec.pattern.clone(),
            "--json".to_string(),
        ];
        if !spec.lang.trim().is_empty() {
            argv.push("--lang".to_string());
            argv.push(spec.lang.clone());
        }
        if let Some(rw) = &spec.rewrite {
            // `--rewrite` computes replacement text in the JSON; WITHOUT
            // `--update-all` ast-grep writes nothing. That omission is the
            // security contract — do not add a write flag here.
            argv.push("--rewrite".to_string());
            argv.push(rw.clone());
        }
        // Search the whole worktree (cwd = root).
        argv.push(".".to_string());
        wrap_background_argv(argv)
    }

    /// Run ast-grep off the event loop and parse its JSON. Blocking — call from
    /// `spawn_blocking`.
    fn run(
        &self,
        root: &Path,
        spec: &StructuralSpec,
    ) -> Result<Vec<StructuralMatch>, StructuralError> {
        let Some(bin) = Self::binary() else {
            return Err(StructuralError::not_installed("ast-grep"));
        };
        let argv = Self::build_argv(&bin, spec);
        let (prog, rest) = argv
            .split_first()
            .ok_or_else(|| StructuralError::other("empty argv"))?;
        // off-loop: a blocking subprocess, always spawned from spawn_blocking.
        #[expect(clippy::disallowed_methods)]
        let out = Command::new(prog)
            .args(rest)
            .current_dir(root)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| StructuralError::other(format!("ast-grep failed to launch: {e}")))?;
        if !out.status.success() {
            // ast-grep exits non-zero on a bad pattern / unsupported lang; a
            // no-match run exits 0 with an empty array. Surface the stderr tail.
            let msg = String::from_utf8_lossy(&out.stderr);
            let tail: String = msg.lines().take(3).collect::<Vec<_>>().join("; ");
            return Err(StructuralError::other(if tail.is_empty() {
                format!("ast-grep exited with status {}", out.status)
            } else {
                tail
            }));
        }
        parse_ast_grep_json(&out.stdout, MAX_STRUCTURAL_MATCHES).map_err(StructuralError::other)
    }
}

impl StructuralSearch for AstGrep {
    fn id(&self) -> &'static str {
        "ast-grep"
    }
    fn caps(&self) -> StructuralCaps {
        StructuralCaps {
            search: true,
            rewrite: true,
        }
    }
    fn search(
        &self,
        root: &Path,
        spec: &StructuralSpec,
    ) -> Result<Vec<StructuralMatch>, StructuralError> {
        // A search never carries a rewrite.
        let spec = StructuralSpec {
            rewrite: None,
            ..spec.clone()
        };
        self.run(root, &spec)
    }
    fn rewrite(
        &self,
        root: &Path,
        spec: &StructuralSpec,
    ) -> Result<Vec<StructuralMatch>, StructuralError> {
        if spec.rewrite.is_none() {
            return Err(StructuralError::other(
                "rewrite called without a rewrite template",
            ));
        }
        self.run(root, spec)
    }
}

/// Resolve the configured structural provider, or `None` when the tier is
/// disabled (`none`) or selects a reserved kind. The returned box is ready to
/// call; whether the binary is present is discovered on first use (degrading to
/// [`StructuralError::not_installed`]).
pub fn provider(kind: StructuralKind) -> Option<Box<dyn StructuralSearch>> {
    match kind {
        StructuralKind::AstGrep => Some(Box::new(AstGrep)),
        StructuralKind::None => None,
        // Reserved kinds have no implementation in this build.
        StructuralKind::Comby | StructuralKind::Gritql => None,
    }
}

/// Whether the ast-grep vendor binary is available (offline probe input).
pub fn ast_grep_available() -> bool {
    AstGrep::binary().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv_is_argv_only_and_never_writes() {
        let spec = StructuralSpec {
            pattern: "console.log($A)".into(),
            lang: "ts".into(),
            rewrite: Some("logger.debug($A)".into()),
        };
        let argv = AstGrep::build_argv("ast-grep", &spec);
        // The pattern and rewrite ride as single argv elements (no shell).
        assert!(argv.contains(&"console.log($A)".to_string()));
        assert!(argv.contains(&"logger.debug($A)".to_string()));
        assert!(argv.contains(&"--json".to_string()));
        assert!(argv.contains(&"--lang".to_string()));
        assert!(argv.contains(&"ts".to_string()));
        // The security contract: NEVER a write flag.
        assert!(!argv.iter().any(|a| a == "--update-all" || a == "-U"));
    }

    #[test]
    fn argv_omits_lang_and_rewrite_when_absent() {
        let spec = StructuralSpec {
            pattern: "foo($A)".into(),
            lang: String::new(),
            rewrite: None,
        };
        let argv = AstGrep::build_argv("sg", &spec);
        assert!(!argv.contains(&"--lang".to_string()));
        assert!(!argv.contains(&"--rewrite".to_string()));
        assert_eq!(argv.last().map(String::as_str), Some("."));
    }

    #[test]
    fn provider_selection() {
        assert!(provider(StructuralKind::AstGrep).is_some());
        assert!(provider(StructuralKind::None).is_none());
        assert!(provider(StructuralKind::Comby).is_none());
    }

    #[test]
    fn caps_declare_both_ops() {
        let caps = AstGrep.caps();
        assert!(caps.search && caps.rewrite);
        assert_eq!(AstGrep.id(), "ast-grep");
    }
}
