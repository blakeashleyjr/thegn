//! Admission before provider access or ledger retirement. The legacy ledger
//! has no environment/account binding, so one provider kind must have one
//! unambiguous account reference and lifetime policy for automatic cleanup.
use std::fs::File;
use std::path::{Path, PathBuf};

use thegn_core::config::EnvProviderConfig;

fn lock_dir() -> PathBuf {
    #[cfg(test)]
    {
        thread_local! {
            static DIR: tempfile::TempDir = tempfile::tempdir().expect("test reaper lock dir");
        }
        DIR.with(|dir| dir.path().join("locks"))
    }
    #[cfg(not(test))]
    {
        thegn_core::util::xdg_state_home()
            .join("thegn")
            .join("locks")
    }
}

fn ensure_dir(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    crate::platform::restrict_dir_owner_only(dir);
    Ok(())
}

/// A process-shared lease held for one provider kind's full reconcile pass.
pub(crate) struct ReaperLease {
    _file: File,
}

pub(crate) fn try_reaper_lease(kind: &str) -> Result<Option<ReaperLease>, String> {
    try_reaper_lease_in(&lock_dir(), kind)
}

fn try_reaper_lease_in(dir: &Path, kind: &str) -> Result<Option<ReaperLease>, String> {
    if kind.is_empty()
        || kind.len() > 32
        || !kind
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte))
    {
        return Err("invalid provider kind for reaper lock".into());
    }
    let fail = |error: std::io::Error| format!("cannot establish reaper lock: {error}");
    ensure_dir(dir).map_err(fail)?;
    let path = dir.join(format!("reaper-{kind}.lock"));
    let file = crate::platform::open_lock_file(&path).map_err(fail)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(ReaperLease { _file: file })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(fail(error)),
    }
}

pub(crate) fn with_unambiguous<'a, T>(
    envs: impl IntoIterator<Item = &'a EnvProviderConfig>,
    reconcile: impl FnOnce(&'a EnvProviderConfig) -> T,
) -> Result<Option<T>, &'static str> {
    let mut envs = envs.into_iter();
    let Some(first) = envs.next() else {
        return Ok(None);
    };
    if envs.any(|other| {
        other.provider.trim() != first.provider.trim()
            || other.api_base != first.api_base
            || other.api_key_env.trim() != first.api_key_env.trim()
            || other.max_lifetime_secs != first.max_lifetime_secs
    }) {
        return Err(
            "ambiguous environment account/lifetime policy; bind ownership or align configuration before automatic cleanup",
        );
    }
    Ok(Some(reconcile(first)))
}

/// Resources one reaper pass deliberately left in place (quarantined, refused, failed). Each line is logged as it happens
/// (`msg::warn`: the log and CLI keep it); the pass ends with ONE in-app Alert when the set of kept resources changed.
#[derive(Debug, Default)]
pub(crate) struct KeptReport {
    kept: Vec<(String, String)>, // (resource, message)
}

impl KeptReport {
    pub(crate) fn keep(&mut self, resource: &str, message: String) {
        thegn_core::msg::warn(&message);
        self.kept.push((resource.to_string(), message));
    }

    /// End of one provider kind's pass.
    pub(crate) fn publish(self, kind: &str) {
        let mut latch = kept_latch()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let alert = latch_kept(&mut latch, kind, self.kept);
        drop(latch);
        if let Some(message) = alert
            && let Some(state) = crate::notify::global()
        {
            state.emit_toast(&message, thegn_core::notification::Priority::Alert);
        }
    }
}

fn kept_latch() -> &'static std::sync::Mutex<std::collections::BTreeMap<String, Vec<String>>> {
    static LATCH: std::sync::OnceLock<
        std::sync::Mutex<std::collections::BTreeMap<String, Vec<String>>>,
    > = std::sync::OnceLock::new();
    LATCH.get_or_init(|| std::sync::Mutex::new(std::collections::BTreeMap::new()))
}

fn latch_kept(
    latch: &mut std::collections::BTreeMap<String, Vec<String>>,
    kind: &str,
    mut kept: Vec<(String, String)>,
) -> Option<String> {
    kept.sort();
    if kept.is_empty() {
        latch.remove(kind);
        return None;
    }

    let mut names = Vec::new();
    for (resource, _) in &kept {
        if names.last() != Some(resource) {
            names.push(resource.clone());
        }
    }
    if latch.get(kind) == Some(&names) {
        return None;
    }
    latch.insert(kind.to_string(), names.clone());
    Some(kept_alert_text(kind, names.len(), &kept[0].1))
}

fn kept_alert_text(kind: &str, count: usize, first_message: &str) -> String {
    if count == 1 {
        format!("{kind} reaper kept 1 resource for review: {first_message}")
    } else {
        format!(
            "{kind} reaper kept {count} resources for review: {first_message} (+{} more in the log)",
            count - 1
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ambiguity_precedes_inventory_deletion_and_ledger_retirement() {
        let first = EnvProviderConfig {
            provider: "fly".into(),
            api_key_env: "ACCOUNT_A".into(),
            max_lifetime_secs: 60,
            ..Default::default()
        };
        for field in ["lifetime", "credentials", "endpoint"] {
            let mut other = first.clone();
            match field {
                "lifetime" => other.max_lifetime_secs = 0,
                "credentials" => other.api_key_env = "ACCOUNT_B".into(),
                _ => other.api_base = "https://other.invalid".into(),
            }
            let mut actions = [0; 3];
            let result = with_unambiguous([&first, &other], |_| {
                actions[0] += 1; // inventory
                actions[1] += 1; // delete
                actions[2] += 1; // ledger retirement
            });
            assert!(result.is_err(), "{field}");
            assert_eq!(actions, [0; 3]);
        }
        let mut reconciliations = 0;
        with_unambiguous([&first, &first.clone()], |_| reconciliations += 1).unwrap();
        assert_eq!(reconciliations, 1, "identical policy reconciles once");
    }

    #[test]
    fn reaper_lease_is_single_flight_per_provider_kind() {
        let dir = tempfile::tempdir().unwrap();
        let first = try_reaper_lease_in(dir.path(), "hetzner").unwrap();
        assert!(first.is_some(), "first holder should acquire hetzner");

        let second = try_reaper_lease_in(dir.path(), "hetzner").unwrap();
        assert!(
            second.is_none(),
            "a second open file description must conflict"
        );
        let fly = try_reaper_lease_in(dir.path(), "fly").unwrap();
        assert!(
            fly.is_some(),
            "different provider kinds have separate locks"
        );

        drop(first);
        let after_release = try_reaper_lease_in(dir.path(), "hetzner").unwrap();
        assert!(
            after_release.is_some(),
            "dropping the holder releases its lock"
        );
    }

    #[test]
    fn reaper_lease_rejects_invalid_provider_kinds() {
        let dir = tempfile::tempdir().unwrap();
        for kind in ["../x", "", "a12345678901234567890123456789012"] {
            assert!(
                try_reaper_lease_in(dir.path(), kind).is_err(),
                "invalid kind should fail: {kind:?}"
            );
        }
    }

    #[test]
    fn reaper_lease_rejects_unusable_lock_directory() {
        let root = tempfile::tempdir().unwrap();
        let file_path = root.path().join("not-a-directory");
        std::fs::write(&file_path, b"file").unwrap();
        assert!(try_reaper_lease_in(&file_path, "fly").is_err());
    }

    #[test]
    fn kept_alert_is_silent_for_a_clean_pass() {
        let mut latch = std::collections::BTreeMap::new();
        assert_eq!(latch_kept(&mut latch, "hetzner", vec![]), None);
        assert!(!latch.contains_key("hetzner"));
    }

    #[test]
    fn kept_alert_fires_once_for_the_same_kept_set() {
        let mut latch = std::collections::BTreeMap::new();
        let first = vec![
            ("vm-b".to_string(), "msg b".to_string()),
            ("vm-a".to_string(), "msg a".to_string()),
        ];
        assert!(latch_kept(&mut latch, "hetzner", first).is_some());
        let second = vec![
            ("vm-a".to_string(), "changed a".to_string()),
            ("vm-b".to_string(), "changed b".to_string()),
        ];
        assert_eq!(latch_kept(&mut latch, "hetzner", second), None);
    }

    #[test]
    fn kept_alert_fires_again_when_the_kept_set_changes() {
        let mut latch = std::collections::BTreeMap::new();
        let first = vec![
            ("vm-a".to_string(), "msg a".to_string()),
            ("vm-b".to_string(), "msg b".to_string()),
        ];
        assert!(latch_kept(&mut latch, "hetzner", first.clone()).is_some());
        let mut changed = first;
        changed.push(("vm-c".to_string(), "msg c".to_string()));
        assert!(latch_kept(&mut latch, "hetzner", changed).is_some());
    }

    #[test]
    fn kept_alert_rearms_after_a_clean_pass() {
        let mut latch = std::collections::BTreeMap::new();
        let kept = vec![("vm-a".to_string(), "msg a".to_string())];
        assert!(latch_kept(&mut latch, "hetzner", kept.clone()).is_some());
        assert_eq!(latch_kept(&mut latch, "hetzner", vec![]), None);
        assert!(latch_kept(&mut latch, "hetzner", kept).is_some());
    }

    #[test]
    fn kept_alert_names_the_count_and_first_line() {
        let mut latch = std::collections::BTreeMap::new();
        let multiple = vec![
            ("vm-b".to_string(), "msg b".to_string()),
            ("vm-a".to_string(), "msg a".to_string()),
            ("vm-a".to_string(), "msg a2".to_string()),
        ];
        assert_eq!(
            latch_kept(&mut latch, "hetzner", multiple).as_deref(),
            Some("hetzner reaper kept 2 resources for review: msg a (+1 more in the log)")
        );
        assert_eq!(
            latch_kept(
                &mut latch,
                "digitalocean",
                vec![("vm-a".to_string(), "single line".to_string())]
            )
            .as_deref(),
            Some("digitalocean reaper kept 1 resource for review: single line")
        );
    }

    #[test]
    fn kept_alert_latches_per_kind() {
        let mut latch = std::collections::BTreeMap::new();
        let kept = vec![("vm-a".to_string(), "msg a".to_string())];
        assert!(latch_kept(&mut latch, "hetzner", kept.clone()).is_some());
        assert!(latch_kept(&mut latch, "fly", kept.clone()).is_some());
        assert_eq!(latch_kept(&mut latch, "hetzner", kept.clone()), None);
        assert_eq!(latch_kept(&mut latch, "fly", kept), None);
    }
}
