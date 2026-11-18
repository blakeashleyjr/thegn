//! The Lead's skill seed (THE-2171): the operating notes a Lead tab's agent
//! reads from its notes dir.

use std::path::Path;

use anyhow::{Context as _, Result};

/// Name of the Lead skill directory under `.claude/skills/`.
const SKILL_NAME: &str = "lead";

/// Seed the Lead skill into `notes_dir/.claude/skills/lead/SKILL.md`.
///
/// Idempotent: the file is only rewritten when its content differs.
#[allow(dead_code, reason = "called by the Lead launch path")]
pub(crate) fn seed(_cfg: &thegn_core::config::Config, notes_dir: &Path) -> Result<()> {
    let body = embedded_body()?;
    let path = notes_dir
        .join(".claude")
        .join("skills")
        .join(SKILL_NAME)
        .join("SKILL.md");
    if std::fs::read(&path).is_ok_and(|existing| existing == body.as_bytes()) {
        return Ok(());
    }
    let rel = Path::new(".claude").join("skills").join(SKILL_NAME);
    let dir = thegn_core::fsperm::ensure_private_dir_beneath(notes_dir, &rel)
        .with_context(|| format!("create {}", notes_dir.join(&rel).display()))?;
    dir.write_owner_only_atomic("SKILL.md", body.as_bytes())
        .with_context(|| format!("seed the Lead skill at {}", path.display()))?;
    Ok(())
}

fn embedded_body() -> Result<&'static str> {
    thegn_core::skills::EMBEDDED_MANIFEST
        .iter()
        .find(|e| e.name == SKILL_NAME)
        .map(|e| e.document)
        .context("the embedded registry has no `lead` skill")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(dir: &Path) -> std::path::PathBuf {
        dir.join(".claude/skills/lead/SKILL.md")
    }

    #[test]
    fn seed_writes_once_is_idempotent_and_rewrites_on_change() {
        let _state = crate::testenv::IsolatedState::with_env(&[]);
        let dir = tempfile::tempdir().unwrap();
        let cfg = thegn_core::config::Config::default();
        let path = target(dir.path());

        seed(&cfg, dir.path()).unwrap();
        let first = std::fs::read(&path).unwrap();
        assert_eq!(first, embedded_body().unwrap().as_bytes());
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();

        seed(&cfg, dir.path()).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), mtime);

        std::fs::write(&path, b"stale").unwrap();
        seed(&cfg, dir.path()).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), first);
    }
}
