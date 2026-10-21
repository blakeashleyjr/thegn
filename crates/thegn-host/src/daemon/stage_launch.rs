//! Stage launch identity and response reconciliation for THE-272.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Mutex;

use thegn_svc::control::client::ControlRequestError;
use thegn_svc::control::{OpenSpec, SessionInfo};

use super::tombstone::{MAX_TOMBSTONES, TOMBSTONE_TTL_MS};

pub(crate) const STAGE_LAUNCH_ENV: &str = "THEGN_STAGE_LAUNCH";

pub(crate) fn mint_launch_id() -> Option<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).ok()?;
    let mut id = String::with_capacity(32);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut id, "{byte:02x}").ok()?;
    }
    Some(id)
}

pub(crate) fn launch_id_of(spec: &OpenSpec) -> Option<Result<String, String>> {
    let value = spec
        .env
        .iter()
        .find_map(|(key, value)| (key == STAGE_LAUNCH_ENV).then_some(value))?;
    if value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Some(Ok(value.clone()))
    } else {
        Some(Err(
            "expected exactly 32 lowercase hexadecimal characters".into()
        ))
    }
}

#[derive(Default)]
pub(crate) struct LaunchRegistry {
    entries: HashMap<String, LaunchEntry>,
}

struct LaunchEntry {
    row: Option<i64>,
    worktree: Option<String>,
    at_ms: i64,
    state: LaunchState,
}

enum LaunchState {
    Opening,
    Opened(SessionInfo),
    Refused(String),
}

pub(crate) enum LaunchAdmit {
    Fresh,
    Replay(SessionInfo),
    Refused(String),
    StillOpening,
    Mismatch,
}

impl LaunchRegistry {
    pub(crate) fn begin(
        &mut self,
        id: &str,
        row: Option<i64>,
        worktree: Option<&str>,
        now_ms: i64,
    ) -> LaunchAdmit {
        self.entries.retain(|_, entry| {
            matches!(&entry.state, LaunchState::Opening)
                || now_ms.saturating_sub(entry.at_ms) <= TOMBSTONE_TTL_MS
        });
        if let Some(entry) = self.entries.get(id) {
            if entry.row != row || entry.worktree.as_deref() != worktree {
                return LaunchAdmit::Mismatch;
            }
            return match &entry.state {
                LaunchState::Opening => LaunchAdmit::StillOpening,
                LaunchState::Opened(info) => LaunchAdmit::Replay(info.clone()),
                LaunchState::Refused(message) => LaunchAdmit::Refused(message.clone()),
            };
        }
        if self.entries.len() >= MAX_TOMBSTONES {
            let oldest = self
                .entries
                .iter()
                .filter(|(_, entry)| !matches!(&entry.state, LaunchState::Opening))
                .min_by_key(|(_, entry)| entry.at_ms)
                .map(|(id, _)| id.clone());
            if let Some(oldest) = oldest {
                self.entries.remove(&oldest);
            } else {
                return LaunchAdmit::Refused("too many stage launches in flight".into());
            }
        }
        self.entries.insert(
            id.to_string(),
            LaunchEntry {
                row,
                worktree: worktree.map(str::to_string),
                at_ms: now_ms,
                state: LaunchState::Opening,
            },
        );
        LaunchAdmit::Fresh
    }

    fn refuse(&mut self, id: &str) {
        if let Some(entry) = self.entries.get_mut(id) {
            entry.state = LaunchState::Refused(
                "the first open with this launch id failed; see its response".into(),
            );
        }
    }

    fn commit(&mut self, id: &str, info: &SessionInfo) {
        if let Some(entry) = self.entries.get_mut(id) {
            entry.state = LaunchState::Opened(info.clone());
        }
    }
}

pub(crate) struct LaunchGuard<'a> {
    registry: &'a Mutex<LaunchRegistry>,
    id: String,
    committed: bool,
}

impl<'a> LaunchGuard<'a> {
    pub(crate) fn new(registry: &'a Mutex<LaunchRegistry>, id: &str) -> Self {
        Self {
            registry,
            id: id.to_string(),
            committed: false,
        }
    }

    pub(crate) fn commit(mut self, info: &SessionInfo) {
        self.registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .commit(&self.id, info);
        self.committed = true;
    }
}

impl Drop for LaunchGuard<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.registry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .refuse(&self.id);
        }
    }
}

pub(crate) fn open_error_is_ambiguous(error: &anyhow::Error) -> bool {
    !error
        .chain()
        .any(|cause| cause.downcast_ref::<ControlRequestError>().is_some())
}

pub(crate) async fn open_with_reconcile<F, Fut>(open: F) -> anyhow::Result<SessionInfo>
where
    F: Fn() -> Fut,
    Fut: Future<Output = anyhow::Result<SessionInfo>>,
{
    match open().await {
        Ok(info) => Ok(info),
        Err(first) if open_error_is_ambiguous(&first) => match open().await {
            Ok(info) => Ok(info),
            Err(retry) => Err(first.context(format!("stage launch retry also failed: {retry:#}"))),
        },
        Err(error) => Err(error),
    }
}

#[cfg(test)]
#[path = "stage_launch_tests.rs"]
mod tests;
