//! First-party CPU capping for interactive worktree panes.
//!
//! On the host-toolchain backends (`bwrap`, plain `none`) a pane's process tree
//! — including whatever `cargo build` the user runs in that tab — is otherwise
//! free to peg every core. This module wraps such a pane in a `systemd-run
//! --user --scope` transient unit with a cgroup v2 `CPUQuota`, and joins it to a
//! shared [`CPU_SLICE`] so the *aggregate* of all panes is bounded too. When the
//! host lacks cgroup `cpu` delegation it degrades to a soft `nice` (priority
//! only). The OCI backends carry `--cpus`/`--memory` natively, and the Systemd
//! backend caps inline via `systemd_cap_args`, so neither is scope-wrapped.
//!
//! Extracted from the oversized `sandbox.rs` (kept flat). The
//! argv builders are pure over the probed mechanism ([`CpuCap`]) so they are
//! unit-tested deterministically, mirroring `thegn-host`'s `CapBackend`.

use crate::sandbox::{Backend, SandboxLimits, SandboxSpec};
use crate::sandbox_backend::HostOs;
use crate::util;
use std::sync::OnceLock;

/// Parent user slice that every capped worktree pane joins, so the *aggregate*
/// CPU of all thegn panes is bounded by a single `CPUQuota` (set once at host
/// startup via `systemctl --user set-property`). Panes attach with
/// `--slice=<this>`; a per-pane `-p CPUQuota` nests inside it.
pub const CPU_SLICE: &str = "thegn.slice";

/// Nice level for the soft (`nice`) fallback used when there is no cgroup `cpu`
/// delegation — lowers scheduling priority so a busy pane yields, without a hard
/// ceiling.
const CPU_NICE: i32 = 10;

/// How an interactive worktree pane gets its CPU/mem ceiling on a host-toolchain
/// backend (bwrap / none). Resolved once from PATH + cgroup delegation;
/// `cap_prefix` is pure over it so it is unit-testable deterministically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuCap {
    /// `systemd-run --user --scope` with a real cgroup v2 `CPUQuota` ceiling.
    ScopeHard,
    /// `nice -n N` (+ `ionice -c3`) — priority only, no hard cap.
    NiceSoft,
    /// No wrapper available; run uncapped.
    None,
}

impl CpuCap {
    /// Short human label for `thegn doctor` / status.
    pub fn label(self) -> &'static str {
        match self {
            CpuCap::ScopeHard => "hard — systemd user scope",
            CpuCap::NiceSoft => "SOFT — nice (no cgroup cpu delegation)",
            CpuCap::None => "none",
        }
    }

    /// Whether this mechanism can ever *reach* a pane on `os` — i.e. whether
    /// there is some backend [`cap_prefix`] would actually wrap.
    ///
    /// Probing a mechanism is not the same as being able to apply it, and on
    /// macOS the two diverge completely. `cap_prefix` only wraps
    /// `Backend::Bwrap` or `Backend::None`, and only for a local placement. On a
    /// Mac `Bwrap` is impossible ([`crate::sandbox_backend::backend_runs_on`]
    /// gates it to Linux), and a **local** `Backend::None` never produces a spec
    /// at all (`sandbox::resolve_placed` returns `None` for it, because "none +
    /// local" means the caller's plain host shell) — so `wrap_pane_argv` is
    /// never called and no macOS pane is ever `nice`-wrapped.
    ///
    /// Without this, `thegn doctor` reported `SOFT — nice` on macOS: a
    /// mechanism that is genuinely detected (`nice` IS on PATH) and genuinely
    /// unreachable. That is the same class of lie as reporting the requested
    /// sandbox backend instead of the one that actually ran — report what is
    /// observed, not what was picked.
    pub fn reachable_on(self, os: HostOs) -> bool {
        match self {
            // No wrapper to reach anything with.
            CpuCap::None => false,
            // Both wrappers ride `cap_prefix`, whose only eligible backends are
            // the Linux host-toolchain ones.
            CpuCap::ScopeHard | CpuCap::NiceSoft => os == HostOs::Linux,
        }
    }

    /// [`label`](Self::label), qualified when the mechanism cannot reach a pane
    /// on this OS. What `thegn doctor` should print.
    pub fn label_on(self, os: HostOs) -> String {
        if self.reachable_on(os) || self == CpuCap::None {
            return self.label().to_string();
        }
        format!(
            "none — {} is present but never applies here (host panes are not wrapped on {})",
            match self {
                CpuCap::ScopeHard => "systemd-run",
                _ => "nice",
            },
            os.as_str()
        )
    }
}

/// Pure decision: pick the enforcement mechanism from the probed facts. Split
/// from the probe so the 3-way ladder is exhaustively unit-testable.
pub fn choose_cpu_cap(systemd_run: bool, cgroup_cpu_delegated: bool, nice: bool) -> CpuCap {
    if systemd_run && cgroup_cpu_delegated {
        CpuCap::ScopeHard
    } else if nice {
        CpuCap::NiceSoft
    } else {
        CpuCap::None
    }
}

/// True when this process runs under cgroup v2 with the `cpu` controller
/// available in its own cgroup — the precondition for a `systemd-run --scope`
/// `CPUQuota` to actually bite (otherwise the scope is created but the quota is
/// silently ignored). Reads the unified `0::<path>` line of `/proc/self/cgroup`
/// and checks that cgroup's `cgroup.controllers` for `cpu`.
fn cgroup_cpu_delegated() -> bool {
    let Ok(self_cg) = std::fs::read_to_string("/proc/self/cgroup") else {
        return false;
    };
    // A cgroup v2 (unified) host emits a single `0::<path>` line; a legacy v1 /
    // hybrid host won't, and we can't offer a hard cap there.
    let Some(rel) = self_cg.lines().find_map(|l| l.strip_prefix("0::")) else {
        return false;
    };
    let rel = rel.trim().trim_start_matches('/');
    let controllers = std::path::Path::new("/sys/fs/cgroup")
        .join(rel)
        .join("cgroup.controllers");
    std::fs::read_to_string(controllers)
        .map(|s| s.split_whitespace().any(|c| c == "cpu"))
        .unwrap_or(false)
}

/// Probe (once) how this host can cap an interactive pane's CPU. Reads PATH and
/// cgroup v2 controller delegation; memoized because neither changes within a
/// run. Free when unused — nothing calls it unless a cap is configured.
pub fn detect_cpu_cap() -> CpuCap {
    static CAP: OnceLock<CpuCap> = OnceLock::new();
    *CAP.get_or_init(|| {
        choose_cpu_cap(
            util::have("systemd-run"),
            cgroup_cpu_delegated(),
            util::have("nice"),
        )
    })
}

/// Translate a "cores" value (`"2"`, `"1.5"`, `"0.5"`) into a systemd
/// `CPUQuota` percent (`"200%"`, `"150%"`, `"50%"`). `None` on non-positive or
/// unparseable input, so junk config silently means "no cap" rather than a
/// malformed unit property.
fn cpu_cores_to_percent(cores: &str) -> Option<String> {
    let cores: f64 = cores.trim().parse().ok()?;
    if !cores.is_finite() || cores <= 0.0 {
        return None;
    }
    Some(format!("{}%", (cores * 100.0).round() as i64))
}

/// Resolve `[sandbox.limits] cpu_total` into an aggregate `CPUQuota` percent for
/// [`CPU_SLICE`]. `"auto"` ⇒ leave 2 cores free (`max(1, ncpu-2)`); empty /
/// `"off"` / `"none"` ⇒ no aggregate cap; otherwise treated as cores. `ncpu` is
/// passed in so the mapping stays pure/testable. Callers pass `"auto"` for an
/// unset (`None`) config value — the aggregate cap is on by default.
pub fn resolve_cpu_total(value: &str, ncpu: usize) -> Option<String> {
    let v = value.trim();
    if v.is_empty() || v.eq_ignore_ascii_case("off") || v.eq_ignore_ascii_case("none") {
        return None;
    }
    if v.eq_ignore_ascii_case("auto") {
        let cores = ncpu.saturating_sub(2).max(1);
        return Some(format!("{}%", cores * 100));
    }
    cpu_cores_to_percent(v)
}

/// Resolve `[sandbox.limits] memory_total` into an aggregate `MemoryHigh` value
/// for [`CPU_SLICE`]. Empty / `"off"` / `"none"` ⇒ no aggregate memory cap,
/// which is also the default (unlike `cpu_total`, there is no `"auto"` — there
/// is no defensible fraction of a machine's RAM to claim without being told).
/// The value is passed through verbatim, so systemd's own suffixes (`G`, `M`,
/// `%`) all work.
pub fn resolve_memory_total(value: &str) -> Option<String> {
    let v = value.trim();
    if v.is_empty() || v.eq_ignore_ascii_case("off") || v.eq_ignore_ascii_case("none") {
        return None;
    }
    systemd_bytes(v)
}

/// A `[sandbox.limits]` memory value as **systemd** spells it.
///
/// The two backends disagree about case and this is not cosmetic: OCI's
/// `--memory` takes `512m`/`2g`, which is also the style `config.toml.example`
/// documents — and systemd rejects it outright (`Failed to parse MemoryHigh=
/// value '56g': Invalid argument`). `systemctl set-property` applies its
/// properties as ONE transaction, so a single lowercase suffix does not merely
/// drop the memory cap: it voids the `CPUQuota` and the weights in the same
/// call, leaving a slice that looks configured and bounds nothing. That is
/// exactly how the aggregate cap silently sat at its stale value while
/// `thegn doctor` cheerfully reported the configured one.
///
/// So normalize rather than ask the user to know which backend they are on:
/// uppercase the unit, accept the `kb`/`mb`/`gb` spellings, and pass a bare
/// byte count through. `None` for anything that isn't a size, so junk config
/// omits one property instead of poisoning the whole transaction.
fn systemd_bytes(value: &str) -> Option<String> {
    let v = value.trim();
    if v.is_empty() {
        return None;
    }
    let digits_end = v.find(|c: char| !c.is_ascii_digit() && c != '.')?;
    let (num, unit) = v.split_at(digits_end);
    if num.is_empty() || num.parse::<f64>().is_err() {
        // No digits at all, or a malformed number — not a size.
        return None;
    }
    let unit = match unit.trim().to_ascii_uppercase().as_str() {
        "K" | "KB" => "K",
        "M" | "MB" => "M",
        "G" | "GB" => "G",
        "T" | "TB" => "T",
        _ => return None,
    };
    Some(format!("{num}{unit}"))
}

/// [`systemd_bytes`] with a passthrough for a bare byte count, for the argv
/// builders: `MemoryMax=1073741824` is as valid as `MemoryMax=1G`.
fn systemd_mem_arg(value: &str) -> Option<String> {
    let v = value.trim();
    if v.is_empty() {
        return None;
    }
    if v.chars().all(|c| c.is_ascii_digit()) {
        return Some(v.to_string());
    }
    systemd_bytes(v)
}

/// Whether a pane should join the aggregate [`CPU_SLICE`]. Unset (`None`) means
/// "auto" — on by default; only an explicit `"off"`/`"none"`/`""` disables it.
fn slice_enabled(limits: &SandboxLimits) -> bool {
    match limits.cpu_total.as_deref() {
        None => true,
        Some(v) => {
            let v = v.trim();
            !(v.is_empty() || v.eq_ignore_ascii_case("off") || v.eq_ignore_ascii_case("none"))
        }
    }
}

/// The extra `systemd-run` properties for the Systemd backend's own argv (it is
/// already a `systemd-run --user` line, so the cap rides inline — no scope wrap):
/// `--slice=<CPU_SLICE>` for the aggregate, plus per-pane `CPUQuota`/`MemoryMax`.
pub(crate) fn systemd_cap_args(limits: &SandboxLimits) -> Vec<String> {
    let mut v = Vec::new();
    if slice_enabled(limits) {
        v.push(format!("--slice={CPU_SLICE}"));
    }
    if let Some(q) = limits.cpu.as_deref().and_then(cpu_cores_to_percent) {
        v.extend(["-p".into(), format!("CPUQuota={q}")]);
    }
    if let Some(m) = limits.memory.as_deref().and_then(systemd_mem_arg) {
        v.extend(["-p".into(), format!("MemoryMax={m}")]);
    }
    v
}

/// Entry point from `sandbox::enter_argv`: cap the composed pane argv using the
/// probed host mechanism. A no-op unless the backend is a local host-toolchain
/// shell with something to enforce.
pub(crate) fn wrap_pane_argv(spec: &SandboxSpec, argv: Vec<String>) -> Vec<String> {
    cap_prefix(
        spec.backend,
        spec.placement.is_local(),
        &spec.limits,
        argv,
        detect_cpu_cap(),
    )
}

/// The resolved `[sandbox.limits]`, published once by whichever entry point
/// loaded the config (`main`, for both the compositor and every CLI verb).
///
/// Background jobs are wrapped deep inside the merge/agent call graph, whose
/// functions carry only `[merge_queue]` config — and threading the whole
/// resource policy through `run_fold`/`attempt_land`/`bisect_offender` and their
/// eighteen call sites to reach two spawns is not a trade worth making. Unset is
/// meaningful: a process that never published (a unit test) gets NO wrapping, so
/// the gate tests stay hermetic instead of picking up the developer's own
/// `~/.config/thegn/config.toml` and spawning real scopes.
static BACKGROUND_LIMITS: OnceLock<SandboxLimits> = OnceLock::new();

/// Publish the resource policy for background-job wrapping. Idempotent; the
/// first call wins.
pub fn publish_background_limits(limits: SandboxLimits) {
    // `get_or_init` rather than `set`: same first-publish-wins semantics without
    // a Result to discard.
    BACKGROUND_LIMITS.get_or_init(|| limits);
}

/// Does `systemd-run --user --scope` actually work here? [`detect_cpu_cap`]
/// establishes that the binary exists and that cgroup `cpu` is delegated, which
/// is necessary but not sufficient — the user manager still has to accept the
/// transient unit.
///
/// This matters more for a background job than for a pane. If the wrapper fails,
/// the inner command never runs and `cmd.output()` reports the *wrapper's*
/// non-zero exit, which the fold gate cannot tell apart from "the test suite
/// failed" — so a broken cap would silently blame a perfectly good branch. That
/// is a worse bug than the one capping exists to fix, so pay one `true` spawn per
/// process (memoized) to be sure. Only background callers reach this; the pane
/// path must never spawn near the event loop.
fn background_scope_usable() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| {
        std::process::Command::new("systemd-run")
            .args([
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                &format!("--slice={CPU_SLICE}"),
                "--",
                "true",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// Wrap a thegn-spawned **background job** — the merge-queue gate, an agent
/// handoff — so it joins the same aggregate [`CPU_SLICE`] an interactive pane
/// does.
///
/// These are the heaviest things thegn starts (`gate_command` is typically a
/// full test suite) and they used to escape every ceiling: they are spawned
/// straight from the thegn process, which lives in whatever cgroup thegn was
/// launched into, so the aggregate cap bounded the panes and then this ran on
/// top of it. Same mechanism ladder as an interactive pane.
///
/// Fail-safe in both directions: no published policy, or a scope wrapper that
/// doesn't work here, means the job runs exactly as it did before. Capping is an
/// optimization; running the command is the contract.
///
/// Callers must be off the event loop — the probe spawns.
pub fn wrap_background_argv(argv: Vec<String>) -> Vec<String> {
    wrap_control_argv(argv, false)
}

/// Wrap an argv the **control plane** is about to spawn (the pane daemon's
/// `sessions.open`), so an API-opened session joins the same aggregate ceiling
/// as everything else thegn starts.
///
/// This closes a real escape. A compositor-opened pane is capped because its
/// argv was already wrapped by `sandbox::enter_argv` before the `OpenSpec` was
/// built; a session opened *directly* against the control API — by the CLI, a
/// thin client, or a supervising agent — went straight to the PTY with nothing
/// in front of it. A fleet spawned that way was the one thing on the box with
/// no limit at all.
///
/// `already_capped` is the caller's explicit declaration, never a guess.
/// Sniffing the argv would be unreliable in both directions: a user's
/// `[[agents]]` entry of `nice -n 5 claude` reads as already-capped, while a
/// genuinely `systemd-run`-wrapped argv is handled inside [`cap_prefix`] anyway.
///
/// Fail-safe like the background path: with no published policy, or a scope
/// wrapper that does not work here, the argv runs exactly as it would have.
///
/// Callers must be off the event loop — the probe spawns.
pub fn wrap_control_argv(argv: Vec<String>, already_capped: bool) -> Vec<String> {
    if already_capped {
        return argv;
    }
    let Some(limits) = BACKGROUND_LIMITS.get() else {
        return argv;
    };
    let wrapped = cap_prefix(Backend::None, true, limits, argv.clone(), detect_cpu_cap());
    // Only the scope path can break the inner command; `nice` cannot.
    if wrapped.first().map(String::as_str) == Some("systemd-run") && !background_scope_usable() {
        return argv;
    }
    wrapped
}

/// Force the memoized scope probe now, and report whether scopes are usable.
///
/// The probe spawns a real `systemd-run … true`, which has no business
/// happening on a control-API request path. A daemon calls this once at startup
/// (off the runtime's worker threads) so every later `sessions.open` finds the
/// answer already cached.
///
/// Returns the verdict rather than discarding it, so the caller can log what it
/// found — a ceiling that silently isn't there is worth a line in the log.
pub fn warm_scope_probe() -> bool {
    background_scope_usable()
}

/// Wrap a host-toolchain pane argv (bwrap / bare shell) so its whole process
/// tree is CPU-capped. Pure over `mech` for testability. Attaches the pane to
/// the shared [`CPU_SLICE`] (aggregate ceiling) and, when `[sandbox.limits] cpu`
/// is set, adds a per-pane `CPUQuota`. Returns `argv` unchanged when there is
/// nothing to enforce, the backend isn't a local host-toolchain shell, or the
/// argv already starts with `systemd-run` (the Systemd backend caps inline).
fn cap_prefix(
    backend: Backend,
    is_local: bool,
    limits: &SandboxLimits,
    argv: Vec<String>,
    mech: CpuCap,
) -> Vec<String> {
    // OCI carries `--cpus`; Systemd caps inline; remote host-toolchain capping is
    // deferred (needs a remote cgroup probe).
    if !matches!(backend, Backend::Bwrap | Backend::None)
        || !is_local
        || argv.first().map(String::as_str) == Some("systemd-run")
    {
        return argv;
    }
    let per_pane = limits.cpu.as_deref().and_then(cpu_cores_to_percent);
    let use_slice = slice_enabled(limits);
    let mem = limits.memory.as_deref().and_then(systemd_mem_arg);
    if per_pane.is_none() && !use_slice && mem.is_none() {
        return argv; // nothing configured to enforce
    }
    match mech {
        CpuCap::ScopeHard => {
            let mut v = vec![
                "systemd-run".to_string(),
                "--user".into(),
                "--scope".into(),
                "--quiet".into(),
                "--collect".into(),
            ];
            if use_slice {
                v.push(format!("--slice={CPU_SLICE}"));
            }
            if let Some(q) = per_pane {
                v.extend(["-p".into(), format!("CPUQuota={q}")]);
            }
            if let Some(m) = mem {
                v.extend(["-p".into(), format!("MemoryMax={m}")]);
            }
            v.push("--".into());
            v.extend(argv);
            v
        }
        CpuCap::NiceSoft => {
            // No cgroup delegation: a hard quota/slice can't be honored, so fall
            // back to priority-only — the machine stays responsive even though
            // the pane isn't hard-capped.
            let mut v = Vec::new();
            if util::have("ionice") {
                v.extend(["ionice".into(), "-c3".into()]);
            }
            v.extend(["nice".into(), "-n".into(), CPU_NICE.to_string()]);
            v.extend(argv);
            v
        }
        CpuCap::None => argv,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(cpu: Option<&str>, mem: Option<&str>, total: Option<&str>) -> SandboxLimits {
        SandboxLimits {
            cpu: cpu.map(str::to_string),
            memory: mem.map(str::to_string),
            cpu_total: total.map(str::to_string),
            memory_total: None,
        }
    }

    #[test]
    fn cpu_cores_to_percent_maps_cores() {
        assert_eq!(cpu_cores_to_percent("2").as_deref(), Some("200%"));
        assert_eq!(cpu_cores_to_percent("1.5").as_deref(), Some("150%"));
        assert_eq!(cpu_cores_to_percent("0.5").as_deref(), Some("50%"));
        assert_eq!(cpu_cores_to_percent(" 2 ").as_deref(), Some("200%"));
        assert_eq!(cpu_cores_to_percent("0"), None);
        assert_eq!(cpu_cores_to_percent("-1"), None);
        assert_eq!(cpu_cores_to_percent("junk"), None);
        assert_eq!(cpu_cores_to_percent(""), None);
    }

    #[test]
    fn resolve_cpu_total_auto_leaves_two_free() {
        assert_eq!(resolve_cpu_total("auto", 8).as_deref(), Some("600%"));
        assert_eq!(resolve_cpu_total("AUTO", 4).as_deref(), Some("200%"));
        // Never resolves below one core, even on tiny hosts.
        assert_eq!(resolve_cpu_total("auto", 2).as_deref(), Some("100%"));
        assert_eq!(resolve_cpu_total("auto", 1).as_deref(), Some("100%"));
        // Explicit cores + disabling values.
        assert_eq!(resolve_cpu_total("6", 8).as_deref(), Some("600%"));
        assert_eq!(resolve_cpu_total("off", 8), None);
        assert_eq!(resolve_cpu_total("none", 8), None);
        assert_eq!(resolve_cpu_total("", 8), None);
    }

    #[test]
    fn resolve_memory_total_normalizes_for_systemd_and_disables() {
        // THE bug this normalizer exists for: systemd rejects a lowercase unit
        // outright, and `systemctl set-property` is one transaction — so a
        // single "56g" voided the CPUQuota and the weights alongside it and
        // left a slice that looked configured and bounded nothing.
        assert_eq!(resolve_memory_total("56g").as_deref(), Some("56G"));
        assert_eq!(resolve_memory_total("24gb").as_deref(), Some("24G"));
        assert_eq!(resolve_memory_total(" 512m ").as_deref(), Some("512M"));
        assert_eq!(resolve_memory_total("2T").as_deref(), Some("2T"));
        // Already-correct input is untouched.
        assert_eq!(resolve_memory_total("8G").as_deref(), Some("8G"));
        // The disable vocabulary matches cpu_total's; there is NO "auto".
        assert_eq!(resolve_memory_total(""), None);
        assert_eq!(resolve_memory_total("off"), None);
        assert_eq!(resolve_memory_total("NONE"), None);
        // Junk omits ONE property rather than poisoning the transaction.
        assert_eq!(resolve_memory_total("lots"), None);
        assert_eq!(resolve_memory_total("12%"), None);
    }

    #[test]
    fn systemd_mem_arg_passes_a_bare_byte_count() {
        // `MemoryMax=1073741824` is as valid as `MemoryMax=1G`.
        assert_eq!(systemd_mem_arg("1073741824").as_deref(), Some("1073741824"));
        assert_eq!(systemd_mem_arg("4g").as_deref(), Some("4G"));
        assert_eq!(systemd_mem_arg(""), None);
        assert_eq!(systemd_mem_arg("junk"), None);
    }

    #[test]
    fn background_argv_joins_the_shared_slice() {
        // A background job (the fold gate, an agent handoff) must land in the
        // SAME slice as the panes, so the aggregate ceiling covers the whole of
        // thegn's work rather than the visible half of it.
        let l = limits(None, None, None); // all defaults ⇒ slice on
        let argv = vec!["sh".to_string(), "-c".into(), "just test".into()];
        let out = cap_prefix(Backend::None, true, &l, argv.clone(), CpuCap::ScopeHard);
        assert_eq!(out[0], "systemd-run");
        assert!(out.iter().any(|a| a == "--slice=thegn.slice"));
        let sep = out.iter().position(|a| a == "--").unwrap();
        assert_eq!(&out[sep + 1..], argv.as_slice(), "the job itself is intact");

        // Explicitly disabled aggregate ⇒ the job is left alone, same as a pane.
        let off = limits(None, None, Some("off"));
        assert_eq!(
            cap_prefix(Backend::None, true, &off, argv.clone(), CpuCap::ScopeHard),
            argv
        );
    }

    #[test]
    fn slice_on_by_default_off_when_disabled() {
        assert!(slice_enabled(&limits(None, None, None))); // unset ⇒ auto ⇒ on
        assert!(slice_enabled(&limits(None, None, Some("6"))));
        assert!(!slice_enabled(&limits(None, None, Some("off"))));
        assert!(!slice_enabled(&limits(None, None, Some(""))));
    }

    #[test]
    fn a_detected_mechanism_is_not_an_applicable_one() {
        use CpuCap::*;
        // `cap_prefix` only ever wraps `Backend::Bwrap` or a LOCAL
        // `Backend::None`. On macOS bwrap is impossible, and a local
        // `Backend::None` never yields a spec (`resolve_placed` returns None for
        // it), so neither wrapper can reach a pane — even though `nice` is on
        // PATH and the probe therefore reports `NiceSoft`.
        for mech in [ScopeHard, NiceSoft] {
            assert!(mech.reachable_on(HostOs::Linux), "{mech:?}");
            for os in [HostOs::MacOs, HostOs::Windows, HostOs::Other] {
                assert!(!mech.reachable_on(os), "{mech:?} on {os:?}");
            }
        }
        // `None` reaches nothing anywhere, by definition.
        for os in [HostOs::Linux, HostOs::MacOs, HostOs::Windows] {
            assert!(!CpuCap::None.reachable_on(os));
        }

        // The label must say so, or `doctor` reports a cap that never applies —
        // the same class of lie as naming the requested sandbox backend instead
        // of the one that actually ran.
        assert_eq!(NiceSoft.label_on(HostOs::Linux), NiceSoft.label());
        let mac = NiceSoft.label_on(HostOs::MacOs);
        assert!(mac.starts_with("none"), "{mac}");
        assert!(mac.contains("macOS"), "{mac}");
        assert!(mac.contains("nice"), "must name what was detected: {mac}");
        // `None` needs no qualification — it already says nothing applies.
        assert_eq!(CpuCap::None.label_on(HostOs::MacOs), "none");
    }

    #[test]
    fn choose_cpu_cap_ladder() {
        use CpuCap::*;
        // Hard cap requires BOTH systemd-run and cgroup cpu delegation.
        assert_eq!(choose_cpu_cap(true, true, true), ScopeHard);
        assert_eq!(choose_cpu_cap(true, true, false), ScopeHard);
        // systemd present but no delegation ⇒ soft nice (if available).
        assert_eq!(choose_cpu_cap(true, false, true), NiceSoft);
        assert_eq!(choose_cpu_cap(false, true, true), NiceSoft);
        assert_eq!(choose_cpu_cap(false, false, true), NiceSoft);
        // Nothing available ⇒ no cap.
        assert_eq!(choose_cpu_cap(true, false, false), None);
        assert_eq!(choose_cpu_cap(false, true, false), None);
        assert_eq!(choose_cpu_cap(false, false, false), None);
    }

    #[test]
    fn scope_wraps_bwrap_with_slice_and_per_pane() {
        let l = limits(Some("1.5"), Some("4g"), None); // total None ⇒ slice on
        let argv = vec!["bwrap".to_string(), "--".into(), "/bin/sh".into()];
        let out = cap_prefix(Backend::Bwrap, true, &l, argv, CpuCap::ScopeHard);
        assert_eq!(out[0], "systemd-run");
        let joined = out.join(" ");
        assert!(joined.contains("--user --scope --quiet --collect"));
        assert!(joined.contains("--slice=thegn.slice"));
        assert!(joined.contains("CPUQuota=150%"));
        assert!(
            joined.contains("MemoryMax=4G"),
            "lowercase is normalized for systemd"
        );
        // The original argv survives, after the `--` separator.
        let sep = out.iter().position(|a| a == "--").unwrap();
        assert_eq!(&out[sep + 1..], ["bwrap", "--", "/bin/sh"]);
    }

    #[test]
    fn scope_slice_only_when_no_per_pane() {
        // Aggregate auto (default), per-pane unset: join the slice, no CPUQuota.
        let l = limits(None, None, None);
        let argv = vec!["/bin/sh".into(), "-lc".into(), "exec zsh".into()];
        let out = cap_prefix(Backend::None, true, &l, argv, CpuCap::ScopeHard);
        assert_eq!(out[0], "systemd-run");
        assert!(out.iter().any(|a| a == "--slice=thegn.slice"));
        assert!(!out.join(" ").contains("CPUQuota="));
    }

    #[test]
    fn nice_soft_fallback() {
        let l = limits(Some("2"), None, None);
        let argv = vec!["bwrap".to_string(), "true".into()];
        let out = cap_prefix(Backend::Bwrap, true, &l, argv, CpuCap::NiceSoft);
        let nice = out.iter().position(|a| a == "nice").unwrap();
        assert_eq!(&out[nice..nice + 3], ["nice", "-n", "10"]);
        assert!(!out.iter().any(|a| a == "systemd-run"));
        assert!(out.contains(&"bwrap".to_string()));
    }

    #[test]
    fn unchanged_when_nothing_to_enforce() {
        // Everything disabled ⇒ argv untouched even on a hard-cap host.
        let l = limits(None, None, Some("off"));
        let argv = vec!["bwrap".to_string(), "true".into()];
        assert_eq!(
            cap_prefix(Backend::Bwrap, true, &l, argv.clone(), CpuCap::ScopeHard),
            argv
        );
        // Mechanism None ⇒ never wrap, even with caps set.
        let l2 = limits(Some("2"), None, None);
        assert_eq!(
            cap_prefix(Backend::Bwrap, true, &l2, argv.clone(), CpuCap::None),
            argv
        );
    }

    #[test]
    fn skips_oci_remote_and_double_wrap() {
        let l = limits(Some("2"), None, None);
        // OCI backend: not scope-wrapped (it has --cpus).
        let argv = vec!["podman".to_string(), "exec".into()];
        assert_eq!(
            cap_prefix(Backend::Podman, true, &l, argv.clone(), CpuCap::ScopeHard),
            argv
        );
        // Remote placement: deferred.
        let bw = vec!["bwrap".to_string(), "true".into()];
        assert_eq!(
            cap_prefix(Backend::Bwrap, false, &l, bw.clone(), CpuCap::ScopeHard),
            bw
        );
        // Already a systemd-run line: no double-wrap.
        let sd = vec!["systemd-run".to_string(), "--user".into()];
        assert_eq!(
            cap_prefix(Backend::None, true, &l, sd.clone(), CpuCap::ScopeHard),
            sd
        );
    }

    #[test]
    fn systemd_cap_args_emits_slice_and_props() {
        let l = limits(Some("1.5"), Some("4g"), None);
        let args = systemd_cap_args(&l);
        let joined = args.join(" ");
        assert!(joined.contains("--slice=thegn.slice"));
        assert!(joined.contains("CPUQuota=150%"));
        assert!(
            joined.contains("MemoryMax=4G"),
            "lowercase is normalized for systemd"
        );
        // Disabled aggregate + no per-pane ⇒ no args at all.
        assert!(systemd_cap_args(&limits(None, None, Some("off"))).is_empty());
    }

    /// A caller that has already capped its own argv — the compositor, which
    /// wraps via `sandbox::enter_argv` long before the `OpenSpec` is built —
    /// must never be wrapped a second time.
    #[test]
    fn an_already_capped_argv_is_returned_untouched() {
        let argv: Vec<String> = ["systemd-run", "--user", "--scope", "--", "claude"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(wrap_control_argv(argv.clone(), true), argv);
        let plain: Vec<String> = vec!["claude".into()];
        assert_eq!(wrap_control_argv(plain.clone(), true), plain);
    }

    /// Hermeticity: a process that never published a policy (every unit test)
    /// gets no wrapping at all, so nothing here spawns a real scope.
    #[test]
    fn no_published_policy_means_no_wrapping() {
        let argv: Vec<String> = vec!["claude".into(), "-p".into()];
        assert_eq!(wrap_control_argv(argv.clone(), false), argv);
        assert_eq!(wrap_background_argv(argv.clone()), argv);
    }
}
