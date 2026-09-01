//! First-party CPU capping for interactive worktree panes.
//!
//! On the host-toolchain backends (`bwrap`, plain `none`) a pane's process tree
//! — including whatever `cargo build` the user runs in that tab — is otherwise
//! free to peg every core. This module wraps such a pane in a `systemd-run
//! --user --scope` transient unit with a cgroup v2 `CPUQuota`, and joins it to a
//! shared [`CPU_SLICE`] so the *aggregate* of all panes is bounded too. When the
//! host lacks cgroup `cpu` delegation it degrades to a soft `nice` (priority
//! only). thegn's own background jobs (the fold gate, an agent handoff, an LSP
//! server, a control-API session) join the same slice but carry no per-pane
//! `CPUQuota` — see `CapRole`. The OCI backends carry `--cpus`/`--memory` natively, and the Systemd
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

    /// Whether this mechanism can ever *reach* a pane on `os`.
    ///
    /// **This used to be OS-dependent, and is no longer.** The reasoning was:
    /// `cap_prefix` wraps only `Backend::Bwrap` or a local `Backend::None`; on a
    /// Mac bwrap is impossible, and a local `Backend::None` never produces a spec
    /// (`sandbox::resolve_placed` returns `None` for it — "none + local" *means*
    /// the caller's plain host shell), so `wrap_pane_argv` was never called and
    /// no macOS pane was ever wrapped.
    ///
    /// That was true, and it was also the bug: it meant an uncontained pane got
    /// no ceiling on ANY OS, which is how `thegn.slice` came to sit correctly
    /// configured and empty while every pane ran uncapped beside it.
    /// [`wrap_uncontained_pane_argv`] now caps that pane directly, without a
    /// spec, from platform-neutral host code — so a detected mechanism reaches a
    /// pane everywhere it is detected.
    ///
    /// The distinction the method exists to draw therefore collapses to the one
    /// case left: [`CpuCap::None`], which by definition wraps nothing. Kept as a
    /// named concept rather than inlined, because "probed" and "applies" are
    /// genuinely different questions and the next divergence should have a place
    /// to live — `thegn doctor` must keep reporting what is observed, not what
    /// was picked.
    pub fn reachable_on(self, _os: HostOs) -> bool {
        !matches!(self, CpuCap::None)
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

/// How many CPUs the **machine** has, independent of any cgroup quota this
/// process happens to be running under.
///
/// This is deliberately NOT `std::thread::available_parallelism`, and the
/// difference was a live bug. `available_parallelism` is cgroup-quota- and
/// affinity-aware — which is exactly right for "how many threads should I
/// spawn" and exactly wrong for "what ceiling should the shared slice carry".
/// A thegn started *inside* a pane scope (`CPUQuota=800%`) sees 8 CPUs and
/// publishes `auto` ⇒ `max(1, 8-2)` = 600% onto the one shared `thegn.slice`;
/// a thegn started under a tighter scope sees 1–2 and publishes 100%. Each
/// nested instance therefore ratcheted the *shared* ceiling down for every pane,
/// terminal and gate build on the box, until the user had to raise it by hand.
/// The aggregate ceiling must be derived from the hardware, so it is idempotent
/// no matter where the process that computes it happens to live.
///
/// Linux answers from sysfs (`/sys/devices/system/cpu/possible`), then
/// `/proc/cpuinfo`; anywhere else both reads simply fail and it falls back to
/// `available_parallelism`, which is the best available answer off Linux (and
/// where there is no cgroup to skew it). No `#[cfg]` is needed for that — a
/// missing path is already the "not Linux" signal, and this file's cgroup probe
/// reads `/proc` the same way.
pub fn physical_ncpu() -> usize {
    if let Some(n) = std::fs::read_to_string("/sys/devices/system/cpu/possible")
        .ok()
        .as_deref()
        .and_then(parse_cpu_possible)
    {
        return n;
    }
    if let Some(n) = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .as_deref()
        .and_then(parse_cpuinfo_processors)
    {
        return n;
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Parse a Linux cpu-mask list (`/sys/devices/system/cpu/possible`) into a
/// count. The format is comma-separated inclusive ranges or singletons:
/// `"0-23"`, `"0"`, `"0-1,3-5"`. `None` on anything else, so a surprising
/// kernel falls through to the next source rather than inventing a number.
fn parse_cpu_possible(s: &str) -> Option<usize> {
    let mut total = 0usize;
    for part in s.trim().split(',') {
        let part = part.trim();
        if part.is_empty() {
            return None;
        }
        let n = match part.split_once('-') {
            Some((lo, hi)) => {
                let lo: usize = lo.trim().parse().ok()?;
                let hi: usize = hi.trim().parse().ok()?;
                hi.checked_sub(lo)?.checked_add(1)?
            }
            None => {
                part.trim().parse::<usize>().ok()?;
                1
            }
        };
        total = total.checked_add(n)?;
    }
    (total > 0).then_some(total)
}

/// Count `processor\t: N` lines in `/proc/cpuinfo` — the fallback when sysfs is
/// unavailable (a container with `/sys` masked). `None` when there are none.
fn parse_cpuinfo_processors(s: &str) -> Option<usize> {
    let n = s
        .lines()
        .filter(|l| l.split(':').next().is_some_and(|k| k.trim() == "processor"))
        .count();
    (n > 0).then_some(n)
}

/// True when this process is ALREADY inside the shared [`CPU_SLICE`] — i.e. it
/// was launched from a thegn pane, a `systemd-run --slice=thegn.slice` wrapper,
/// or any other nested instance.
///
/// Such an instance must never (re)publish the slice's properties: it inherits
/// the ceiling it is meant to be bounded by, and publishing from inside is how
/// the ratchet closed on itself. It is also the guard that keeps a `just start`
/// dev instance, `test/smoke.sh` and the e2e runs from rewriting the live
/// session's ceiling — XDG isolation isolates state, not systemd, and
/// `set-property` on a user-level slice is last-writer-wins across every process
/// on the login session.
pub fn inside_thegn_slice() -> bool {
    std::fs::read_to_string("/proc/self/cgroup")
        .map(|s| cgroup_in_thegn_slice(&s))
        .unwrap_or(false)
}

/// Pure half of [`inside_thegn_slice`]: does a `/proc/self/cgroup` body place
/// the process under [`CPU_SLICE`]? Matched on whole path *segments* so a
/// sibling named `thegn.slice-other.scope` is not mistaken for the slice.
fn cgroup_in_thegn_slice(cgroup: &str) -> bool {
    cgroup.lines().any(|line| {
        // v2: `0::/user.slice/…/thegn.slice/run-x.scope`; v1: `n:ctrl:/path`.
        let path = line.rsplit(':').next().unwrap_or("");
        path.split('/').any(|seg| seg == CPU_SLICE)
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

/// A systemd memory value (`"56G"`, `"512m"`, or the raw byte count
/// `systemctl show` reports back) as bytes.
///
/// Needed because the two spellings are the *same* value: config says `56g`,
/// the unit reports `60129542144`, and comparing them as strings makes every
/// correctly-applied memory cap look like drift. `None` for `infinity`,
/// percentages and junk — nothing to compare, which doctor reports as such.
pub fn mem_bytes(value: &str) -> Option<u64> {
    let v = value.trim();
    if v.is_empty() || v.eq_ignore_ascii_case("infinity") {
        return None;
    }
    if v.chars().all(|c| c.is_ascii_digit()) {
        // TWO GATES MEET HERE, and they want opposite things. Clippy's
        // `manual_ok_err` wants `v.parse().ok()`; the ignored-result ratchet
        // greps for that spelling followed by a semicolon and flags the file.
        // The expanded `match` satisfies the ratchet, so silence clippy on this
        // one statement rather than pinning a whole file for a false positive.
        #[allow(clippy::manual_ok_err)]
        return match v.parse() {
            Ok(bytes) => Some(bytes),
            // Doesn't fit a u64 — nothing meaningful to compare, so it lands in
            // the same bucket as `infinity`/percentages/junk above.
            Err(_) => None,
        };
    }
    let norm = systemd_bytes(v)?;
    let (num, unit) = norm.split_at(norm.len() - 1);
    let mult: u64 = match unit {
        "K" => 1 << 10,
        "M" => 1 << 20,
        "G" => 1 << 30,
        "T" => 1 << 40,
        _ => return None,
    };
    let num: f64 = num.parse().ok()?;
    Some((num * mult as f64).round() as u64)
}

/// Render a byte count the way `[sandbox.limits]` spells one, so doctor's
/// "live" line reads like the config it is being compared to (`56G`, not
/// `60129542144`). Falls back to the raw count when it isn't a whole unit.
pub fn format_mem_bytes(bytes: u64) -> String {
    for (unit, mult) in [
        ("T", 1u64 << 40),
        ("G", 1 << 30),
        ("M", 1 << 20),
        ("K", 1 << 10),
    ] {
        if bytes >= mult && bytes.is_multiple_of(mult) {
            return format!("{}{unit}", bytes / mult);
        }
    }
    bytes.to_string()
}

/// Turn a systemd `CPUQuotaPerSecUSec=` value — what `systemctl show` reports
/// for a slice's **live** `CPUQuota` — back into the `NNN%` spelling
/// [`resolve_cpu_total`] produces, so `thegn doctor` can compare configured
/// against in-effect and flag drift.
///
/// systemd renders the property as a timespan (`"6s"`, `"1s 500ms"`,
/// `"400ms"`), or `"infinity"` when no quota is set. `None` for infinity and for
/// anything unparseable — doctor then reports "no live quota" rather than
/// inventing one. Pure so the round-trip is unit-tested.
pub fn quota_usec_to_percent(value: &str) -> Option<String> {
    let v = value.trim();
    if v.is_empty() || v.eq_ignore_ascii_case("infinity") {
        return None;
    }
    let usec = parse_systemd_timespan_usec(v)?;
    // 1 second of CPU per second == 100%.
    Some(format!("{}%", usec / 10_000))
}

/// Sum a systemd timespan (`"1s 500ms"`, `"6s"`, `"2min"`) into microseconds.
/// Only the units systemd uses for a sub-minute-scale quota are accepted; a
/// bare number is microseconds, as `usec_t` renders it.
fn parse_systemd_timespan_usec(v: &str) -> Option<u64> {
    let mut total: u64 = 0;
    let mut any = false;
    for tok in v.split_whitespace() {
        let split = tok.find(|c: char| !c.is_ascii_digit() && c != '.')?;
        let (num, unit) = tok.split_at(split);
        let num: f64 = num.parse().ok()?;
        if !num.is_finite() || num < 0.0 {
            return None;
        }
        // Longest-suffix-first: "ms" must not be read as "m", "min" not as "m".
        let per_unit: f64 = match unit {
            "us" | "usec" => 1.0,
            "ms" | "msec" => 1_000.0,
            "s" | "sec" | "seconds" => 1_000_000.0,
            "min" | "m" => 60_000_000.0,
            "h" | "hr" => 3_600_000_000.0,
            _ => return None,
        };
        total = total.checked_add((num * per_unit).round() as u64)?;
        any = true;
    }
    any.then_some(total)
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
        CapRole::Pane,
    )
}

/// Wrap a provider-backed pane argv with the same CPU/memory policy used by a
/// local host-toolchain pane. Provider implementations must use this narrow
/// pure adapter instead of inventing a second resource-limit path.
pub fn wrap_provider_pane_argv(
    argv: Vec<String>,
    limits: &SandboxLimits,
    mechanism: CpuCap,
) -> Vec<String> {
    cap_prefix(Backend::None, true, limits, argv, mechanism, CapRole::Pane)
}

/// How many parallel build jobs a pane should ask for, from its own CPU ceiling.
///
/// `CARGO_BUILD_JOBS` is per-INVOCATION, so N worktrees each claim the whole
/// machine independently — the amplification behind ~67 concurrent compilers on
/// a 24-core box. A pane that is capped at 8 cores has no use for more than 8
/// build jobs: past that it is only paying context-switch and peak-RSS cost for
/// work the cgroup will throttle anyway.
///
/// Driven by `[sandbox.limits] cpu` rather than a new toggle, because that key
/// already *is* the per-pane ceiling — one number, one meaning. `None` when it
/// is unset (nothing to derive from) so the pane's own environment decides, as
/// it does today.
///
/// Advisory: the dev shell yields to an existing value, so an explicit
/// `CARGO_BUILD_JOBS=20 just build` still wins.
pub fn cargo_jobs_for(limits: &SandboxLimits) -> Option<usize> {
    let cores: f64 = limits.cpu.as_deref()?.trim().parse().ok()?;
    if !cores.is_finite() || cores <= 0.0 {
        return None;
    }
    // Floor, but never below 1: half a core still has to be able to build.
    Some((cores.floor() as usize).max(1))
}

/// Cap an **uncontained** local pane — one with no sandbox spec at all.
///
/// The ceiling used to ride the sandbox path exclusively, and that conflated two
/// separate things: **capping is not sandboxing.** A pane whose backend resolved
/// to `host`/`none` — no container runtime installed, or one configured off — has
/// no kernel boundary, but it still runs builds and still needs a resource
/// ceiling. It got none, so `thegn.slice` sat correctly configured and empty
/// while every pane and its compilers ran uncapped beside it.
///
/// This is also why `cap_prefix`'s `Backend::None` arm looked like dead code:
/// `sandbox::resolve_placed` returns `None` for `none + local` (that combination
/// *means* "the caller's plain host shell"), so `enter_argv` never runs for it
/// and the arm was never reached. The arm was not dead — it simply had no
/// caller. This is that caller.
///
/// Fail-safe, like [`wrap_background_argv`]: an unpublished policy or a
/// `systemd-run` that cannot create a scope here means the pane runs exactly as
/// it did before. A pane that fails to spawn is far worse than an uncapped one.
///
/// Callers must be off the event loop — the usability probe spawns.
pub fn wrap_uncontained_pane_argv(argv: Vec<String>) -> Vec<String> {
    let Some(limits) = BACKGROUND_LIMITS.get() else {
        return argv;
    };
    let wrapped = cap_prefix(
        Backend::None,
        true,
        limits,
        argv.clone(),
        detect_cpu_cap(),
        CapRole::Pane,
    );
    if wrapped.first().map(String::as_str) == Some("systemd-run") && !background_scope_usable() {
        return argv;
    }
    wrapped
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
/// **It gets the slice and `MemoryMax`, but NOT the per-pane `CPUQuota`** — see
/// `CapRole`. The aggregate slice is already the ceiling for everything thegn
/// runs; `[sandbox.limits] cpu` is a tab's share of the machine, and applying it
/// here quietly ran full gate builds on a fraction of the cores on a box that
/// had already reserved the rest for exactly this.
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
/// Like the background path it takes the slice and `MemoryMax` but not the
/// per-pane `CPUQuota` (`CapRole::Background`): a control-API session is
/// thegn's own work inside the aggregate ceiling, not a tab competing with the
/// other tabs for the machine.
///
/// `already_capped` is the caller's explicit declaration, never a guess.
/// Sniffing the argv would be unreliable in both directions: a user's
/// `[[agents]]` entry of `nice -n 5 claude` reads as already-capped, while a
/// genuinely `systemd-run`-wrapped argv is handled inside `cap_prefix` anyway.
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
    let wrapped = cap_prefix(
        Backend::None,
        true,
        limits,
        argv.clone(),
        detect_cpu_cap(),
        CapRole::Background,
    );
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

/// What kind of thing is being wrapped — which decides whether the **per-pane**
/// `CPUQuota` applies.
///
/// The two are not interchangeable, and treating them as one was a bug.
/// `[sandbox.limits] cpu` is *pane ergonomics*: one tab's `cargo build` should
/// not starve the other tabs, so a pane gets a slice of the machine. A
/// background job — the merge-queue fold gate, an agent handoff, an LSP server,
/// a control-API session — is not competing for a tab; it is thegn's own work,
/// already bounded in aggregate by [`CPU_SLICE`]. Applying the per-pane quota to
/// it silently capped a full gate build at (say) 8 of 24 cores, on top of the
/// aggregate ceiling it was already inside, and the fold gate is the single
/// longest thing thegn runs.
///
/// So: panes get slice + per-pane `CPUQuota`; background jobs get the slice
/// only. **Memory is not split** — `[sandbox.limits] memory` stays on both,
/// because `MemoryMax` is per-process-tree protection against one runaway
/// build eating the box, and that argument applies to a gate build at least as
/// strongly as to a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CapRole {
    /// An interactive worktree pane: slice + per-pane `CPUQuota` + `MemoryMax`.
    Pane,
    /// A thegn-spawned background/control job: slice + `MemoryMax`, no per-pane
    /// `CPUQuota` (the slice already bounds the aggregate).
    Background,
}

/// Wrap a host-toolchain argv (bwrap / bare shell) so its whole process tree is
/// CPU-capped. Pure over `mech` for testability. Attaches it to the shared
/// [`CPU_SLICE`] (aggregate ceiling) and — for [`CapRole::Pane`] only, see that
/// type — adds the per-pane `CPUQuota` from `[sandbox.limits] cpu`. Returns
/// `argv` unchanged when there is nothing to enforce, the backend isn't a local
/// host-toolchain shell, or the argv already starts with `systemd-run` (the
/// Systemd backend caps inline).
fn cap_prefix(
    backend: Backend,
    is_local: bool,
    limits: &SandboxLimits,
    argv: Vec<String>,
    mech: CpuCap,
    role: CapRole,
) -> Vec<String> {
    // OCI carries `--cpus`; Systemd caps inline; remote host-toolchain capping is
    // deferred (needs a remote cgroup probe).
    if !matches!(backend, Backend::Bwrap | Backend::None)
        || !is_local
        || argv.first().map(String::as_str) == Some("systemd-run")
    {
        return argv;
    }
    let per_pane = match role {
        CapRole::Pane => limits.cpu.as_deref().and_then(cpu_cores_to_percent),
        // The slice is the background job's ceiling; the pane quota is not.
        CapRole::Background => None,
    };
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
    fn memory_spellings_compare_by_value_not_by_string() {
        // The config says "56g"; the unit reports back 60129542144. Comparing
        // the strings made every correctly-applied memory cap read as DRIFT.
        assert_eq!(mem_bytes("56g"), Some(60_129_542_144));
        assert_eq!(mem_bytes("60129542144"), mem_bytes("56G"));
        assert_eq!(mem_bytes("512m"), Some(536_870_912));
        assert_eq!(mem_bytes("infinity"), None);
        assert_eq!(mem_bytes("60%"), None);
        assert_eq!(mem_bytes(""), None);
        // …and the live value is printed in the config's own spelling.
        assert_eq!(format_mem_bytes(60_129_542_144), "56G");
        assert_eq!(format_mem_bytes(536_870_912), "512M");
        assert_eq!(format_mem_bytes(1023), "1023");
    }

    #[test]
    fn live_quota_round_trips_back_to_the_configured_spelling() {
        // What `systemctl show -p CPUQuotaPerSecUSec` reports for a slice that
        // `resolve_cpu_total` set — doctor compares the two spellings, so they
        // have to meet.
        assert_eq!(resolve_cpu_total("auto", 24).as_deref(), Some("2200%"));
        assert_eq!(quota_usec_to_percent("22s").as_deref(), Some("2200%"));
        assert_eq!(quota_usec_to_percent("6s").as_deref(), Some("600%"));
        assert_eq!(quota_usec_to_percent("1s 500ms").as_deref(), Some("150%"));
        assert_eq!(quota_usec_to_percent("400ms").as_deref(), Some("40%"));
        assert_eq!(quota_usec_to_percent("500000us").as_deref(), Some("50%"));
        // No quota in effect, or junk ⇒ nothing to compare against.
        assert_eq!(quota_usec_to_percent("infinity"), None);
        assert_eq!(quota_usec_to_percent("[not set]"), None);
        assert_eq!(quota_usec_to_percent(""), None);
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
    fn cargo_jobs_follow_the_per_pane_ceiling() {
        // A pane capped at N cores asks for N jobs — not the machine's core
        // count, which is what let N worktrees each spawn a full build.
        assert_eq!(cargo_jobs_for(&limits(Some("8"), None, None)), Some(8));
        assert_eq!(cargo_jobs_for(&limits(Some("1"), None, None)), Some(1));
        // Fractional floors, but never to zero — half a core must still build.
        assert_eq!(cargo_jobs_for(&limits(Some("1.5"), None, None)), Some(1));
        assert_eq!(cargo_jobs_for(&limits(Some("0.5"), None, None)), Some(1));
        // No ceiling configured ⇒ nothing to derive; the pane's own env decides.
        assert_eq!(cargo_jobs_for(&limits(None, None, None)), None);
        assert_eq!(cargo_jobs_for(&limits(Some(""), None, None)), None);
        assert_eq!(cargo_jobs_for(&limits(Some("junk"), None, None)), None);
        assert_eq!(cargo_jobs_for(&limits(Some("0"), None, None)), None);
    }

    #[test]
    fn uncontained_pane_is_capped_like_a_contained_one() {
        // The gap this closes: a pane whose backend resolved to host/none has no
        // sandbox spec, so it never reached `enter_argv` and never got a ceiling
        // — `thegn.slice` stayed correctly configured and EMPTY. Capping is not
        // sandboxing; an uncontained pane runs the same builds.
        let l = limits(Some("8"), Some("24g"), Some("16"));
        let argv = vec!["/bin/zsh".to_string(), "-lc".into(), "exec zsh".into()];
        let out = cap_prefix(
            Backend::None,
            true,
            &l,
            argv.clone(),
            CpuCap::ScopeHard,
            CapRole::Pane,
        );
        assert_eq!(out[0], "systemd-run");
        let joined = out.join(" ");
        assert!(
            joined.contains("--slice=thegn.slice"),
            "must join the slice"
        );
        assert!(joined.contains("CPUQuota=800%"));
        assert!(joined.contains("MemoryMax=24G"), "lowercase is normalized");
        let sep = out.iter().position(|a| a == "--").unwrap();
        assert_eq!(
            &out[sep + 1..],
            argv.as_slice(),
            "the shell survives intact"
        );
    }

    #[test]
    fn background_argv_joins_the_shared_slice() {
        // A background job (the fold gate, an agent handoff) must land in the
        // SAME slice as the panes, so the aggregate ceiling covers the whole of
        // thegn's work rather than the visible half of it.
        let l = limits(None, None, None); // all defaults ⇒ slice on
        let argv = vec!["sh".to_string(), "-c".into(), "just test".into()];
        let out = cap_prefix(
            Backend::None,
            true,
            &l,
            argv.clone(),
            CpuCap::ScopeHard,
            CapRole::Background,
        );
        assert_eq!(out[0], "systemd-run");
        assert!(out.iter().any(|a| a == "--slice=thegn.slice"));
        let sep = out.iter().position(|a| a == "--").unwrap();
        assert_eq!(&out[sep + 1..], argv.as_slice(), "the job itself is intact");

        // Explicitly disabled aggregate ⇒ the job is left alone, same as a pane.
        let off = limits(None, None, Some("off"));
        assert_eq!(
            cap_prefix(
                Backend::None,
                true,
                &off,
                argv.clone(),
                CpuCap::ScopeHard,
                CapRole::Background
            ),
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
    fn a_detected_mechanism_now_reaches_a_pane_on_every_os() {
        use CpuCap::*;
        // This assertion INVERTED when `wrap_uncontained_pane_argv` landed, and
        // the inversion is the point. It used to hold that no mechanism could
        // reach a pane off Linux, because `cap_prefix` needed a spec and a local
        // `Backend::None` never produces one. True — and it meant an uncontained
        // pane got no ceiling ANYWHERE, which is how the aggregate slice ended up
        // configured and empty. The uncontained path caps that pane directly,
        // from platform-neutral code, so a detected mechanism now applies.
        for mech in [ScopeHard, NiceSoft] {
            for os in [HostOs::Linux, HostOs::MacOs, HostOs::Windows, HostOs::Other] {
                assert!(mech.reachable_on(os), "{mech:?} on {os:?}");
            }
        }
        // `None` still reaches nothing, by definition — there is no wrapper.
        for os in [HostOs::Linux, HostOs::MacOs, HostOs::Windows] {
            assert!(!CpuCap::None.reachable_on(os));
        }

        // So the label no longer needs an OS qualifier: `doctor` reports the
        // mechanism plainly, because it genuinely applies.
        assert_eq!(NiceSoft.label_on(HostOs::Linux), NiceSoft.label());
        assert_eq!(NiceSoft.label_on(HostOs::MacOs), NiceSoft.label());
        assert_eq!(ScopeHard.label_on(HostOs::MacOs), ScopeHard.label());
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
        let out = cap_prefix(
            Backend::Bwrap,
            true,
            &l,
            argv,
            CpuCap::ScopeHard,
            CapRole::Pane,
        );
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
        let out = cap_prefix(
            Backend::None,
            true,
            &l,
            argv,
            CpuCap::ScopeHard,
            CapRole::Pane,
        );
        assert_eq!(out[0], "systemd-run");
        assert!(out.iter().any(|a| a == "--slice=thegn.slice"));
        assert!(!out.join(" ").contains("CPUQuota="));
    }

    #[test]
    fn nice_soft_fallback() {
        let l = limits(Some("2"), None, None);
        let argv = vec!["bwrap".to_string(), "true".into()];
        let out = cap_prefix(
            Backend::Bwrap,
            true,
            &l,
            argv,
            CpuCap::NiceSoft,
            CapRole::Pane,
        );
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
            cap_prefix(
                Backend::Bwrap,
                true,
                &l,
                argv.clone(),
                CpuCap::ScopeHard,
                CapRole::Pane
            ),
            argv
        );
        // Mechanism None ⇒ never wrap, even with caps set.
        let l2 = limits(Some("2"), None, None);
        assert_eq!(
            cap_prefix(
                Backend::Bwrap,
                true,
                &l2,
                argv.clone(),
                CpuCap::None,
                CapRole::Pane
            ),
            argv
        );
    }

    #[test]
    fn skips_oci_remote_and_double_wrap() {
        let l = limits(Some("2"), None, None);
        // OCI backend: not scope-wrapped (it has --cpus).
        let argv = vec!["podman".to_string(), "exec".into()];
        assert_eq!(
            cap_prefix(
                Backend::Podman,
                true,
                &l,
                argv.clone(),
                CpuCap::ScopeHard,
                CapRole::Pane
            ),
            argv
        );
        // Remote placement: deferred.
        let bw = vec!["bwrap".to_string(), "true".into()];
        assert_eq!(
            cap_prefix(
                Backend::Bwrap,
                false,
                &l,
                bw.clone(),
                CpuCap::ScopeHard,
                CapRole::Pane
            ),
            bw
        );
        // Already a systemd-run line: no double-wrap.
        let sd = vec!["systemd-run".to_string(), "--user".into()];
        assert_eq!(
            cap_prefix(
                Backend::None,
                true,
                &l,
                sd.clone(),
                CpuCap::ScopeHard,
                CapRole::Pane
            ),
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

    /// THE background/pane split: a background job joins the slice and keeps
    /// `MemoryMax`, but must NOT inherit the per-pane `CPUQuota`. Applying it
    /// capped a full merge-queue gate build at the size of one tab.
    #[test]
    fn background_drops_the_per_pane_quota_but_keeps_slice_and_memory() {
        let l = limits(Some("8"), Some("24g"), Some("16"));
        let argv = vec!["sh".to_string(), "-c".into(), "just test".into()];

        let bg = cap_prefix(
            Backend::None,
            true,
            &l,
            argv.clone(),
            CpuCap::ScopeHard,
            CapRole::Background,
        )
        .join(" ");
        assert!(bg.contains("--slice=thegn.slice"), "still joins the slice");
        assert!(bg.contains("MemoryMax=24G"), "memory is per-process, kept");
        assert!(
            !bg.contains("CPUQuota="),
            "the per-pane cpu quota must not bound a background job: {bg}"
        );

        // A pane, same limits, keeps both — this is the half that is ergonomics.
        let pane = cap_prefix(
            Backend::None,
            true,
            &l,
            argv,
            CpuCap::ScopeHard,
            CapRole::Pane,
        )
        .join(" ");
        assert!(pane.contains("CPUQuota=800%"));
        assert!(pane.contains("MemoryMax=24G"));
        assert!(pane.contains("--slice=thegn.slice"));
    }

    /// With ONLY a per-pane cpu configured and the aggregate off, a background
    /// job has nothing left to enforce and must come back untouched — not a
    /// bare `systemd-run … --` with no properties.
    #[test]
    fn background_with_only_a_pane_quota_is_left_alone() {
        let l = limits(Some("8"), None, Some("off"));
        let argv = vec!["sh".to_string(), "-c".into(), "just test".into()];
        assert_eq!(
            cap_prefix(
                Backend::None,
                true,
                &l,
                argv.clone(),
                CpuCap::ScopeHard,
                CapRole::Background
            ),
            argv
        );
    }

    #[test]
    fn cpu_possible_mask_parses_ranges_and_singletons() {
        assert_eq!(parse_cpu_possible("0-23"), Some(24));
        assert_eq!(parse_cpu_possible("0-23\n"), Some(24));
        assert_eq!(parse_cpu_possible("0"), Some(1));
        assert_eq!(parse_cpu_possible("0-1,3-5"), Some(5));
        assert_eq!(parse_cpu_possible("0,2,4"), Some(3));
        // Junk falls through to the next source rather than inventing a number.
        assert_eq!(parse_cpu_possible(""), None);
        assert_eq!(parse_cpu_possible("garbage"), None);
        assert_eq!(parse_cpu_possible("0-"), None);
        assert_eq!(parse_cpu_possible("3-1"), None);
        assert_eq!(parse_cpu_possible("0-2,"), None);
    }

    #[test]
    fn cpuinfo_processor_lines_are_counted() {
        let body = "processor\t: 0\nvendor_id\t: X\n\nprocessor\t: 1\nvendor_id\t: X\n";
        assert_eq!(parse_cpuinfo_processors(body), Some(2));
        // "processor" must be the KEY, not a substring of some other field.
        assert_eq!(parse_cpuinfo_processors("model name\t: processor\n"), None);
        assert_eq!(parse_cpuinfo_processors(""), None);
    }

    /// The two host-reading entry points, exercised for real: both are pure
    /// reads of `/sys` + `/proc` with a fallback ladder, and both must produce
    /// an answer on any host without panicking.
    #[test]
    fn host_probes_answer_on_this_machine() {
        // Whatever the source, it is a usable core count. It must NOT be
        // clamped by this test process's own cgroup — that was the bug — but a
        // test can only assert the floor without knowing the hardware.
        assert!(physical_ncpu() >= 1);
        // The fs-reading guard agrees with its pure half on this host's own
        // cgroup, whichever answer that is (a test run from inside a thegn pane
        // legitimately reports `true`).
        let expected = std::fs::read_to_string("/proc/self/cgroup")
            .map(|c| cgroup_in_thegn_slice(&c))
            .unwrap_or(false);
        assert_eq!(inside_thegn_slice(), expected);
    }

    /// The nested-instance guard. A thegn launched inside a pane scope inherits
    /// the ceiling; republishing from there is what ratcheted the shared slice
    /// down a step per nesting level.
    #[test]
    fn nested_instances_are_detected_from_proc_self_cgroup() {
        assert!(cgroup_in_thegn_slice(
            "0::/user.slice/user-1000.slice/user@1000.service/thegn.slice/run-r1.scope\n"
        ));
        assert!(cgroup_in_thegn_slice("0::/thegn.slice\n"));
        // A top-level instance is not nested.
        assert!(!cgroup_in_thegn_slice(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/foot.service\n"
        ));
        assert!(!cgroup_in_thegn_slice("0::/\n"));
        assert!(!cgroup_in_thegn_slice(""));
        // Segment match, not substring: a sibling that merely starts with the
        // name is a different unit.
        assert!(!cgroup_in_thegn_slice(
            "0::/user.slice/thegn.slice-other.scope\n"
        ));
        assert!(!cgroup_in_thegn_slice("0::/user.slice/notthegn.slice\n"));
        // cgroup v1 / hybrid lines carry the path last too.
        assert!(cgroup_in_thegn_slice(
            "1:cpu,cpuacct:/user.slice/thegn.slice/x.scope\n"
        ));
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
