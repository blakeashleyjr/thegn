//! Remote sandbox health: the one-exec in-sandbox probe, its parser, and the
//! restart / OOM classification that turns two samples into a specific warning
//! (OpenSpec `remote-sandbox-visibility`).
//!
//! Pure: no I/O. The host runs [`PROBE_SCRIPT`] through the provider's exec API
//! (one short `sh -c`), hands the captured output to [`parse_probe`], and
//! compares samples with [`classify`]. Every probe field is optional: a sandbox
//! without `/proc/pressure`, cgroup v2 or a readable kernel log still yields a
//! sample with the fields it could read.

use serde::{Deserialize, Serialize};

/// The probe's first line; output without it is refused (not a thegn probe).
pub const PROBE_MAGIC: &str = "thegn-health v1";

/// Largest probe output parsed; longer input is truncated before parsing.
pub const MAX_PROBE_BYTES: usize = 16 * 1024;

/// Kernel OOM-kill lines kept from one probe (newest last).
pub const MAX_OOM_LINES: usize = 5;

/// Memory use at or above this share of the limit is "under pressure".
pub const MEM_PRESSURE_PERCENT: u64 = 90;

/// The in-sandbox probe: POSIX sh, read-only, no package dependencies; every
/// read tolerates a missing file. One line per fact, `key value…`.
pub const PROBE_SCRIPT: &str = r#"echo 'thegn-health v1'
echo "boot_id $(cat /proc/sys/kernel/random/boot_id 2>/dev/null)"
echo "uptime $(cut -d' ' -f1 /proc/uptime 2>/dev/null)"
echo "loadavg $(cut -d' ' -f1-3 /proc/loadavg 2>/dev/null)"
echo "cpus $(nproc 2>/dev/null)"
grep -E '^(MemTotal|MemAvailable):' /proc/meminfo 2>/dev/null
echo "cg_mem_max $(cat /sys/fs/cgroup/memory.max 2>/dev/null)"
echo "cg_mem_current $(cat /sys/fs/cgroup/memory.current 2>/dev/null)"
echo "cg_oom_kill $(sed -n 's/^oom_kill //p' /sys/fs/cgroup/memory.events 2>/dev/null)"
echo "psi_mem $(sed -n 's/^some avg10=\([0-9.]*\).*/\1/p' /proc/pressure/memory 2>/dev/null)"
echo "df $(df -Pk / 2>/dev/null | tail -n 1)"
dmesg 2>/dev/null | grep -E 'Killed process [0-9]+ ' | tail -n 5 | sed 's/^/oom /'
"#;

/// One process the kernel OOM killer reaped, from a kernel log line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OomKill {
    /// The command name in the kernel's parentheses (`rustc`).
    pub process: String,
    /// `anon-rss` at kill time, KiB, when the line carried it.
    pub rss_kib: Option<u64>,
}

/// One probe sample. `None` = the sandbox could not report that fact.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HealthSample {
    pub boot_id: Option<String>,
    pub uptime_secs: Option<u64>,
    /// 1, 5 and 15 minute load averages.
    pub load: Option<[f64; 3]>,
    pub cpus: Option<u32>,
    pub mem_total_kib: Option<u64>,
    pub mem_available_kib: Option<u64>,
    /// cgroup v2 `memory.max` (`None` when absent or `max`).
    pub cgroup_mem_max_bytes: Option<u64>,
    pub cgroup_mem_current_bytes: Option<u64>,
    /// cgroup v2 `memory.events` `oom_kill` counter.
    pub cgroup_oom_kills: Option<u64>,
    /// `/proc/pressure/memory` `some avg10` (percent of time stalled).
    pub mem_psi_avg10: Option<f64>,
    pub disk_total_kib: Option<u64>,
    pub disk_used_kib: Option<u64>,
    pub disk_avail_kib: Option<u64>,
    /// Kernel OOM kills in the current boot (newest last, at most
    /// [`MAX_OOM_LINES`]).
    pub oom_kills: Vec<OomKill>,
}

impl HealthSample {
    /// Memory `(used, limit)` in bytes: the cgroup limit when one is set (that
    /// is what the OOM killer enforces), else the VM's `MemTotal`.
    pub fn memory(&self) -> Option<(u64, u64)> {
        if let (Some(max), Some(cur)) = (self.cgroup_mem_max_bytes, self.cgroup_mem_current_bytes) {
            return Some((cur, max));
        }
        let total = self.mem_total_kib?;
        let avail = self.mem_available_kib?;
        Some((
            total.saturating_sub(avail).saturating_mul(1024),
            total.saturating_mul(1024),
        ))
    }

    /// Memory used as a whole percent of the limit.
    pub fn memory_percent(&self) -> Option<u64> {
        let (used, limit) = self.memory()?;
        (limit > 0).then(|| used.saturating_mul(100) / limit)
    }

    /// Disk used as a whole percent of `/`.
    pub fn disk_percent(&self) -> Option<u64> {
        let total = self.disk_total_kib?;
        let used = self.disk_used_kib?;
        (total > 0).then(|| used.saturating_mul(100) / total)
    }
}

/// Parse the probe's output. `Err` only when the output is not a probe's (the
/// magic line is missing); unknown or malformed lines are skipped.
pub fn parse_probe(output: &str) -> Result<HealthSample, String> {
    let mut end = output.len().min(MAX_PROBE_BYTES);
    while !output.is_char_boundary(end) {
        end -= 1;
    }
    let output = &output[..end];
    let mut lines = output.lines().map(str::trim).skip_while(|l| l.is_empty());
    if lines.next() != Some(PROBE_MAGIC) {
        return Err("the health probe printed no thegn-health header".into());
    }
    let mut s = HealthSample::default();
    for line in lines {
        let (key, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let rest = rest.trim();
        match key {
            "boot_id" => {
                s.boot_id = (!rest.is_empty() && rest.len() <= 64 && rest.is_ascii())
                    .then(|| rest.to_string());
            }
            "uptime" => {
                s.uptime_secs = rest
                    .parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite() && *v >= 0.0)
                    .map(|v| v as u64);
            }
            "loadavg" => {
                let v: Vec<f64> = rest
                    .split_whitespace()
                    .filter_map(|x| x.parse().ok())
                    .collect();
                s.load = (v.len() == 3).then(|| [v[0], v[1], v[2]]);
            }
            "cpus" => s.cpus = rest.parse().ok(),
            "MemTotal:" => s.mem_total_kib = kib_field(rest),
            "MemAvailable:" => s.mem_available_kib = kib_field(rest),
            "cg_mem_max" => s.cgroup_mem_max_bytes = rest.parse().ok(),
            "cg_mem_current" => s.cgroup_mem_current_bytes = rest.parse().ok(),
            "cg_oom_kill" => s.cgroup_oom_kills = rest.parse().ok(),
            "psi_mem" => s.mem_psi_avg10 = rest.parse().ok().filter(|v: &f64| v.is_finite()),
            "df" => {
                // `Filesystem 1024-blocks Used Available Capacity Mounted`
                let cols: Vec<&str> = rest.split_whitespace().collect();
                if cols.len() >= 4 {
                    s.disk_total_kib = parse_kib(cols[1]);
                    s.disk_used_kib = parse_kib(cols[2]);
                    s.disk_avail_kib = parse_kib(cols[3]);
                }
            }
            "oom" => {
                if let Some(kill) = parse_oom_line(rest) {
                    s.oom_kills.push(kill);
                }
            }
            _ => {}
        }
    }
    if s.oom_kills.len() > MAX_OOM_LINES {
        let drop = s.oom_kills.len() - MAX_OOM_LINES;
        s.oom_kills.drain(..drop);
    }
    Ok(s)
}

/// `16384 kB` → 16384.
fn kib_field(rest: &str) -> Option<u64> {
    rest.split_whitespace().next()?.parse().ok()
}

/// A kernel line `… Killed process 1234 (rustc) total-vm:…, anon-rss:3791232kB, …`.
pub fn parse_oom_line(line: &str) -> Option<OomKill> {
    let at = line.find("Killed process ")?;
    let rest = &line[at..];
    let open = rest.find('(')?;
    let close = open + rest[open..].find(')')?;
    let process: String = rest[open + 1..close]
        .chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect();
    if process.is_empty() {
        return None;
    }
    let rss_kib = rest.find("anon-rss:").and_then(|i| {
        let digits: String = rest[i + "anon-rss:".len()..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse().ok()
    });
    Some(OomKill { process, rss_kib })
}

/// Why a sandbox restarted, as far as two samples can tell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RestartCause {
    /// The last sample before the restart showed memory at or above
    /// [`MEM_PRESSURE_PERCENT`] of the limit, or OOM kills.
    LikelyOutOfMemory {
        used_bytes: Option<u64>,
        limit_bytes: Option<u64>,
    },
    /// The provider reported the sandbox cold between the two samples: it
    /// slept on idle and woke on the next request (see [`refine`]).
    IdleSleep,
    /// The sandbox is at or past its configured `max_lifetime_secs`.
    LifetimeExpired,
    /// Nothing in the previous sample or the provider facts explains it
    /// (provider recycle needs provider events; see the design).
    Unknown,
}

/// A notable change between two samples of one sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum HealthEvent {
    /// The boot id changed: the sandbox rebooted between the samples.
    Restarted {
        cause: RestartCause,
        uptime_secs: Option<u64>,
    },
    /// The kernel killed processes in the current boot since the last sample.
    OomKilled { kills: Vec<OomKill> },
}

/// Compare the previous sample (if any) with the current one.
///
/// A restart needs both boot ids; without a previous sample, OOM kills in the
/// current boot are reported as-is.
pub fn classify(prev: Option<&HealthSample>, cur: &HealthSample) -> Vec<HealthEvent> {
    let mut events = Vec::new();
    let rebooted_from = prev.filter(|p| {
        matches!(
            (p.boot_id.as_deref(), cur.boot_id.as_deref()),
            (Some(a), Some(b)) if a != b
        )
    });
    let rebooted = rebooted_from.is_some();
    if let Some(prev) = rebooted_from {
        let pressured = prev
            .memory_percent()
            .is_some_and(|p| p >= MEM_PRESSURE_PERCENT)
            || !prev.oom_kills.is_empty();
        let cause = if pressured {
            let (used, limit) = prev
                .memory()
                .map_or((None, None), |(u, l)| (Some(u), Some(l)));
            RestartCause::LikelyOutOfMemory {
                used_bytes: used,
                limit_bytes: limit,
            }
        } else {
            RestartCause::Unknown
        };
        events.push(HealthEvent::Restarted {
            cause,
            uptime_secs: cur.uptime_secs,
        });
    }
    // Kills already seen in the same boot are not news.
    let seen = match prev {
        Some(p) if !rebooted => p.oom_kills.as_slice(),
        _ => &[],
    };
    let new: Vec<OomKill> = cur
        .oom_kills
        .iter()
        .skip(common_prefix_tail(seen, &cur.oom_kills))
        .cloned()
        .collect();
    if !new.is_empty() {
        events.push(HealthEvent::OomKilled { kills: new });
    }
    events
}

/// How many leading entries of `cur` were already in `seen` (the kernel log is
/// append-only, so `seen`'s tail aligns with a prefix of `cur`).
fn common_prefix_tail(seen: &[OomKill], cur: &[OomKill]) -> usize {
    (0..=seen.len().min(cur.len()))
        .rev()
        .find(|&n| seen[seen.len() - n..] == cur[..n])
        .unwrap_or(0)
}

/// `3.7 GB` (decimal, one place), the unit the warnings use.
pub fn gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1_000_000_000.0)
}

/// The one-line warning for an event: specific, never "stream ended". `what` is
/// the sandbox noun (`sprite`, `sandbox`).
pub fn warning(what: &str, event: &HealthEvent) -> String {
    match event {
        HealthEvent::Restarted { cause, .. } => match cause {
            RestartCause::LikelyOutOfMemory {
                used_bytes: Some(used),
                limit_bytes: Some(limit),
            } => format!(
                "{what} restarted: likely out of memory ({} of {} used before the restart)",
                gb(*used),
                gb(*limit)
            ),
            RestartCause::LikelyOutOfMemory { .. } => {
                format!("{what} restarted: likely out of memory")
            }
            RestartCause::IdleSleep => format!("{what} restarted: it slept while idle"),
            RestartCause::LifetimeExpired => {
                format!("{what} restarted: it reached its maximum lifetime")
            }
            RestartCause::Unknown => format!("{what} restarted (cause unknown)"),
        },
        HealthEvent::OomKilled { kills } => {
            let named: Vec<String> = kills
                .iter()
                .map(|k| match k.rss_kib {
                    Some(kib) => format!("{} {}", k.process, gb(kib.saturating_mul(1024))),
                    None => k.process.clone(),
                })
                .collect();
            format!("{what} out of memory: kernel killed {}", named.join(", "))
        }
    }
}

/// Time left below which the lifetime-expiry warning fires (one hour).
pub const LIFETIME_WARN_SECS: i64 = 3_600;

/// The state-DB key of one remote sandbox: its env name and provider id.
pub fn sandbox_key(env: &str, id: &str) -> String {
    format!("{env}/{id}")
}

/// Provider-side facts that tell an unexplained restart apart (design: "Restart
/// and OOM classification").
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProviderFacts {
    /// The provider API reported the sandbox cold, warm or stopped at some
    /// point between the two samples.
    pub cold_between_samples: bool,
    /// Seconds since the provider created the sandbox.
    pub age_secs: Option<u64>,
    /// The env's configured `max_lifetime_secs` (0 = none).
    pub max_lifetime_secs: u64,
}

/// Replace a restart's `Unknown` cause with what the provider facts show:
/// lifetime expiry first, then idle sleep. A likely-OOM cause is kept (the
/// previous sample is stronger evidence) and non-restart events pass through.
pub fn refine(event: HealthEvent, facts: &ProviderFacts) -> HealthEvent {
    let HealthEvent::Restarted {
        cause: RestartCause::Unknown,
        uptime_secs,
    } = &event
    else {
        return event;
    };
    let expired = facts.max_lifetime_secs > 0
        && facts
            .age_secs
            .is_some_and(|age| age >= facts.max_lifetime_secs);
    let cause = if expired {
        RestartCause::LifetimeExpired
    } else if facts.cold_between_samples {
        RestartCause::IdleSleep
    } else {
        return event;
    };
    HealthEvent::Restarted {
        cause,
        uptime_secs: *uptime_secs,
    }
}

/// The warning for a sandbox whose lifetime is nearly or already used up
/// (`left_secs` negative = overdue); `None` while at least
/// [`LIFETIME_WARN_SECS`] remain.
pub fn lifetime_warning(what: &str, left_secs: i64) -> Option<String> {
    if left_secs >= LIFETIME_WARN_SECS {
        None
    } else if left_secs <= 0 {
        Some(format!("{what} is past its maximum lifetime"))
    } else {
        Some(format!(
            "{what} reaches its maximum lifetime in {}",
            short_duration(left_secs as u64)
        ))
    }
}

/// The journal `kind` of an event (`restart` / `oom_kill`, per the design).
pub fn event_kind(event: &HealthEvent) -> &'static str {
    match event {
        HealthEvent::Restarted { .. } => "restart",
        HealthEvent::OomKilled { .. } => "oom_kill",
    }
}

/// The health dot for a sample: `Ok`, `Warn` (memory or disk ≥ 90%, or OOM
/// kills this boot), `Unknown` (no sample).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthLevel {
    Ok,
    Warn,
    Unknown,
}

pub fn level(sample: Option<&HealthSample>) -> HealthLevel {
    let Some(s) = sample else {
        return HealthLevel::Unknown;
    };
    let hot = |p: Option<u64>| p.is_some_and(|p| p >= MEM_PRESSURE_PERCENT);
    if hot(s.memory_percent()) || hot(s.disk_percent()) || !s.oom_kills.is_empty() {
        HealthLevel::Warn
    } else {
        HealthLevel::Ok
    }
}

/// `3d 4h`, `2h 5m`, `7m`, `42s`.
pub fn short_duration(secs: u64) -> String {
    let (d, h, m) = (secs / 86_400, secs / 3_600 % 24, secs / 60 % 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{secs}s")
    }
}

/// Samples kept per sandbox for the Observe sparklines.
pub const HISTORY_LEN: usize = 60;

/// The newest health facts for one sandbox, as the surfaces read them: the
/// latest sample, a bounded history for sparklines, the current warning line
/// and the provider's lifetime left. Slice B's sampler fills it (through
/// [`HealthCache::record`]); the sidebar, fleet popup and Observe tab only read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HealthReading {
    /// The latest sample; `None` when no probe has succeeded yet.
    pub sample: Option<HealthSample>,
    /// Up to [`HISTORY_LEN`] samples, oldest first, newest last.
    pub history: Vec<HealthSample>,
    /// When the latest probe finished (unix ms).
    pub taken_ms: i64,
    /// The newest restart / OOM warning line, if one is unacknowledged.
    pub warning: Option<String>,
    /// Seconds of provider lifetime left as of `taken_ms`.
    pub lifetime_left_secs: Option<u64>,
    /// Why the last probe failed (shown verbatim; the level is `Unknown`).
    pub error: Option<String>,
}

impl HealthReading {
    /// The dot: the sample's level, raised to `Warn` by an unacknowledged
    /// warning or a lifetime under an hour.
    pub fn level(&self) -> HealthLevel {
        let base = level(self.sample.as_ref());
        let short = self
            .lifetime_left_secs
            .is_some_and(|s| i64::try_from(s).is_ok_and(|s| s < LIFETIME_WARN_SECS));
        if self.warning.is_some() || (short && base != HealthLevel::Unknown) {
            HealthLevel::Warn
        } else {
            base
        }
    }

    /// Lifetime left at `now_ms`, counting down from the sample time.
    pub fn lifetime_left_at(&self, now_ms: i64) -> Option<u64> {
        let left = self.lifetime_left_secs?;
        let elapsed = (now_ms.saturating_sub(self.taken_ms)).max(0) as u64 / 1000;
        Some(left.saturating_sub(elapsed))
    }

    /// The text the surfaces show next to the dot: the warning, else a probe
    /// error, else nothing.
    pub fn note(&self) -> Option<&str> {
        self.warning.as_deref().or(self.error.as_deref())
    }

    /// CPU load per CPU, 0..=1, oldest first (`-` samples as 0).
    pub fn cpu_series(&self) -> Vec<f32> {
        self.history
            .iter()
            .map(|s| match (s.load, s.cpus) {
                (Some(l), Some(c)) if c > 0 => (l[0] / f64::from(c)).clamp(0.0, 1.0) as f32,
                (Some(l), None) => l[0].clamp(0.0, 1.0) as f32,
                _ => 0.0,
            })
            .collect()
    }

    /// Memory used fraction, 0..=1, oldest first.
    pub fn mem_series(&self) -> Vec<f32> {
        self.history
            .iter()
            .map(|s| {
                s.memory_percent()
                    .map_or(0.0, |p| p.min(100) as f32 / 100.0)
            })
            .collect()
    }

    /// Disk used fraction, 0..=1, oldest first.
    pub fn disk_series(&self) -> Vec<f32> {
        self.history
            .iter()
            .map(|s| s.disk_percent().map_or(0.0, |p| p.min(100) as f32 / 100.0))
            .collect()
    }
}

/// Where the surfaces read health from, keyed by the worktree path the
/// sandbox belongs to. A trait so slice B's sampler (or a test) can supply
/// readings without the UI knowing the store.
pub trait HealthSource {
    fn reading(&self, key: &str) -> Option<HealthReading>;
}

/// The in-memory latest-reading store. Cheap to read (one lock, no I/O).
#[derive(Debug, Default)]
pub struct HealthCache {
    map: std::sync::Mutex<std::collections::HashMap<String, HealthReading>>,
}

impl HealthCache {
    fn lock(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, HealthReading>> {
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Store a successful probe: replaces the latest sample, appends to the
    /// bounded history and takes the sampler's warning / lifetime facts.
    pub fn record(
        &self,
        key: &str,
        sample: HealthSample,
        taken_ms: i64,
        warning: Option<String>,
        lifetime_left_secs: Option<u64>,
    ) {
        let mut map = self.lock();
        let entry = map.entry(key.to_string()).or_default();
        entry.history.push(sample.clone());
        if entry.history.len() > HISTORY_LEN {
            let drop = entry.history.len() - HISTORY_LEN;
            entry.history.drain(..drop);
        }
        entry.sample = Some(sample);
        entry.taken_ms = taken_ms;
        entry.warning = warning;
        entry.lifetime_left_secs = lifetime_left_secs;
        entry.error = None;
    }

    /// Store a failed probe: the sample is dropped (never fabricated), the
    /// history stays so the graphs keep their shape.
    pub fn record_error(&self, key: &str, error: String, taken_ms: i64) {
        let mut map = self.lock();
        let entry = map.entry(key.to_string()).or_default();
        entry.sample = None;
        entry.taken_ms = taken_ms;
        entry.error = Some(error);
    }

    /// Forget a sandbox (env down).
    pub fn remove(&self, key: &str) {
        self.lock().remove(key);
    }

    /// The level and note only: what the sidebar needs, without cloning the
    /// history.
    pub fn badge(&self, key: &str) -> Option<(HealthLevel, Option<String>)> {
        let map = self.lock();
        let r = map.get(key)?;
        Some((r.level(), r.note().map(str::to_string)))
    }
}

impl HealthSource for HealthCache {
    fn reading(&self, key: &str) -> Option<HealthReading> {
        self.lock().get(key).cloned()
    }
}

/// The process-wide cache the host sampler feeds and the surfaces read.
pub fn global() -> &'static HealthCache {
    static CACHE: std::sync::OnceLock<HealthCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(HealthCache::default)
}

/// A `df -k` size column; `None` for a non-numeric cell.
fn parse_kib(cell: &str) -> Option<u64> {
    cell.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "thegn-health v1
boot_id 6c1d2a9e-0000-4000-8000-000000000001
uptime 3725.41
loadavg 1.50 0.75 0.25
cpus 8
MemTotal:       16384000 kB
MemAvailable:    4096000 kB
cg_mem_max max
cg_mem_current 1234
cg_oom_kill 1
psi_mem 2.50
df overlay 41943040 10485760 31457280 25% /
oom [  812.1] Out of memory: Killed process 4242 (rustc) total-vm:5000000kB, anon-rss:3612672kB, file-rss:0kB
";

    #[test]
    fn parses_a_full_probe() {
        let s = parse_probe(SAMPLE).unwrap();
        assert_eq!(
            s.boot_id.as_deref(),
            Some("6c1d2a9e-0000-4000-8000-000000000001")
        );
        assert_eq!(s.uptime_secs, Some(3725));
        assert_eq!(s.load, Some([1.5, 0.75, 0.25]));
        assert_eq!(s.cpus, Some(8));
        // `memory.max` = `max` → no cgroup limit, so memory falls back to /proc.
        assert_eq!(s.cgroup_mem_max_bytes, None);
        assert_eq!(s.memory(), Some((12_288_000 * 1024, 16_384_000 * 1024)));
        assert_eq!(s.memory_percent(), Some(75));
        assert_eq!(s.cgroup_oom_kills, Some(1));
        assert_eq!(s.mem_psi_avg10, Some(2.5));
        assert_eq!(s.disk_percent(), Some(25));
        assert_eq!(
            s.oom_kills,
            vec![OomKill {
                process: "rustc".into(),
                rss_kib: Some(3_612_672)
            }]
        );
    }

    #[test]
    fn missing_facts_stay_none_and_garbage_is_refused() {
        let s = parse_probe("thegn-health v1\nboot_id \nuptime \ndf \npsi_mem \n").unwrap();
        assert_eq!(s, HealthSample::default());
        assert!(parse_probe("sh: not found\n").is_err());
        assert!(parse_probe("").is_err());
    }

    #[test]
    fn cgroup_limit_wins_over_meminfo() {
        let s = parse_probe(
            "thegn-health v1\nMemTotal: 1000 kB\nMemAvailable: 900 kB\ncg_mem_max 4000\ncg_mem_current 3800\n",
        )
        .unwrap();
        assert_eq!(s.memory(), Some((3800, 4000)));
        assert_eq!(s.memory_percent(), Some(95));
        assert_eq!(level(Some(&s)), HealthLevel::Warn);
    }

    #[test]
    fn oom_lines_are_capped_to_the_newest() {
        let mut text = String::from("thegn-health v1\n");
        for i in 0..8 {
            text.push_str(&format!("oom Killed process {i} (p{i})\n"));
        }
        let s = parse_probe(&text).unwrap();
        assert_eq!(s.oom_kills.len(), MAX_OOM_LINES);
        assert_eq!(s.oom_kills.last().unwrap().process, "p7");
    }

    #[test]
    fn oom_line_table() {
        let cases = [
            (
                "Killed process 1 (cc1plus) anon-rss:10kB",
                Some(("cc1plus", Some(10))),
            ),
            ("Killed process 1 (ld)", Some(("ld", None))),
            ("Killed process 1 ()", None),
            ("Out of memory: no process", None),
        ];
        for (line, want) in cases {
            let got = parse_oom_line(line);
            let want = want.map(|(p, r)| OomKill {
                process: p.into(),
                rss_kib: r,
            });
            assert_eq!(got, want, "{line}");
        }
    }

    fn sample(boot: &str, used: u64, limit: u64, kills: &[&str]) -> HealthSample {
        HealthSample {
            boot_id: Some(boot.into()),
            uptime_secs: Some(30),
            cgroup_mem_max_bytes: Some(limit),
            cgroup_mem_current_bytes: Some(used),
            oom_kills: kills
                .iter()
                .map(|p| OomKill {
                    process: (*p).into(),
                    rss_kib: None,
                })
                .collect(),
            ..HealthSample::default()
        }
    }

    #[test]
    fn classify_table() {
        let gbs = 1_000_000_000;
        let cases: Vec<(&str, Option<HealthSample>, HealthSample, Vec<String>)> = vec![
            ("first sample, quiet", None, sample("a", 1, 10, &[]), vec![]),
            (
                "first sample, oom this boot",
                None,
                sample("a", 1, 10, &["rustc"]),
                vec!["sprite out of memory: kernel killed rustc".into()],
            ),
            (
                "same boot, nothing new",
                Some(sample("a", 1, 10, &["rustc"])),
                sample("a", 1, 10, &["rustc"]),
                vec![],
            ),
            (
                "same boot, new kill",
                Some(sample("a", 1, 10, &["rustc"])),
                sample("a", 1, 10, &["rustc", "ld"]),
                vec!["sprite out of memory: kernel killed ld".into()],
            ),
            (
                "reboot after memory pressure",
                Some(sample("a", 37 * gbs / 10, 4 * gbs, &[])),
                sample("b", 0, 4 * gbs, &[]),
                vec!["sprite restarted: likely out of memory (3.7 GB of 4.0 GB used before the restart)".into()],
            ),
            (
                "reboot, quiet before",
                Some(sample("a", 1, 10, &[])),
                sample("b", 1, 10, &[]),
                vec!["sprite restarted (cause unknown)".into()],
            ),
        ];
        for (name, prev, cur, want) in cases {
            let got: Vec<String> = classify(prev.as_ref(), &cur)
                .iter()
                .map(|e| warning("sprite", e))
                .collect();
            assert_eq!(got, want, "{name}");
        }
    }

    #[test]
    fn level_and_durations() {
        assert_eq!(level(None), HealthLevel::Unknown);
        assert_eq!(level(Some(&sample("a", 1, 10, &[]))), HealthLevel::Ok);
        assert_eq!(short_duration(42), "42s");
        assert_eq!(short_duration(7 * 60 + 5), "7m");
        assert_eq!(short_duration(2 * 3600 + 5 * 60), "2h 5m");
        assert_eq!(short_duration(3 * 86_400 + 4 * 3600), "3d 4h");
    }

    #[test]
    fn refine_names_the_restart_cause_from_provider_facts() {
        let unknown = HealthEvent::Restarted {
            cause: RestartCause::Unknown,
            uptime_secs: Some(5),
        };
        let cause = |e: HealthEvent| match e {
            HealthEvent::Restarted { cause, .. } => cause,
            other => panic!("not a restart: {other:?}"),
        };
        let cold = ProviderFacts {
            cold_between_samples: true,
            ..ProviderFacts::default()
        };
        assert_eq!(
            cause(refine(unknown.clone(), &cold)),
            RestartCause::IdleSleep
        );
        let expired = ProviderFacts {
            cold_between_samples: true,
            age_secs: Some(7200),
            max_lifetime_secs: 3600,
        };
        assert_eq!(
            cause(refine(unknown.clone(), &expired)),
            RestartCause::LifetimeExpired
        );
        let young = ProviderFacts {
            age_secs: Some(60),
            max_lifetime_secs: 3600,
            ..ProviderFacts::default()
        };
        assert_eq!(
            cause(refine(unknown.clone(), &young)),
            RestartCause::Unknown
        );
        // A likely-OOM cause and a non-restart event are never rewritten.
        let oom = HealthEvent::Restarted {
            cause: RestartCause::LikelyOutOfMemory {
                used_bytes: None,
                limit_bytes: None,
            },
            uptime_secs: None,
        };
        assert_eq!(refine(oom.clone(), &expired), oom);
        let kill = HealthEvent::OomKilled { kills: Vec::new() };
        assert_eq!(refine(kill.clone(), &expired), kill);
        assert_eq!(
            warning("sprite", &refine(unknown, &cold)),
            "sprite restarted: it slept while idle"
        );
    }

    #[test]
    fn lifetime_warning_table() {
        assert_eq!(lifetime_warning("sprite", LIFETIME_WARN_SECS), None);
        assert_eq!(
            lifetime_warning("sprite", 1_500).as_deref(),
            Some("sprite reaches its maximum lifetime in 25m")
        );
        assert_eq!(
            lifetime_warning("sprite", 0).as_deref(),
            Some("sprite is past its maximum lifetime")
        );
        assert_eq!(
            lifetime_warning("sprite", -5).as_deref(),
            Some("sprite is past its maximum lifetime")
        );
    }

    #[test]
    fn sandbox_key_and_event_kind() {
        assert_eq!(sandbox_key("sx", "s-1"), "sx/s-1");
        assert_eq!(
            event_kind(&HealthEvent::OomKilled { kills: Vec::new() }),
            "oom_kill"
        );
    }

    fn sample_lm(load: f64, mem_pct: u64) -> HealthSample {
        HealthSample {
            load: Some([load, 0.0, 0.0]),
            cpus: Some(4),
            mem_total_kib: Some(1000),
            mem_available_kib: Some(1000 - mem_pct * 10),
            disk_total_kib: Some(100),
            disk_used_kib: Some(50),
            ..Default::default()
        }
    }

    #[test]
    fn cache_records_history_bounded_and_levels() {
        let c = HealthCache::default();
        assert!(c.reading("w").is_none());
        for i in 0..(HISTORY_LEN + 5) {
            c.record("w", sample_lm(1.0, 10), i as i64, None, Some(7200));
        }
        let r = c.reading("w").unwrap();
        assert_eq!(r.history.len(), HISTORY_LEN);
        assert_eq!(r.level(), HealthLevel::Ok);
        assert_eq!(r.cpu_series().len(), HISTORY_LEN);
        assert!((r.cpu_series()[0] - 0.25).abs() < 1e-6);
        assert!((r.mem_series()[0] - 0.10).abs() < 1e-6);
        assert!((r.disk_series()[0] - 0.50).abs() < 1e-6);
        c.record(
            "w",
            sample_lm(1.0, 95),
            9,
            Some("sprite restarted".into()),
            Some(7200),
        );
        assert_eq!(c.badge("w").unwrap().0, HealthLevel::Warn);
        assert_eq!(c.badge("w").unwrap().1.as_deref(), Some("sprite restarted"));
    }

    #[test]
    fn short_lifetime_warns_and_counts_down() {
        let c = HealthCache::default();
        c.record("w", sample_lm(0.1, 10), 1_000, None, Some(3_700));
        let r = c.reading("w").unwrap();
        assert_eq!(r.level(), HealthLevel::Ok);
        assert_eq!(r.lifetime_left_at(1_000 + 200_000), Some(3_500));
        c.record("w", sample_lm(0.1, 10), 1_000, None, Some(600));
        assert_eq!(c.badge("w").unwrap().0, HealthLevel::Warn);
        assert_eq!(
            c.reading("w").unwrap().lifetime_left_at(10_000_000),
            Some(0)
        );
    }

    #[test]
    fn probe_error_is_unknown_and_keeps_history() {
        let c = HealthCache::default();
        c.record("w", sample_lm(0.1, 10), 1, None, None);
        c.record_error("w", "exec refused".into(), 2);
        let r = c.reading("w").unwrap();
        assert_eq!(r.level(), HealthLevel::Unknown);
        assert_eq!(r.note(), Some("exec refused"));
        assert_eq!(r.history.len(), 1);
        c.remove("w");
        assert!(c.reading("w").is_none());
    }
}
