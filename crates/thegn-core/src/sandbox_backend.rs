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
        if b.is_host_toolchain() && !placement.is_local() {
            " on a non-local placement (a host-toolchain backend can't nest inside \
             ssh/k8s/provider — the placement is already the isolation boundary)"
        } else if placement.is_local() && !backend_runs_on(b, host_os()) {
            // The OS gate. Without naming it, a Mac reported bwrap as unavailable
            // "for this image mode", which sends the reader looking for a config
            // problem that doesn't exist.
            match host_os() {
                HostOs::MacOs => " on macOS",
                HostOs::Linux => " on Linux",
                HostOs::Windows => " on Windows",
                HostOs::Other => " on this OS",
            }
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
                    &host_fallback_msg(cfg, placement, "sandbox: no container backend available"),
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
        &host_fallback_msg(cfg, placement, "sandbox: no usable backend in chain"),
    );
    Some(Backend::None)
}

/// Whether `backend` can even be *considered* for `placement`, before probing
/// whether its runtime is present. Pure so the placement/backend matrix is
/// unit-tested without spawning a probe. One rule:
///
///  - A host-toolchain backend (bwrap, systemd-nspawn, win-native) is a LOCAL
///    isolation primitive — it wraps argv with host-namespace syscalls, so it
///    only means anything on the box thegn runs on. On a non-local placement
///    (ssh / k8s / provider) the placement ITSELF is the isolation boundary, so a
///    nested bwrap is meaningless — and probing for it over the remote exec
///    channel just answers `Unreachable` and stalls the resolver. Unsuitable, so
///    the chain skips straight past it to an in-placement runtime or a bare shell.
pub(crate) fn backend_suitable(b: Backend, placement: &Placement) -> bool {
    backend_suitable_on(b, placement, host_os())
}

/// The OS a backend is being considered for, as a value rather than a `cfg!` —
/// so the Linux, macOS and Windows arms are all unit-testable from one host, the
/// same idiom `thegn_svc::ipc::IpcEndpoint::classify(path, windows)` uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOs {
    Linux,
    MacOs,
    Windows,
    Other,
}

/// The OS this binary was built for.
pub const fn host_os() -> HostOs {
    if cfg!(target_os = "linux") {
        HostOs::Linux
    } else if cfg!(target_os = "macos") {
        HostOs::MacOs
    } else if cfg!(windows) {
        HostOs::Windows
    } else {
        HostOs::Other
    }
}

/// Whether `b` can run on `os` **at all**, independent of whether its runtime is
/// installed. Purely a property of the backend and the OS.
///
/// This is what stops the resolver spending probes on the impossible: without it
/// a macOS box walks `bwrap` (a Linux namespace tool) and `jobobject` (a Windows
/// API) on every pane spawn, and a Linux box walks `apple` and `wsl`. Each miss
/// is a real syscall/exec, and — because a local `Absent` was only cached for 30s
/// — it was paid again every half minute for the life of the session.
pub(crate) fn backend_runs_on(b: Backend, os: HostOs) -> bool {
    match b {
        // Portable: no OS opinion beyond having the runtime.
        Backend::None
        | Backend::Podman
        | Backend::PodmanRootful
        | Backend::Docker
        | Backend::Smol => true,
        // Linux host-namespace primitives.
        Backend::Bwrap | Backend::Systemd => os == HostOs::Linux,
        // Apple's `container` is macOS-native (its per-container Linux VM is what
        // earns `IsolationClass::GuestKernel`). Gating here rather than in the
        // probe also stops a Linux box that ships some unrelated `container`
        // executable from matching a bare PATH probe.
        Backend::Apple => os == HostOs::MacOs,
        // Windows-native.
        Backend::Wsl | Backend::WinAppContainer | Backend::WinJobObject => os == HostOs::Windows,
    }
}

/// [`backend_suitable`] with the OS explicit — pure, so every platform arm is
/// covered by tests on a single host.
pub(crate) fn backend_suitable_on(b: Backend, placement: &Placement, os: HostOs) -> bool {
    // A backend that can't run on this OS is never a candidate, local or remote…
    // except across a placement boundary: a remote host may be a different OS
    // entirely, so only gate the OS for LOCAL placements.
    if placement.is_local() && !backend_runs_on(b, os) {
        return false;
    }
    // OCI on native Windows was declined for two separate reasons, and BOTH are
    // now solved — which is the only reason this gate is gone.
    //
    // Mount *destinations*: `Mount` carries `host`/`dest` separately, and
    // `sandbox::container_path` maps `C:\…` into the deterministic
    // `/mnt/<drive>/…` tree everything inside the container is composed from. A
    // Windows `podman.exe`/`docker.exe` translates the host half of `-v` itself,
    // so the pair lines up.
    //
    // Git *metadata*: every thegn tab is a linked worktree whose `.git` is a
    // pointer file carrying an ABSOLUTE host path. `crate::sandbox_gitshim`
    // binds rewritten `.git` and `gitdir` pointers so git resolves under the
    // mapping, and pins `<git-common>/worktrees` read-only (own entry
    // overmounted rw) so an in-container `git worktree prune` cannot delete a
    // sibling tab's metadata. Both halves are covered end to end by
    // `tests/sandbox_gitshim_e2e.rs` against a real podman.
    //
    // Whether a runtime is actually installed is `available_probe`'s question,
    // not this one — an uninstalled podman simply falls through the chain.
    match b {
        Backend::None => true,
        _ if b.is_oci() => true,
        _ if b.is_host_toolchain() => placement.is_local(),
        _ => false,
    }
}

/// "…; running on the host", plus the actionable part when a runtime is sitting
/// right there but stopped.
///
/// Falling back to the host silently downgrades the security boundary, and the
/// most common reason is a service the user already has installed and could
/// start in one command. Saying only "no container backend available" sends
/// someone off to install software they have — so name the stopped ones.
fn host_fallback_msg(cfg: &SandboxConfig, placement: &Placement, lead: &str) -> String {
    let down: Vec<&'static str> = cfg
        .backend_chain
        .iter()
        .filter_map(|n| Backend::parse(n))
        .filter(|b| *b != Backend::None && backend_suitable(*b, placement))
        // Installed, yet the probe says no ⇒ its daemon/service isn't answering.
        .filter(|b| {
            backend_installed_locally(*b) && available(placement, *b) != RuntimeProbe::Present
        })
        .map(|b| b.label())
        .collect();
    if down.is_empty() {
        return format!("{lead}; running on the host (no kernel boundary)");
    }
    let (subject, verb) = if down.len() == 1 {
        (down[0].to_string(), "start it")
    } else {
        (down.join(", "), "start one")
    };
    format!(
        "{lead}; running on the host (no kernel boundary). {subject} installed but \
         not running — {verb} for a real sandbox, or see `thegn doctor`"
    )
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

type AvailCache = std::sync::Mutex<
    std::collections::HashMap<(String, Backend), (RuntimeProbe, std::time::Instant)>,
>;

/// The process-wide probe memo. Module-level (rather than a static inside
/// `available`) so [`clear_probe_cache`] can reach it.
fn avail_cache() -> &'static std::sync::OnceLock<AvailCache> {
    static CACHE: std::sync::OnceLock<AvailCache> = std::sync::OnceLock::new();
    &CACHE
}

/// Drop every memoized probe result, so the next `available` re-asks the OS.
///
/// The counterpart to caching a local `Absent` forever: that is right for a
/// running process making a selection, and wrong the moment a user goes and
/// starts the runtime we told them to start. Any surface offering a "re-check"
/// must call this first, or it will cheerfully re-render the stale answer it
/// just told the user to fix.
pub fn clear_probe_cache() {
    if let Some(cache) = avail_cache().get() {
        cache.lock().unwrap().clear();
    }
    PASS_MEMO.with(|m| {
        if let Some(map) = m.borrow_mut().as_mut() {
            map.clear();
        }
    });
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
    let cache =
        avail_cache().get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let key = (format!("{placement:?}"), backend);
    // Pass memo first: freshest within a resolve pass, and the only place an
    // `Unreachable` is remembered (dedupes the per-candidate probe storm).
    if let Some(v) = pass_memo_get(&key) {
        return v;
    }
    if let Some(&(v, at)) = cache.lock().unwrap().get(&key)
        && cache_is_fresh(v, at, placement.is_local())
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
/// `Unreachable` is never cached so it can't appear. `Absent` depends on where:
///
/// - **remote** — expires after 30s. That window is load-bearing: caching a
///   remote `Absent` forever once stranded a host that was only briefly down, so
///   a remote must always get another chance.
/// - **local** — never expires. Nothing that makes a local backend absent (the
///   wrong OS, no binary on PATH, a stopped daemon) resolves itself mid-session
///   without the user acting, and re-asking cost a fresh subprocess every 30
///   seconds for the life of the process — the "it fails over and over" half of
///   the broken-first-run report. A user who *does* start their runtime gets the
///   new answer from the explicit re-probe in the support report, not from a
///   timer.
fn cache_is_fresh(v: RuntimeProbe, at: std::time::Instant, local: bool) -> bool {
    match v {
        RuntimeProbe::Present => true,
        RuntimeProbe::Absent => local || at.elapsed() < std::time::Duration::from_secs(30),
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
    // Win-native backends are OS APIs, not binaries on PATH — but "the OS has
    // the API" is not what this probe answers. It answers "selecting this
    // backend contains a pane", and for these two it does not: nothing on the
    // pane spawn path assigns the child to a Job Object or an AppContainer.
    // `enter_argv`'s own comment says as much ("we could intercept and wrap in
    // a job object") — it is aspirational, like the `wsl` backend.
    //
    // Reporting `Present` was doubly wrong. It let `doctor` advertise a
    // containment boundary that is never applied — a security claim, not a
    // cosmetic one — and because a "present" backend produces a `SandboxSpec`,
    // it routed every pane through `enter_argv`, which re-invoked the POSIX
    // command string through `powershell -NoProfile -Command`. Panes died on a
    // PowerShell parse error and crash-looped until the guard gave up.
    //
    // `Absent` falls the chain through to `host`, which is the honest state and
    // the same degradation a Linux box without podman already gets. Flip this
    // back the moment pane spawn actually joins the job.
    if backend == Backend::WinAppContainer || backend == Backend::WinJobObject {
        return RuntimeProbe::Absent;
    }

    // LOCAL: a runtime with a daemon/service is only "present" if that service
    // answers. See `sandbox::liveness_argv` for why PATH presence is not enough
    // (a stopped dockerd and an unstarted Apple `container` both pass a PATH
    // probe, get selected, and then fail every pane).
    //
    // Backends with no liveness verb — bwrap/systemd, and the not-yet-verified
    // smol/wsl — keep the PATH probe, so this narrows nothing that worked before.
    //
    // Deliberately NOT applied to remote placements: `probe_runtime` there is a
    // single bounded round-trip over ssh whose three-state answer distinguishes
    // Unreachable from Absent, and running a second remote command per backend
    // would multiply the very probe storm `probe_pass_guard` exists to damp.
    if placement.is_local()
        && let Some(args) = crate::sandbox::liveness_argv(backend)
    {
        return from_bool(run_local_output(&backend_prefix(backend), &args).is_some());
    }

    placement.probe_runtime(backend.binary())
}

/// Is `backend`'s client binary on PATH, ignoring whether its service is up?
///
/// Only the support report needs this: selection folds installed-but-down into
/// `Absent` (the chain wants one bit — usable or not), while a human staring at
/// `thegn doctor` needs "installed, not running" separated from "not installed",
/// because those have completely different remedies.
pub(crate) fn backend_installed_locally(backend: Backend) -> bool {
    crate::util::have(backend.binary())
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
        // bwrap: usable locally ON LINUX, meaningless on a provider (the
        // placement is the isolation) — so the resolver never probes it there
        // and never stalls. The OS is explicit because locality alone is not
        // enough: a Mac used to consider bwrap "suitable" and probe it forever.
        assert!(backend_suitable_on(
            Backend::Bwrap,
            &Placement::Local,
            HostOs::Linux
        ));
        for os in [HostOs::Linux, HostOs::MacOs, HostOs::Windows] {
            assert!(!backend_suitable_on(Backend::Bwrap, &provider, os));
            assert!(!backend_suitable_on(Backend::Systemd, &provider, os));
        }
        // OCI runtimes DO nest in a placement (a container in the sprite/pod), so
        // they stay eligible remotely — on a non-local placement the OS gate does
        // not apply, because the remote may be a different OS entirely.
        for os in [HostOs::Linux, HostOs::MacOs] {
            assert!(backend_suitable_on(Backend::Podman, &provider, os));
            assert!(backend_suitable_on(Backend::Docker, &provider, os));
        }
        // `none` (run natively in the placement) is always eligible.
        assert!(backend_suitable(Backend::None, &provider));
        assert!(backend_suitable(Backend::None, &Placement::Local));
    }

    /// OCI is selectable on native Windows — but ONLY because the two things
    /// that used to make it broken are both fixed: mount destinations are mapped
    /// by `sandbox::container_path`, and git metadata resolves under that
    /// mapping via `crate::sandbox_gitshim`.
    ///
    /// This is normative (`specs/platform-windows`). The gate was once removed
    /// with the metadata half unsolved and no test to catch it, which shipped a
    /// sandbox where `git` reported `not a git repository: (null)` and an
    /// in-container `git worktree prune` deleted every sibling tab's metadata.
    /// So this test deliberately asserts the *coupling*: eligibility here is
    /// only legitimate while the shim exists, and
    /// `tests/sandbox_gitshim_e2e.rs` is what proves the shim actually works
    /// against a real runtime.
    #[test]
    fn oci_is_eligible_everywhere_now_that_git_metadata_resolves() {
        use crate::placement::{Placement, ProviderPlacement};
        let remote = Placement::Provider(ProviderPlacement {
            provider: "machine0".into(),
            id: "thegn-thegn-ihetss".into(),
            interactive_prefix: vec![],
            control_prefix: vec![],
            up_command: vec![],
            down_command: vec![],
        });
        for b in [
            Backend::Podman,
            Backend::PodmanRootful,
            Backend::Docker,
            Backend::Smol,
        ] {
            for os in [HostOs::Linux, HostOs::MacOs, HostOs::Windows] {
                assert!(
                    backend_suitable_on(b, &Placement::Local, os),
                    "{b:?} must be eligible locally on {os:?}"
                );
                assert!(
                    backend_suitable_on(b, &remote, os),
                    "{b:?} must be eligible across a placement from {os:?}"
                );
            }
        }
        // The shim is the load-bearing half. If it ever stops planning mounts
        // for a Windows-shaped worktree, eligibility above becomes the same
        // false claim it was before — so assert it plans one.
        let shim = crate::sandbox_gitshim::plan_with(
            std::path::Path::new(r"C:\Users\u\wt"),
            std::path::Path::new(r"C:\Users\u\repo\.git"),
            std::path::Path::new("/shim"),
            &crate::sandbox_gitshim::GitFacts {
                pointer: "C:/Users/u/repo/.git/worktrees/wt".into(),
                commondir_absolute: false,
            },
            crate::sandbox::map_windows_path,
        );
        assert!(
            shim.is_some_and(|s| !s.mounts.is_empty()),
            "OCI-on-Windows is only sound while the gitdir shim plans mounts"
        );
    }

    #[test]
    fn only_daemon_backed_runtimes_get_a_liveness_probe() {
        use crate::sandbox::liveness_argv;
        // Client/daemon runtimes: PATH presence is not usability, so each must
        // have a verb that actually talks to the service. This is the bug — a
        // stopped dockerd and an unstarted Apple `container` both pass a PATH
        // probe, get selected, then fail every pane.
        for b in [
            Backend::Podman,
            Backend::PodmanRootful,
            Backend::Docker,
            Backend::Apple,
        ] {
            assert!(
                liveness_argv(b).is_some(),
                "{b:?} has a daemon/service, so it needs a liveness verb"
            );
        }
        assert_eq!(
            liveness_argv(Backend::Apple),
            Some(vec!["system", "status"])
        );
        assert_eq!(liveness_argv(Backend::Docker), Some(vec!["version"]));

        // Process wrappers have no daemon: being on PATH IS being usable, so a
        // liveness verb would be a pointless subprocess on every probe.
        for b in [Backend::Bwrap, Backend::Systemd] {
            assert_eq!(
                liveness_argv(b),
                None,
                "{b:?} is a process wrapper with nothing to be 'running'"
            );
        }
        // Unverified runtimes keep the old PATH behaviour rather than a guess.
        assert_eq!(liveness_argv(Backend::Smol), None);
        assert_eq!(liveness_argv(Backend::Wsl), None);
    }

    #[test]
    fn os_gate_keeps_the_resolver_off_impossible_backends() {
        // The macOS first-run bug: a Mac walked bwrap (Linux namespaces) and
        // jobobject (a Windows API) on every pane spawn, and a Linux box walks
        // apple/wsl. Locality alone never ruled any of it out.
        let cases = [
            (Backend::Bwrap, HostOs::Linux, true),
            (Backend::Bwrap, HostOs::MacOs, false),
            (Backend::Bwrap, HostOs::Windows, false),
            (Backend::Apple, HostOs::MacOs, true),
            (Backend::Apple, HostOs::Linux, false),
            (Backend::WinJobObject, HostOs::Windows, true),
            (Backend::WinJobObject, HostOs::Linux, false),
            (Backend::Wsl, HostOs::Windows, true),
            (Backend::Wsl, HostOs::Linux, false),
            // Portable runtimes keep no OS opinion.
            (Backend::Docker, HostOs::Linux, true),
            (Backend::Docker, HostOs::MacOs, true),
            (Backend::Podman, HostOs::MacOs, true),
        ];
        for (b, os, want) in cases {
            assert_eq!(
                backend_runs_on(b, os),
                want,
                "backend_runs_on({b:?}, {os:?}) should be {want}"
            );
        }
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
        let stale = now - std::time::Duration::from_secs(31);
        for local in [true, false] {
            assert!(
                cache_is_fresh(RuntimeProbe::Present, now, local),
                "present never expires (local={local})"
            );
            assert!(
                cache_is_fresh(RuntimeProbe::Absent, now, local),
                "fresh absent honored (local={local})"
            );
            assert!(
                !cache_is_fresh(RuntimeProbe::Unreachable, now, local),
                "unreachable is never stored, so never considered fresh (local={local})"
            );
        }
        assert!(
            !cache_is_fresh(RuntimeProbe::Absent, stale, false),
            "a REMOTE absent still expires after 30s — caching it forever once \
             stranded a host that was only briefly down"
        );
        assert!(
            cache_is_fresh(RuntimeProbe::Absent, stale, true),
            "a LOCAL absent never expires: the wrong OS, a missing binary or a \
             stopped daemon don't fix themselves mid-session, and re-probing \
             every 30s is what made the failure repeat"
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
