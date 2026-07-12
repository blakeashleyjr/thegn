//! First-party CPU capping for interactive worktree panes.
//!
//! On the host-toolchain backends (`bwrap`, plain `none`) a pane's process tree
//! — including whatever `cargo build` the user runs in that tab — is otherwise
//! free to peg every core. This module wraps such a pane in a `systemd-run
//! --user --scope` transient unit with a cgroup v2 `CPUQuota`, and joins it to a
//! shared [`CPU_SLICE`] so the *aggregate* of all panes is bounded too. When the
//! host lacks cgroup `cpu` delegation it degrades to a soft `nice` (priority
//! only). The OCI backends carry `--cpus`/`--memory` natively, and the Systemd
//! backend caps inline via [`systemd_cap_args`], so neither is scope-wrapped.
//!
//! Extracted from the pinned-oversized `sandbox.rs` (file-size ratchet). The
//! argv builders are pure over the probed mechanism ([`CpuCap`]) so they are
//! unit-tested deterministically, mirroring `thegn-host`'s `CapBackend`.

use crate::sandbox::{Backend, SandboxLimits, SandboxSpec};
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
/// [`cap_prefix`] is pure over it so it is unit-testable deterministically.
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
    if let Some(m) = limits.memory.as_deref().filter(|m| !m.trim().is_empty()) {
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
    let mem = limits.memory.as_deref().filter(|m| !m.trim().is_empty());
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
    fn slice_on_by_default_off_when_disabled() {
        assert!(slice_enabled(&limits(None, None, None))); // unset ⇒ auto ⇒ on
        assert!(slice_enabled(&limits(None, None, Some("6"))));
        assert!(!slice_enabled(&limits(None, None, Some("off"))));
        assert!(!slice_enabled(&limits(None, None, Some(""))));
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
        assert!(joined.contains("MemoryMax=4g"));
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
        assert!(joined.contains("MemoryMax=4g"));
        // Disabled aggregate + no per-pane ⇒ no args at all.
        assert!(systemd_cap_args(&limits(None, None, Some("off"))).is_empty());
    }
}
