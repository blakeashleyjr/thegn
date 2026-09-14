//! Demand-only capability cache. It never authorizes a launch or trust decision.
//! One result and one synchronous producer are retained; no refresh timer runs.

use super::{CLI_NAME, PROBE_TIMEOUT, ProbeReport, ProbeState, probe_command};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime};

const READY_TTL: Duration = Duration::from_secs(30);
const FAILED_TTL: Duration = Duration::from_secs(5);
const MAX_ENV_ENTRIES: usize = 1024;
const MAX_ENV_BYTES: usize = 256 * 1024;
const MAX_ENV_ENTRY_BYTES: usize = 64 * 1024;
const MAX_PATH_BYTES: usize = 16 * 1024;
const MAX_SEARCH_DIRS: usize = 256;

fn check_env_entry(
    name: &std::ffi::OsStr,
    value: &std::ffi::OsStr,
    count: usize,
    bytes: &mut usize,
) -> Result<(), &'static str> {
    let size = name
        .as_encoded_bytes()
        .len()
        .checked_add(value.as_encoded_bytes().len())
        .filter(|size| *size <= MAX_ENV_ENTRY_BYTES)
        .ok_or("version probe environment exceeds input bound")?;
    *bytes = bytes
        .checked_add(size)
        .filter(|size| *size <= MAX_ENV_BYTES)
        .ok_or("version probe environment exceeds input bound")?;
    if count >= MAX_ENV_ENTRIES {
        return Err("version probe environment exceeds entry bound");
    }
    Ok(())
}

#[derive(Clone, PartialEq, Eq)]
struct Executable {
    path: PathBuf,
    // Retaining the handle distinguishes replacement even when size/mtime match.
    // This is cache coherence, not an atomic execution/hostile-writer guarantee.
    identity: Arc<same_file::Handle>,
    bytes: u64,
    modified: SystemTime,
}

#[derive(Clone, PartialEq, Eq)]
struct Key {
    executable: Option<Executable>,
    cwd: PathBuf,
    env_digest: [u8; 32],
}

struct Inputs {
    key: Key,
    // Transient command inputs only: neither values nor their Debug projection
    // enter the cache or diagnostic messages.
    env: Vec<(OsString, OsString)>,
}
impl Inputs {
    fn capture() -> Result<Self, &'static str> {
        // vars_os takes a standard-library snapshot first; that implementation
        // and OS filesystem calls are not hard-real-time APIs. Bound our owned
        // key construction before growing, sorting or hashing the snapshot.
        let mut env = Vec::new();
        let mut bytes = 0;
        for (name, value) in std::env::vars_os() {
            check_env_entry(&name, &value, env.len(), &mut bytes)?;
            env.push((name, value));
        }
        Self::from_snapshot(
            env,
            std::env::current_dir().map_err(|_| "version probe working directory unavailable")?,
        )
    }

    fn from_snapshot(
        mut env: Vec<(OsString, OsString)>,
        cwd: PathBuf,
    ) -> Result<Self, &'static str> {
        if cwd.as_os_str().as_encoded_bytes().len() > MAX_PATH_BYTES || !cwd.is_absolute() {
            return Err("version probe working directory exceeds path bound");
        }
        let mut bytes = 0;
        for (count, (name, value)) in env.iter().enumerate() {
            check_env_entry(name, value, count, &mut bytes)?;
        }
        env.sort_unstable();
        let mut hash = Sha256::new();
        for (name, value) in &env {
            for value in [name, value] {
                let bytes = value.as_encoded_bytes();
                hash.update(bytes.len().to_le_bytes());
                hash.update(bytes);
            }
        }
        let mut path = None;
        if let Some((_, paths)) = env.iter().find(|(name, _)| {
            name == "PATH"
                || (thegn_core::sandbox_backend::host_os()
                    == thegn_core::sandbox_backend::HostOs::Windows
                    && name
                        .to_str()
                        .is_some_and(|name| name.eq_ignore_ascii_case("PATH")))
        }) {
            // Validate the complete search input before choosing any candidate;
            // a truncated PATH must never be used to authorize a different CLI.
            for (count, dir) in std::env::split_paths(paths).enumerate() {
                if count >= MAX_SEARCH_DIRS
                    || dir.as_os_str().as_encoded_bytes().len() > MAX_PATH_BYTES
                {
                    return Err("version probe search path exceeds input bound");
                }
                let candidate = cwd.join(dir).join(CLI_NAME);
                if candidate.as_os_str().as_encoded_bytes().len() > MAX_PATH_BYTES {
                    return Err("version probe executable exceeds path bound");
                }
            }
            // Match the existing discovery order and file test. Preserve the
            // lexical executable path used by script/package entry.
            path = std::env::split_paths(paths)
                .map(|dir| cwd.join(dir).join(CLI_NAME))
                .find(|path| path.is_file());
        }
        let executable = path
            .map(|path| {
                let file = crate::platform::open_capability_identity(&path)
                    .map_err(|_| "version probe executable identity unavailable")?;
                let identity = same_file::Handle::from_file(file)
                    .map_err(|_| "version probe executable identity unavailable")?;
                let metadata = identity
                    .as_file()
                    .metadata()
                    .map_err(|_| "version probe executable metadata unavailable")?;
                let modified = metadata
                    .modified()
                    .map_err(|_| "version probe executable timestamp unavailable")?;
                Ok::<_, &'static str>(Executable {
                    path,
                    identity: Arc::new(identity),
                    bytes: metadata.len(),
                    modified,
                })
            })
            .transpose()?;
        Ok(Self {
            key: Key {
                executable,
                cwd,
                env_digest: hash.finalize().into(),
            },
            env,
        })
    }

    fn run(self, timeout: Duration) -> ProbeReport {
        let Some(executable) = self.key.executable else {
            return ProbeReport::unavailable(format!("`{CLI_NAME}` not found on PATH"));
        };
        let mut command = std::process::Command::new(&executable.path);
        command
            .arg("--version")
            .current_dir(&self.key.cwd)
            .env_clear()
            .envs(self.env);
        probe_command(&executable.path, command, timeout)
    }
}

struct Cached {
    report: ProbeReport,
    completed: Instant,
}
#[derive(Default)]
struct State {
    #[cfg(test)]
    waits: usize,
    observed: Option<Key>,
    generation: Arc<()>,
    running: Option<Arc<()>>,
    cached: Option<Cached>,
}
#[derive(Default)]
struct Cache {
    state: Mutex<State>,
    changed: Condvar,
}
impl Cache {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| {
            let mut state = poisoned.into_inner();
            // Poison never validates an old result or releases an active owner.
            state.cached = None;
            state.generation = Arc::new(());
            self.state.clear_poison();
            state
        })
    }

    fn get(
        &self,
        observe: impl Fn() -> Result<Inputs, &'static str>,
        run: impl FnOnce(Inputs, Duration) -> ProbeReport,
        now: impl Fn() -> Instant,
        budget: Duration,
    ) -> ProbeReport {
        let Some(deadline) = Instant::now().checked_add(budget) else {
            return degraded("version probe deadline unavailable");
        };
        loop {
            // Discovery, metadata and environment capture are all outside the
            // mutex, and happen again even when the last result was unavailable.
            let inputs = match observe() {
                Ok(inputs) => inputs,
                Err(reason) => return degraded(reason),
            };
            let key = inputs.key.clone();
            let instant = now();
            let mut state = self.lock();
            if state.observed.as_ref() != Some(&key) {
                state.observed = Some(key.clone());
                state.generation = Arc::new(());
                state.cached = None;
            }
            if let Some(cached) = &state.cached {
                let ttl = if cached.report.ready() {
                    READY_TTL
                } else {
                    FAILED_TTL
                };
                if instant
                    .checked_duration_since(cached.completed)
                    .is_some_and(|age| age < ttl)
                {
                    return cached.report.clone();
                }
                state.cached = None;
            }
            if state.running.is_some() {
                drop(inputs);
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return degraded("version probe already running; retry on next demand");
                }
                // Condvar releases the state mutex while waiting. A different
                // key invalidates the old generation but never detaches its owner.
                #[cfg(test)]
                {
                    state.waits += 1;
                }
                match self.changed.wait_timeout(state, remaining) {
                    Ok((state, waited)) => {
                        drop(state);
                        if waited.timed_out() {
                            return degraded("version probe already running; retry on next demand");
                        }
                    }
                    Err(poisoned) => {
                        let (mut state, _) = poisoned.into_inner();
                        state.cached = None;
                        state.generation = Arc::new(());
                        self.state.clear_poison();
                        drop(state);
                    }
                }
                continue;
            }
            let token = Arc::clone(&state.generation);
            state.running = Some(Arc::clone(&token));
            let reservation = Reservation {
                cache: self,
                token: Arc::clone(&token),
            };
            drop(state);
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return degraded("version probe deadline exceeded before admission");
            }
            let report = run(inputs, remaining);
            let latest = match observe() {
                Ok(inputs) => inputs,
                Err(reason) => return degraded(reason),
            };
            let completed = now();
            let mut state = self.lock();
            let current = Arc::ptr_eq(&state.generation, &token)
                && state.observed.as_ref() == Some(&key)
                && latest.key == key;
            if current {
                state.cached = Some(Cached {
                    report: report.clone(),
                    completed,
                });
            } else if Arc::ptr_eq(&state.generation, &token) {
                state.observed = Some(latest.key);
                state.generation = Arc::new(());
                state.cached = None;
            }
            drop(state);
            drop(reservation);
            return if current {
                report
            } else {
                degraded("version probe inputs changed; retry on next demand")
            };
        }
    }
}

struct Reservation<'a> {
    cache: &'a Cache,
    token: Arc<()>,
}
impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        let mut state = self.cache.lock();
        if state
            .running
            .as_ref()
            .is_some_and(|running| Arc::ptr_eq(running, &self.token))
        {
            state.running = None;
        }
        drop(state);
        self.cache.changed.notify_all();
    }
}

fn degraded(reason: &'static str) -> ProbeReport {
    ProbeReport {
        state: ProbeState::Degraded,
        executable: None,
        version: None,
        reason: Some(reason.into()),
    }
}

pub(super) fn probe() -> ProbeReport {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE
        .get_or_init(Cache::default)
        .get(Inputs::capture, Inputs::run, Instant::now, PROBE_TIMEOUT)
}

#[cfg(test)]
#[path = "devcontainer_probe_cache_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "platform/devcontainer_probe_execution_tests.rs"]
mod execution_tests;
