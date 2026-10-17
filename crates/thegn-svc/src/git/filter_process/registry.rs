//! Per-worktree ownership for long-running Git clean filters (THE-700).
//!
//! A filter is reused by the native worktree diff path. Entries are reaped on
//! the next registry use after they become idle; there is no sweeper thread.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command as ProcessCommand;
use std::sync::{Arc, LazyLock, Mutex, TryLockError};
use std::time::{Duration, Instant};

use super::protocol::{Capabilities, Command, Outcome, ProtocolError};
use super::{FilterProcess, FilterProcessError};

const IDLE_TTL: Duration = Duration::from_secs(120);
const MAX_LIVE: usize = 8;
const FAIL_BACKOFF: Duration = Duration::from_secs(60);
const REQUEST_DEADLINE: Duration = Duration::from_secs(10);
const MAX_CLEAN_OUTPUT: usize = 256 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FilterKey {
    pub workdir: PathBuf,
    pub process: String,
}

pub enum Routed {
    Done(Outcome),
    Unavailable,
}

struct Slot {
    process: Option<FilterProcess>,
    last_used: Instant,
    failed_at: Option<Instant>,
}

pub struct FilterRegistry {
    entries: Mutex<HashMap<FilterKey, Arc<Mutex<Slot>>>>,
    idle_ttl: Duration,
    max_live: usize,
    fail_backoff: Duration,
}

static GLOBAL: LazyLock<FilterRegistry> = LazyLock::new(FilterRegistry::new);

pub fn global() -> &'static FilterRegistry {
    &GLOBAL
}

impl FilterRegistry {
    fn new() -> Self {
        Self::with_limits(IDLE_TTL, MAX_LIVE, FAIL_BACKOFF)
    }

    pub fn with_limits(idle_ttl: Duration, max_live: usize, fail_backoff: Duration) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            idle_ttl,
            max_live,
            fail_backoff,
        }
    }

    pub fn clean(
        &self,
        key: FilterKey,
        spawn: impl Fn() -> std::io::Result<ProcessCommand>,
        pathname: &str,
        content: Arc<[u8]>,
    ) -> Routed {
        self.sweep();
        let mut retired = Vec::new();
        let slot = {
            let Ok(mut entries) = self.entries.lock() else {
                return Routed::Unavailable;
            };
            let is_new = !entries.contains_key(&key);
            let slot = Arc::clone(entries.entry(key.clone()).or_insert_with(|| {
                Arc::new(Mutex::new(Slot {
                    process: None,
                    last_used: Instant::now(),
                    failed_at: None,
                }))
            }));
            if is_new {
                while entries.len() > self.max_live {
                    let mut oldest = None;
                    for (candidate, candidate_slot) in &*entries {
                        if candidate == &key || Arc::strong_count(candidate_slot) > 1 {
                            continue;
                        }
                        match candidate_slot.try_lock() {
                            Ok(guard) => {
                                let last_used = guard.last_used;
                                if oldest.as_ref().is_none_or(|(_, used)| last_used < *used) {
                                    oldest = Some((candidate.clone(), last_used));
                                }
                            }
                            Err(TryLockError::Poisoned(_)) => {
                                oldest = Some((candidate.clone(), Instant::now()));
                                break;
                            }
                            Err(TryLockError::WouldBlock) => {}
                        }
                    }
                    let Some((oldest, _)) = oldest else { break };
                    if let Some(slot) = entries.remove(&oldest) {
                        retired.push(slot);
                    }
                }
            }
            slot
        };
        drop(retired);
        let Ok(mut slot) = slot.lock() else {
            return Routed::Unavailable;
        };
        let now = Instant::now();
        slot.last_used = now;
        if slot
            .failed_at
            .is_some_and(|failed| now.duration_since(failed) < self.fail_backoff)
        {
            return Routed::Unavailable;
        }
        if slot.process.is_none() {
            match spawn_filter(&spawn) {
                Ok(process) => {
                    slot.process = Some(process);
                    slot.failed_at = None;
                }
                Err(error) => {
                    tracing::debug!(target: "thegn::git", %error, "filter process unavailable");
                    slot.failed_at = Some(Instant::now());
                    return Routed::Unavailable;
                }
            }
        }
        let first = request(&mut slot, pathname, Arc::clone(&content));
        match first {
            Ok(outcome) => Routed::Done(outcome),
            Err(error) if is_nonfatal_refusal(&error) => Routed::Unavailable,
            Err(error) => {
                tracing::debug!(target: "thegn::git", %error, "filter process failed; retrying once");
                {
                    let _retired = slot.process.take();
                }
                match spawn_filter(&spawn) {
                    Ok(process) => slot.process = Some(process),
                    Err(spawn_error) => {
                        tracing::debug!(target: "thegn::git", %spawn_error, "filter process respawn failed");
                        slot.failed_at = Some(Instant::now());
                        return Routed::Unavailable;
                    }
                }
                match request(&mut slot, pathname, content) {
                    Ok(outcome) => Routed::Done(outcome),
                    Err(retry_error) if is_nonfatal_refusal(&retry_error) => Routed::Unavailable,
                    Err(retry_error) => {
                        tracing::debug!(target: "thegn::git", %retry_error, "filter process retry failed");
                        let _retired = slot.process.take();
                        slot.failed_at = Some(Instant::now());
                        Routed::Unavailable
                    }
                }
            }
        }
    }

    fn sweep(&self) {
        let now = Instant::now();
        let mut retired = Vec::new();
        if let Ok(mut entries) = self.entries.lock() {
            let mut idle = Vec::new();
            for (key, slot) in entries.iter() {
                if Arc::strong_count(slot) > 1 {
                    continue;
                }
                match slot.try_lock() {
                    Ok(slot) if now.duration_since(slot.last_used) > self.idle_ttl => {
                        idle.push(key.clone());
                    }
                    Err(TryLockError::Poisoned(_)) => idle.push(key.clone()),
                    _ => {}
                }
            }
            for key in idle {
                if let Some(slot) = entries.remove(&key) {
                    retired.push(slot);
                }
            }
            let mut recent = Vec::new();
            for (key, slot) in entries.iter() {
                if Arc::strong_count(slot) > 1 {
                    continue;
                }
                match slot.try_lock() {
                    Ok(slot) => recent.push((key.clone(), slot.last_used)),
                    Err(TryLockError::Poisoned(_)) => {}
                    Err(TryLockError::WouldBlock) => {}
                }
            }
            recent.sort_by_key(|(_, used)| *used);
            let excess = entries.len().saturating_sub(self.max_live);
            for (key, _) in recent.into_iter().take(excess) {
                if let Some(slot) = entries.remove(&key) {
                    retired.push(slot);
                }
            }
        }
        drop(retired);
    }

    #[cfg(test)]
    fn live(&self) -> usize {
        self.entries
            .lock()
            .map(|entries| entries.len())
            .unwrap_or(0)
    }

    #[cfg(test)]
    pub(crate) fn shutdown_all(&self) {
        let retired = match self.entries.lock() {
            Ok(mut entries) => entries.drain().map(|(_, slot)| slot).collect::<Vec<_>>(),
            Err(_) => Vec::new(),
        };
        drop(retired);
    }
}

fn spawn_filter(
    spawn: &impl Fn() -> std::io::Result<ProcessCommand>,
) -> Result<FilterProcess, FilterProcessError> {
    let command = spawn().map_err(FilterProcessError::Spawn)?;
    FilterProcess::spawn(command, Capabilities::new(true, false), REQUEST_DEADLINE)
}

fn request(
    slot: &mut Slot,
    pathname: &str,
    content: Arc<[u8]>,
) -> Result<Outcome, FilterProcessError> {
    match slot.process.as_mut() {
        Some(process) => process.request(Command::Clean, pathname, content, MAX_CLEAN_OUTPUT),
        None => Err(FilterProcessError::Dead),
    }
}

fn is_nonfatal_refusal(error: &FilterProcessError) -> bool {
    matches!(
        error,
        FilterProcessError::Protocol(ProtocolError::Unsupported(_) | ProtocolError::InvalidPath)
    )
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
