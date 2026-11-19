use std::io::Write;
use std::path::Path;

pub(crate) const FILE: &str = "git-audit.jsonl";
pub(crate) const PREV: &str = "git-audit.1.jsonl";
const MAX_BYTES: u64 = 1 << 20;

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct GitAuditEvent<'a> {
    pub ts_ms: i64,
    pub action: &'a str,
    pub worktree: &'a str,
    pub branch: &'a str,
    pub from: &'a str,
    pub to: &'a str,
    pub outcome: &'a str,
}

/// Best-effort audit recording: a filesystem failure is visible in tracing but
/// never changes the provider operation's result.
pub(crate) fn record(event: &GitAuditEvent<'_>) {
    let anchor = thegn_core::util::xdg_state_home().join("thegn");
    let result = std::fs::create_dir_all(&anchor).and_then(|()| {
        thegn_core::fsperm::ensure_private_dir_beneath(&anchor, Path::new("provider"))
    });
    let result = result.and_then(|dir| record_in(&dir, event, MAX_BYTES));
    if let Err(error) = result {
        tracing::warn!(
            target: "thegn::git_audit",
            error = %error,
            "could not record git audit event"
        );
    }
}

pub(crate) fn record_in(
    dir: &thegn_core::fsperm::PrivateDir,
    event: &GitAuditEvent<'_>,
    max_bytes: u64,
) -> std::io::Result<()> {
    let action = clean_field(event.action);
    let worktree = clean_field(event.worktree);
    let branch = clean_field(event.branch);
    let from = clean_field(event.from);
    let to = clean_field(event.to);
    let outcome = clean_field(event.outcome);
    let clean = GitAuditEvent {
        ts_ms: event.ts_ms,
        action: &action,
        worktree: &worktree,
        branch: &branch,
        from: &from,
        to: &to,
        outcome: &outcome,
    };
    let mut line = serde_json::to_vec(&clean).map_err(std::io::Error::other)?;
    line.push(b'\n');

    let mut file = dir.open_or_create_regular_nofollow_append(FILE)?;
    if file.metadata()?.len() >= max_bytes {
        drop(file);
        dir.remove_file(PREV)?;
        dir.rename_child_noreplace(FILE, PREV)?;
        file = dir.open_or_create_regular_nofollow_append(FILE)?;
    }
    file.write_all(&line)
}

fn clean_field(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(256)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{FILE, GitAuditEvent, PREV, record, record_in};
    use std::path::Path;
    use std::sync::Arc;
    use thegn_core::fsperm::ensure_private_dir_beneath;
    use thegn_core::testenv::IsolatedState;

    fn event<'a>(action: &'a str) -> GitAuditEvent<'a> {
        GitAuditEvent {
            ts_ms: 7,
            action,
            worktree: "/worktree",
            branch: "tg/lane",
            from: "a",
            to: "b",
            outcome: "ok",
        }
    }

    fn audit_path(state: &IsolatedState, name: &str) -> std::path::PathBuf {
        state.state_home().join("thegn/provider").join(name)
    }

    fn private_provider_dir() -> (IsolatedState, thegn_core::fsperm::PrivateDir) {
        let state = IsolatedState::new();
        let anchor = state.state_home().join("thegn");
        std::fs::create_dir_all(&anchor).unwrap();
        let dir = ensure_private_dir_beneath(&anchor, Path::new("provider")).unwrap();
        (state, dir)
    }

    #[test]
    fn record_appends_one_json_line_per_event() {
        let state = IsolatedState::new();
        record(&event("seed"));
        record(&event("pull"));

        let contents = std::fs::read_to_string(audit_path(&state, FILE)).unwrap();
        let lines: Vec<_> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(lines[0])["action"],
            "seed"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(lines[1])["action"],
            "pull"
        );
    }

    #[test]
    fn record_rotates_at_the_cap_keeping_one_previous() {
        let (state, dir) = private_provider_dir();
        record_in(&dir, &event("seed"), 1).unwrap();
        record_in(&dir, &event("pull"), 1).unwrap();

        let previous = std::fs::read_to_string(audit_path(&state, PREV)).unwrap();
        let current = std::fs::read_to_string(audit_path(&state, FILE)).unwrap();
        assert_eq!(previous.lines().count(), 1);
        assert_eq!(current.lines().count(), 1);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(previous.trim())["action"],
            "seed"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(current.trim())["action"],
            "pull"
        );
    }

    #[test]
    fn interleaved_record_in_calls_leave_intact_json_lines() {
        let state = IsolatedState::new();
        let anchor = state.state_home().join("thegn");
        std::fs::create_dir_all(&anchor).unwrap();
        let dir_a = ensure_private_dir_beneath(&anchor, Path::new("provider")).unwrap();
        let dir_b = ensure_private_dir_beneath(&anchor, Path::new("provider")).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));

        let first_barrier = Arc::clone(&barrier);
        let first = std::thread::spawn(move || {
            first_barrier.wait();
            record_in(&dir_a, &event("seed"), u64::MAX).unwrap();
        });
        let second_barrier = Arc::clone(&barrier);
        let second = std::thread::spawn(move || {
            second_barrier.wait();
            record_in(&dir_b, &event("pull"), u64::MAX).unwrap();
        });

        first.join().unwrap();
        second.join().unwrap();

        let contents = std::fs::read_to_string(audit_path(&state, FILE)).unwrap();
        let mut actions: Vec<_> = contents
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()["action"].clone())
            .collect();
        actions.sort_by_key(|action| action.as_str().unwrap().to_owned());
        assert_eq!(
            actions,
            vec![serde_json::json!("pull"), serde_json::json!("seed")]
        );
    }

    #[test]
    fn record_strips_controls_and_bounds_fields() {
        let state = IsolatedState::new();
        let long = format!("start\n\t{}", "x".repeat(300));
        record(&GitAuditEvent {
            ts_ms: 9,
            action: &long,
            worktree: &long,
            branch: &long,
            from: &long,
            to: &long,
            outcome: &long,
        });

        let contents = std::fs::read_to_string(audit_path(&state, FILE)).unwrap();
        let value: serde_json::Value = serde_json::from_str(contents.trim()).unwrap();
        for field in ["action", "worktree", "branch", "from", "to", "outcome"] {
            let value = value[field].as_str().unwrap();
            assert_eq!(value.chars().count(), 256);
            assert!(value.chars().all(|character| !character.is_control()));
        }
    }
}
