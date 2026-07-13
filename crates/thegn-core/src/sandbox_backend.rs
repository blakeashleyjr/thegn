//! Backend **selection + availability probing** for a [`Placement`] — the
//! decision layer between `[sandbox]` config and a concrete [`Backend`]. Split
//! out of `sandbox.rs` (god-file ratchet) as a coherent unit: `pick_backend`
//! walks the config/chain, `available`/`available_probe` do the memoized
//! per-`(placement, backend)` probe, and `placement_reachable` distinguishes an
//! unreachable remote from one that merely lacks a runtime.
//!
//! The three-state [`RuntimeProbe`] is the crux: a remote SSH probe that fails
//! at the *transport* (ssh exit 255, killed connection) must read as
//! `Unreachable`, NOT `Absent` — otherwise a reachable host with podman
//! installed gets silently degraded to `Backend::None`, which for a remote
//! placement ships a `cd <local-path>` to the wrong machine.

use crate::config::{OnMissing, SandboxConfig};
use crate::placement::{Placement, RuntimeProbe};
use crate::sandbox::{Backend, backend_prefix, run_local_output};

/// Resolve the backend for `placement` from the config/chain. `Some(b)` is a
/// decision (including `Some(Backend::None)` = run a bare/host shell); `None`
/// means **undecidable because a remote host was unreachable** — the caller must
/// halt with an "unreachable" message rather than degrade to a host shell (which,
/// for a remote placement, would ship a `cd <local-path>` to the wrong machine).
pub(crate) fn pick_backend(cfg: &SandboxConfig, placement: &Placement) -> Option<Backend> {
    let suitable = |b: Backend| -> bool {
        // Native Windows declines OCI runtimes even when Docker/Podman Desktop
        // is installed: their Linux containers live in a WSL2 VM that cannot
        // bind-mount the worktree at its real absolute path (git worktree
        // metadata carries host paths), breaking the sandbox contract. WSL as
        // an explicit backend stays eligible; win-native scoping is suitable.
        if cfg!(windows) && b.is_oci() && b != Backend::Wsl {
            return false;
        }
        match b {
            Backend::None => true,
            _ if b.is_oci() => true,
            _ if b.is_host_toolchain() => true,
            _ => false,
        }
    };
    let unsuitable_reason = |b: Backend| -> &'static str {
        if cfg!(windows) && b.is_oci() && b != Backend::Wsl {
            " on native Windows (Linux containers can't bind-mount the worktree \
             at its real path — use WSL2 for container sandboxes)"
        } else {
            " for this image mode"
        }
    };

    // A remote probe that returned `Unreachable` means we couldn't learn what
    // runtimes exist. If the chain then finds nothing, we must NOT silently pick
    // `Backend::None` for the remote — that ships a bare-shell `cd <local-path>`
    // to a host we never reached. Track it and return `None` (undecidable).
    let mut saw_unreachable = false;

    // Explicit backend: use it if suitable+present; otherwise warn and fall
    // through to the chain. `Auto` falls straight through to the chain.
    if let Some(explicit) = Backend::from_config(cfg.backend) {
        match explicit {
            Backend::None => return Some(Backend::None),
            b => {
                if suitable(b) {
                    match available(placement, b) {
                        RuntimeProbe::Present => return Some(b),
                        RuntimeProbe::Unreachable => saw_unreachable = true,
                        RuntimeProbe::Absent => {}
                    }
                }
                on_missing(
                    cfg,
                    &format!(
                        "sandbox backend '{}' unavailable{}; trying the chain",
                        cfg.backend,
                        if suitable(b) {
                            ""
                        } else {
                            unsuitable_reason(b)
                        }
                    ),
                );
            }
        }
    }

    for name in &cfg.backend_chain {
        let Some(b) = Backend::parse(name) else {
            continue;
        };
        let is_win_native = b == Backend::WinAppContainer || b == Backend::WinJobObject;
        if b == Backend::None {
            // Don't quietly pick the host-shell terminal for a remote we couldn't
            // reach — surface the unreachable host so the caller halts.
            if !placement.is_local() && saw_unreachable {
                return None;
            }
            if !is_win_native {
                on_missing(
                    cfg,
                    "sandbox: no container backend available; running on the host",
                );
            }
            return Some(Backend::None);
        }
        if suitable(b) {
            match available(placement, b) {
                RuntimeProbe::Present => return Some(b),
                RuntimeProbe::Unreachable => saw_unreachable = true,
                RuntimeProbe::Absent => {}
            }
        }
    }
    // Chain didn't include "none". A reachable host with no runtime still falls
    // back to the host shell; an unreachable remote stays undecidable.
    if !placement.is_local() && saw_unreachable {
        return None;
    }
    on_missing(
        cfg,
        "sandbox: no usable backend in chain; running on the host",
    );
    Some(Backend::None)
}

fn on_missing(cfg: &SandboxConfig, what: &str) {
    match cfg.on_missing {
        OnMissing::Fail => crate::msg::die(what),
        // "prompt" is treated as "warn" here; the picker layer can offer choices.
        _ => crate::msg::warn(what),
    }
}

/// Did `placement`'s control transport reach the host while probing the runtime
/// backends in `chain`? A local placement is always reachable; a remote one is
/// reachable if any suitable-backend probe returned a definite `Present`/`Absent`
/// (not `Unreachable`). A placement that probed nothing (no suitable backend in
/// the chain) is treated as reachable — absence of evidence isn't "down". Rides
/// the probe cache, so it's cheap once `pick_backend` has already probed. Used to
/// choose "host unreachable" vs "no runtime" in a `SandboxHalt` message.
pub fn placement_reachable(placement: &Placement, chain: &[String]) -> bool {
    if placement.is_local() {
        return true;
    }
    let mut probed_any = false;
    for b in chain
        .iter()
        .filter_map(|n| Backend::parse(n))
        .filter(|b| *b != Backend::None && (b.is_oci() || b.is_host_toolchain()))
    {
        probed_any = true;
        if available(placement, b) != RuntimeProbe::Unreachable {
            return true;
        }
    }
    !probed_any
}

/// Three-state availability of `backend`'s runtime in this placement (locally on
/// PATH, or probed through the placement's control primitive: ssh / kubectl exec
/// / provider). `Unreachable` (remote transport failed) is distinct from `Absent`
/// so a reachable remote is never silently degraded to `Backend::None`.
///
/// **Memoized** (D3): probe once per `(placement, backend)`; cache `Present`
/// permanently, `Absent` only 30s (a permanent `false` stranded a remote host),
/// and **never** cache `Unreachable` — a transient blip must not strand the host.
pub(crate) fn available(placement: &Placement, backend: Backend) -> RuntimeProbe {
    type AvailCache = std::sync::Mutex<
        std::collections::HashMap<(String, Backend), (RuntimeProbe, std::time::Instant)>,
    >;
    static CACHE: std::sync::OnceLock<AvailCache> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let key = (format!("{placement:?}"), backend);
    if let Some(&(v, at)) = cache.lock().unwrap().get(&key)
        && cache_is_fresh(v, at)
    {
        return v;
    }
    // A remote probe rides ssh: retry an `Unreachable` answer through a short
    // backoff before believing it — a one-off transport flap must not abort a
    // provisioning run with "runtime not detected" (the false-negative bug).
    let v = if placement.is_local() {
        available_probe(placement, backend)
    } else {
        use crate::progress::{SandboxPhase, emit};
        emit(SandboxPhase::Connect {
            host: placement.label(),
        });
        let policy = crate::retry::ReconnectPolicy::probe();
        let mut attempt: u32 = 0;
        let v = probe_with_retry(&policy, &mut std::thread::sleep, &mut || {
            attempt += 1;
            if attempt > 1 {
                emit(SandboxPhase::ConnectRetry {
                    attempt,
                    max: policy.max_attempts,
                });
            }
            available_probe(placement, backend)
        });
        if v == RuntimeProbe::Unreachable {
            emit(SandboxPhase::PhaseFailed {
                err: format!("{} unreachable", placement.label()),
            });
        } else {
            emit(SandboxPhase::PhaseDone);
        }
        v
    };
    if avail_cacheable(v) {
        cache
            .lock()
            .unwrap()
            .insert(key, (v, std::time::Instant::now()));
    }
    v
}

/// Retry an `Unreachable` probe per the policy before accepting it; a definite
/// `Present`/`Absent` returns immediately. Pure loop over injected closures —
/// the sleep is the only side effect (unit-tested with a recording sleeper).
fn probe_with_retry(
    policy: &crate::retry::ReconnectPolicy,
    sleep: &mut dyn FnMut(std::time::Duration),
    probe: &mut dyn FnMut() -> RuntimeProbe,
) -> RuntimeProbe {
    let mut attempt: u32 = 1;
    loop {
        let v = probe();
        if v != RuntimeProbe::Unreachable || attempt >= policy.max_attempts {
            return v;
        }
        let Some(delay) = policy.backoff(attempt) else {
            return v;
        };
        sleep(delay);
        attempt += 1;
    }
}

/// Cache policy for a memoized probe result: `Present` is stored forever,
/// `Absent` is honored for 30s, `Unreachable` is never stored. Pure — unit-tested.
fn avail_cacheable(v: RuntimeProbe) -> bool {
    !matches!(v, RuntimeProbe::Unreachable)
}

/// Is a cached `(result, stamped_at)` still usable? `Present` never expires;
/// `Absent` expires after 30s; `Unreachable` is never cached so it can't appear.
fn cache_is_fresh(v: RuntimeProbe, at: std::time::Instant) -> bool {
    match v {
        RuntimeProbe::Present => true,
        RuntimeProbe::Absent => at.elapsed() < std::time::Duration::from_secs(30),
        RuntimeProbe::Unreachable => false,
    }
}

/// The uncached availability probe (subprocess / PATH / remote). See [`available`].
fn available_probe(placement: &Placement, backend: Backend) -> RuntimeProbe {
    let from_bool = |b: bool| {
        if b {
            RuntimeProbe::Present
        } else {
            RuntimeProbe::Absent
        }
    };
    // Rootful podman can't be detected by a bare PATH probe (it needs `sudo -n
    // podman version`); only meaningful locally.
    if placement.is_local() && backend == Backend::PodmanRootful {
        return from_bool(run_local_output(&backend_prefix(backend), &["version"]).is_some());
    }

    if placement.is_local()
        && (backend == Backend::WinAppContainer || backend == Backend::WinJobObject)
    {
        return from_bool(cfg!(windows));
    }

    if !placement.is_local()
        && (backend == Backend::WinAppContainer || backend == Backend::WinJobObject)
    {
        return RuntimeProbe::Absent;
    }

    placement.probe_runtime(backend.binary())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreachable_probe_is_never_cached_present_forever_absent_ttl() {
        assert!(avail_cacheable(RuntimeProbe::Present));
        assert!(avail_cacheable(RuntimeProbe::Absent));
        assert!(
            !avail_cacheable(RuntimeProbe::Unreachable),
            "a transient unreachable must not be memoized"
        );
        let now = std::time::Instant::now();
        assert!(
            cache_is_fresh(RuntimeProbe::Present, now),
            "present never expires"
        );
        assert!(
            cache_is_fresh(RuntimeProbe::Absent, now),
            "fresh absent honored"
        );
        let stale = now - std::time::Duration::from_secs(31);
        assert!(
            !cache_is_fresh(RuntimeProbe::Absent, stale),
            "absent expires after 30s so a runtime install is re-detected"
        );
        assert!(
            !cache_is_fresh(RuntimeProbe::Unreachable, now),
            "unreachable is never stored, so never considered fresh"
        );
    }

    #[test]
    fn probe_retry_rides_through_a_transient_flap() {
        // Unreachable → Unreachable → Present: the flap is retried away.
        let policy = crate::retry::ReconnectPolicy::probe();
        let mut calls = 0;
        let mut slept = Vec::new();
        let v = probe_with_retry(&policy, &mut |d| slept.push(d), &mut || {
            calls += 1;
            if calls < 3 {
                RuntimeProbe::Unreachable
            } else {
                RuntimeProbe::Present
            }
        });
        assert_eq!(v, RuntimeProbe::Present);
        assert_eq!(calls, 3);
        assert_eq!(slept.len(), 2, "slept between attempts");
    }

    #[test]
    fn probe_retry_definite_answer_returns_immediately() {
        let policy = crate::retry::ReconnectPolicy::probe();
        let mut calls = 0;
        let v = probe_with_retry(&policy, &mut |_| panic!("no sleep"), &mut || {
            calls += 1;
            RuntimeProbe::Absent
        });
        assert_eq!(v, RuntimeProbe::Absent);
        assert_eq!(calls, 1, "a definite answer needs no retry");
    }

    #[test]
    fn probe_retry_gives_up_after_budget() {
        let policy = crate::retry::ReconnectPolicy::probe(); // 3 attempts
        let mut calls = 0;
        let v = probe_with_retry(&policy, &mut |_| {}, &mut || {
            calls += 1;
            RuntimeProbe::Unreachable
        });
        assert_eq!(
            v,
            RuntimeProbe::Unreachable,
            "still unreachable after budget"
        );
        assert_eq!(calls, 3, "exactly max_attempts probes");
    }
}
