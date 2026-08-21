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
    let suitable = |b: Backend| backend_suitable(b, placement);
    let unsuitable_reason = |b: Backend| -> &'static str {
        if cfg!(windows) && b.is_oci() && b != Backend::Wsl {
            " on native Windows (Linux containers can't bind-mount the worktree \
             at its real path — use WSL2 for container sandboxes)"
        } else if b.is_host_toolchain() && !placement.is_local() {
            " on a non-local placement (a host-toolchain backend can't nest inside \
             ssh/k8s/provider — the placement is already the isolation boundary)"
        } else {
            " for this image mode"
        }
    };

    // Open the resolve phase so the splash's "sandbox" step names what the
    // resolver is probing (a wedged runtime probe otherwise freezes an opaque
    // spinner — see `output_with_timeout`'s detached reap). A no-op when no
    // progress sink is installed on this thread (CLI, tests).
    crate::progress::emit(crate::progress::SandboxPhase::Resolve);

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
                    crate::progress::emit(crate::progress::SandboxPhase::ResolveProbe {
                        backend: b.label().to_string(),
                    });
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
            crate::progress::emit(crate::progress::SandboxPhase::ResolveProbe {
                backend: b.label().to_string(),
            });
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

/// Whether `backend` can even be *considered* for `placement`, before probing
/// whether its runtime is present. Pure so the placement/backend matrix is
/// unit-tested without spawning a probe. Two rules:
///
///  - Native Windows declines OCI runtimes even when Docker/Podman Desktop is
///    installed: their Linux containers live in a WSL2 VM that can't bind-mount
///    the worktree at its real absolute path (git worktree metadata carries host
///    paths), breaking the sandbox contract. WSL as an explicit backend stays
///    eligible.
///  - A host-toolchain backend (bwrap, systemd-nspawn, win-native) is a LOCAL
///    isolation primitive — it wraps argv with host-namespace syscalls, so it
///    only means anything on the box thegn runs on. On a non-local placement
///    (ssh / k8s / provider) the placement ITSELF is the isolation boundary, so a
///    nested bwrap is meaningless — and probing for it over the remote exec
///    channel just answers `Unreachable` and stalls the resolver. Unsuitable, so
///    the chain skips straight past it to an in-placement runtime or a bare shell.
pub(crate) fn backend_suitable(b: Backend, placement: &Placement) -> bool {
    if cfg!(windows) && b.is_oci() && b != Backend::Wsl {
        return false;
    }
    match b {
        Backend::None => true,
        _ if b.is_oci() => true,
        _ if b.is_host_toolchain() => placement.is_local(),
        _ => false,
    }
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
        // Same suitability gate as `pick_backend`: never probe a host-toolchain
        // backend (bwrap, …) over a non-local transport — it can't run there, so
        // its `Unreachable` answer is noise that would both mislabel reachability
        // and re-incur the very remote probe the picker now skips.
        .filter(|b| *b != Backend::None && backend_suitable(*b, placement))
    {
        probed_any = true;
        if available(placement, b) != RuntimeProbe::Unreachable {
            return true;
        }
    }
    !probed_any
}

thread_local! {
    /// Per-resolution-pass probe memo (see [`probe_pass_guard`]). `Some` only
    /// while a pass guard is live; keyed like the global cache.
    static PASS_MEMO: std::cell::RefCell<Option<std::collections::HashMap<(String, Backend), RuntimeProbe>>> =
        const { std::cell::RefCell::new(None) };
}

/// Scope a **probe pass**: while the returned guard is alive, `available`
/// memoizes *every* result — **including `Unreachable`** — on the current thread,
/// so an already-unreachable placement is probed once per `(placement, backend)`
/// rather than re-probed for every candidate the resolver walks. That storm is
/// the multiplier behind a hung-transport stall: N candidates × M chain backends,
/// each independently re-probing because the global cache (rightly) refuses to
/// persist `Unreachable`. The memo lives only for the pass — thread-local,
/// dropped with the guard — so the "never strand a host across sessions" rule is
/// intact: the next open starts with an empty pass and re-probes. Nesting is
/// safe; only the outermost guard installs and clears the memo.
#[must_use]
pub fn probe_pass_guard() -> ProbePass {
    let outermost = PASS_MEMO.with(|m| {
        let mut slot = m.borrow_mut();
        if slot.is_none() {
            *slot = Some(std::collections::HashMap::new());
            true
        } else {
            false
        }
    });
    ProbePass { outermost }
}

/// RAII guard for a [`probe_pass_guard`] scope. Clears the pass memo on drop
/// (only if it installed one), so an early return or a panic can't leak a stale
/// `Unreachable` into a later pass.
pub struct ProbePass {
    outermost: bool,
}

impl Drop for ProbePass {
    fn drop(&mut self) {
        if self.outermost {
            PASS_MEMO.with(|m| *m.borrow_mut() = None);
        }
    }
}

fn pass_memo_get(key: &(String, Backend)) -> Option<RuntimeProbe> {
    PASS_MEMO.with(|m| m.borrow().as_ref().and_then(|map| map.get(key).copied()))
}

fn pass_memo_put(key: &(String, Backend), v: RuntimeProbe) {
    PASS_MEMO.with(|m| {
        if let Some(map) = m.borrow_mut().as_mut() {
            map.insert(key.clone(), v);
        }
    });
}

/// Three-state availability of `backend`'s runtime in this placement (locally on
/// PATH, or probed through the placement's control primitive: ssh / kubectl exec
/// / provider). `Unreachable` (remote transport failed) is distinct from `Absent`
/// so a reachable remote is never silently degraded to `Backend::None`.
///
/// **Memoized** (D3): probe once per `(placement, backend)`; cache `Present`
/// permanently, `Absent` only 30s (a permanent `false` stranded a remote host),
/// and **never** cache `Unreachable` globally — a transient blip must not strand
/// the host across sessions. Within a single [`probe_pass_guard`] scope, though,
/// even `Unreachable` is memoized so one wedged transport isn't re-probed for
/// every candidate in the pass.
pub(crate) fn available(placement: &Placement, backend: Backend) -> RuntimeProbe {
    type AvailCache = std::sync::Mutex<
        std::collections::HashMap<(String, Backend), (RuntimeProbe, std::time::Instant)>,
    >;
    static CACHE: std::sync::OnceLock<AvailCache> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let key = (format!("{placement:?}"), backend);
    // Pass memo first: freshest within a resolve pass, and the only place an
    // `Unreachable` is remembered (dedupes the per-candidate probe storm).
    if let Some(v) = pass_memo_get(&key) {
        return v;
    }
    if let Some(&(v, at)) = cache.lock().unwrap().get(&key)
        && cache_is_fresh(v, at)
    {
        pass_memo_put(&key, v);
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
            .insert(key.clone(), (v, std::time::Instant::now()));
    }
    // Always record in the pass memo (incl. `Unreachable`) so this pass doesn't
    // re-probe the same (placement, backend). No-op when no pass is active.
    pass_memo_put(&key, v);
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

/// The uncached availability probe (subprocess / PATH / remote). See `available`.
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

    // Apple's `container` is a macOS-native runtime (its per-container Linux VM
    // is what earns `IsolationClass::GuestKernel`). Its binary name is generic
    // enough that a bare PATH probe could match something unrelated, so LOCALLY
    // gate on the OS the same way the win-native backends are — otherwise
    // putting `"apple"` in the default chain would change behaviour on a Linux
    // box that happens to ship some other `container` executable. Remote
    // placements fall through to the normal PATH probe, so a macOS ssh target
    // still resolves it.
    if placement.is_local() && backend == Backend::Apple && !cfg!(target_os = "macos") {
        return RuntimeProbe::Absent;
    }

    placement.probe_runtime(backend.binary())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pass_memo_dedupes_within_scope_and_clears_after() {
        let key = ("Provider(x)".to_string(), Backend::Podman);
        // No pass active: get/put are no-ops (the global cache path is unaffected).
        assert_eq!(pass_memo_get(&key), None);
        pass_memo_put(&key, RuntimeProbe::Unreachable);
        assert_eq!(pass_memo_get(&key), None, "no memo without a pass guard");

        {
            let _pass = probe_pass_guard();
            assert_eq!(pass_memo_get(&key), None, "pass starts empty");
            // Within the pass, even Unreachable is remembered (the whole point).
            pass_memo_put(&key, RuntimeProbe::Unreachable);
            assert_eq!(pass_memo_get(&key), Some(RuntimeProbe::Unreachable));
            // Nested guard doesn't reset the outer memo.
            {
                let _inner = probe_pass_guard();
                assert_eq!(
                    pass_memo_get(&key),
                    Some(RuntimeProbe::Unreachable),
                    "nested guard shares the outer pass"
                );
            }
            assert_eq!(
                pass_memo_get(&key),
                Some(RuntimeProbe::Unreachable),
                "inner drop doesn't clear the outer pass"
            );
        }
        // Outermost guard dropped ⇒ memo gone, so a later pass re-probes.
        assert_eq!(pass_memo_get(&key), None, "pass memo cleared on scope exit");
    }

    #[test]
    fn host_toolchain_backends_are_local_only() {
        use crate::placement::{Placement, ProviderPlacement};
        let provider = Placement::Provider(ProviderPlacement {
            provider: "machine0".into(),
            id: "thegn-thegn-ihetss".into(),
            interactive_prefix: vec![],
            control_prefix: vec![],
            up_command: vec![],
            down_command: vec![],
        });
        // bwrap: usable locally, meaningless on a provider (the placement is the
        // isolation) — so the resolver never probes it there and never stalls.
        assert!(backend_suitable(Backend::Bwrap, &Placement::Local));
        assert!(!backend_suitable(Backend::Bwrap, &provider));
        assert!(!backend_suitable(Backend::Systemd, &provider));
        // OCI runtimes DO nest in a placement (a container in the sprite/pod), so
        // they stay eligible remotely.
        if !cfg!(windows) {
            assert!(backend_suitable(Backend::Podman, &provider));
            assert!(backend_suitable(Backend::Docker, &provider));
        }
        // `none` (run natively in the placement) is always eligible.
        assert!(backend_suitable(Backend::None, &provider));
        assert!(backend_suitable(Backend::None, &Placement::Local));
    }

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
