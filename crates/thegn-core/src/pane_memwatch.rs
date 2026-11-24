//! Pure decision logic for the per-pane memory watch (THE-2600 follow-up).
//!
//! Each interactive pane runs in its own transient systemd scope under
//! [`crate::sandbox_cpucap::CPU_SLICE`]. The host samples that scope's cgroup v2
//! files off the event loop and feeds the numbers here. Everything in this
//! module is pure — parsers over file bodies, a threshold state machine, the
//! restart guard and the notice text — so the I/O stays in the host and the
//! behaviour is testable with injected samples and an injected clock.
//!
//! Three behaviours hang off it:
//! - **Warn** when `memory.current` crosses [`WARN_PERCENT`] of `memory.max` or
//!   the `memory.events` `high` counter starts rising; re-armed once usage
//!   drops below [`REARM_PERCENT`] ([`step`]).
//! - **Report** an OOM kill when the `oom_kill` counter increments ([`step`]).
//! - **Restart** a pane whose shell died *together with* an `oom_kill`, at most
//!   once per [`RESTART_COOLDOWN_MS`] ([`restart_verdict`]).

use crate::harness;
use crate::util::sh_quote;

/// `memory.current` at or above this share of `memory.max` raises the warning.
pub const WARN_PERCENT: u64 = 85;
/// Usage must fall below this share of `memory.max` before the warning re-arms.
pub const REARM_PERCENT: u64 = 75;
/// A pane exit counts as memory-caused when an `oom_kill` was seen this recently.
/// Wider than systemd's 90 s stop timeout, so the scope teardown still matches.
pub const OOM_EXIT_WINDOW_MS: u64 = 180_000;
/// Minimum gap between two automatic restarts of the same pane slot.
pub const RESTART_COOLDOWN_MS: u64 = 600_000;

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;

/// The counters of a `memory.events` file that the watch cares about.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemEventCounts {
    pub high: u64,
    pub max: u64,
    pub oom: u64,
    pub oom_kill: u64,
}

/// One reading of a pane scope's memory files.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemSample {
    /// `memory.current` in bytes.
    pub current: u64,
    /// `memory.max` in bytes; `None` for `max` (uncapped) or an unreadable file.
    pub max: Option<u64>,
    /// `memory.swap.current` in bytes (0 when the file is absent).
    pub swap_current: u64,
    pub events: MemEventCounts,
}

impl MemSample {
    /// Usage as a whole percent of the cap; `None` without a (non-zero) cap.
    pub fn percent_of_max(&self) -> Option<u64> {
        let max = self.max.filter(|m| *m > 0)?;
        Some(self.current.saturating_mul(100) / max)
    }
}

/// Parse a `memory.events` body (`key value` lines); absent keys read as 0.
pub fn parse_events(text: &str) -> MemEventCounts {
    let mut counts = MemEventCounts::default();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        let (Some(key), Some(value)) = (words.next(), words.next()) else {
            continue;
        };
        let Ok(value) = value.parse::<u64>() else {
            continue;
        };
        match key {
            "high" => counts.high = value,
            "max" => counts.max = value,
            "oom" => counts.oom = value,
            "oom_kill" => counts.oom_kill = value,
            _ => {}
        }
    }
    counts
}

/// A single-integer cgroup file (`memory.current`, `memory.swap.current`).
pub fn parse_u64(text: &str) -> Option<u64> {
    text.trim().parse().ok()
}

/// `memory.max`: `Some(bytes)` for a number, `None` for `max` or junk.
pub fn parse_limit(text: &str) -> Option<u64> {
    parse_u64(text)
}

/// The cgroup path of the pane scope in a `/proc/<pid>/cgroup` body: the v2
/// (`0::`) entry, only when it is a `.scope` under the thegn slice. A process
/// outside the slice (uncapped pane, nested instance on another slice) has no
/// pane scope to watch.
pub fn pane_scope_path(proc_cgroup: &str) -> Option<&str> {
    proc_cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(str::trim)
        .filter(|path| {
            path.ends_with(".scope")
                && path
                    .split('/')
                    .any(|seg| seg == crate::sandbox_cpucap::CPU_SLICE)
        })
}

/// What one sample raised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// Usage is near the cap (`near_cap`) and/or the `high` counter rose.
    Warn { near_cap: bool, high_rising: bool },
    /// `oom_kill` rose by `kills` since the previous sample.
    OomKilled { kills: u64 },
}

/// Per-pane threshold state carried between samples.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WatchState {
    /// A warning is outstanding; cleared once usage drops below [`REARM_PERCENT`].
    warned: bool,
    /// The persistent per-pane badge: set at [`WARN_PERCENT`], cleared below
    /// [`REARM_PERCENT`]. Independent of `warned` (a rising `high` counter arms
    /// the one-shot warning without making the pane "near the cap").
    badged: bool,
    last_high: Option<u64>,
    last_oom_kill: Option<u64>,
}

impl WatchState {
    /// Whether the pane should carry the near-cap indicator right now.
    pub fn badged(&self) -> bool {
        self.badged
    }
}

/// Fold one sample into `state` and return the signals it raised. The first
/// sample only sets the counter baselines — a pane that already carries history
/// (a thegn restart re-adopting a long-lived scope) must not fire a stale OOM.
pub fn step(state: &mut WatchState, sample: &MemSample) -> Vec<Signal> {
    let mut out = Vec::new();
    if let Some(prev) = state.last_oom_kill
        && sample.events.oom_kill > prev
    {
        out.push(Signal::OomKilled {
            kills: sample.events.oom_kill - prev,
        });
    }
    state.last_oom_kill = Some(sample.events.oom_kill);

    let high_rising = state
        .last_high
        .is_some_and(|prev| sample.events.high > prev);
    state.last_high = Some(sample.events.high);

    let percent = sample.percent_of_max();
    let near_cap = percent.is_some_and(|p| p >= WARN_PERCENT);
    if near_cap {
        state.badged = true;
    } else if percent.is_none_or(|p| p < REARM_PERCENT) {
        state.badged = false;
    }
    if state.warned {
        // Re-arm only on a real drop: a rising `high` counter or a reading
        // still in the band between the two thresholds keeps it armed-off.
        if !high_rising && percent.is_none_or(|p| p < REARM_PERCENT) {
            state.warned = false;
        }
    } else if near_cap || high_rising {
        state.warned = true;
        out.push(Signal::Warn {
            near_cap,
            high_rising,
        });
    }
    out
}

/// `21.3G`, `24G`, `512M`: whole units print without a fraction.
pub fn format_bytes(bytes: u64) -> String {
    if bytes >= GIB {
        if bytes.is_multiple_of(GIB) {
            format!("{}G", bytes / GIB)
        } else {
            format!("{:.1}G", bytes as f64 / GIB as f64)
        }
    } else {
        format!("{}M", bytes / MIB)
    }
}

/// The warning text: `memory 21.3G/24G, swap 3.1G`.
pub fn warn_text(sample: &MemSample) -> String {
    match sample.max {
        Some(max) => format!(
            "memory {}/{}, swap {}",
            format_bytes(sample.current),
            format_bytes(max),
            format_bytes(sample.swap_current)
        ),
        None => format!(
            "memory {} (no cap), swap {}",
            format_bytes(sample.current),
            format_bytes(sample.swap_current)
        ),
    }
}

/// The OOM notice. The victim's name is not derivable without the kernel log,
/// so it is always "a process".
pub fn oom_text(cap: Option<u64>) -> String {
    match cap {
        Some(cap) => format!("OOM-killed a process at {}", format_bytes(cap)),
        None => "OOM-killed a process at the memory cap".to_string(),
    }
}

/// What the exit handler should do about a pane that just exited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartVerdict {
    /// The exit coincides with an `oom_kill` and the cooldown has elapsed.
    Restart,
    /// Not a memory exit (clean exit, or no recent `oom_kill`): leave to the
    /// ordinary exit handling.
    NotOom,
    /// A memory exit, but this pane slot was auto-restarted too recently.
    RateLimited,
}

/// Decide whether an exit is restartable. Times are monotonic milliseconds.
///
/// A clean exit (`Some(0)`) never restarts — the user typed `exit` — and an
/// exit with no `oom_kill` inside [`OOM_EXIT_WINDOW_MS`] is not ours to
/// restart. The caller must also pass `last_oom_ms = None` when the user typed
/// into the pane after the `oom_kill` (an `exit`/Ctrl-D then returns the status
/// of the last command, often 137): that exit is the user closing the pane.
pub fn restart_verdict(
    enabled: bool,
    exit_code: Option<i32>,
    last_oom_ms: Option<u64>,
    last_restart_ms: Option<u64>,
    now_ms: u64,
) -> RestartVerdict {
    let oom_recent = last_oom_ms.is_some_and(|at| now_ms.saturating_sub(at) <= OOM_EXIT_WINDOW_MS);
    if !enabled || exit_code == Some(0) || !oom_recent {
        return RestartVerdict::NotOom;
    }
    let cooled = last_restart_ms.is_none_or(|at| now_ms.saturating_sub(at) >= RESTART_COOLDOWN_MS);
    if cooled {
        RestartVerdict::Restart
    } else {
        RestartVerdict::RateLimited
    }
}

/// A recognised agent that was the pane's foreground program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSeen {
    /// The program's short name, reduced to filename-safe characters; used only
    /// in messages.
    pub name: String,
    /// The harness's own fixed, id-free continue command (`claude --continue`,
    /// `pi --continue`); `None` for a harness without one.
    pub continue_cmd: Option<String>,
}

/// Recognise a known agent harness in a foreground argv (looking past
/// `node`/`bun`/`deno`/`python` to the script they run). `None` for anything
/// else. Only the program NAME is kept: no argument ever leaves the argv.
pub fn recognise_agent(argv: &[String]) -> Option<AgentSeen> {
    let stem_of = |word: &String| {
        std::path::Path::new(word)
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
    };
    let first = stem_of(argv.first()?)?;
    let (stem, found) = match harness::harness(&first) {
        Some(found) => (first, found),
        None if matches!(
            first.as_str(),
            "node" | "bun" | "deno" | "python" | "python3"
        ) =>
        {
            let script = stem_of(argv.get(1)?)?;
            let found = harness::harness(&script)?;
            (script, found)
        }
        None => return None,
    };
    let name: String = stem
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(32)
        .collect();
    Some(AgentSeen {
        name,
        continue_cmd: harness::Harness::continue_command(found),
    })
}

/// Whether `text` may be typed into a shell unattended: non-empty and free of
/// control characters (which a line editor would act on: Ctrl-C, Ctrl-U, Tab,
/// ESC sequences, newlines) and Unicode format controls.
pub fn safe_to_type(text: &str) -> bool {
    !text.is_empty()
        && !text
            .chars()
            .any(|c| c.is_control() || crate::pipeline_report::is_format_control(c))
}

/// The line to type into a freshly restarted shell, or `None` when nothing may
/// be typed. Only a harness's own fixed continue command is ever typed — never
/// a command line reconstructed from the dead process — optionally preceded by
/// `cd` into the directory the agent ran in (`claude --continue` resumes the
/// latest session of the *current* directory).
pub fn typed_relaunch(agent: &AgentSeen, cwd: Option<&str>) -> Option<String> {
    let cont = agent.continue_cmd.as_deref().filter(|c| safe_to_type(c))?;
    if let Some(dir) = cwd.filter(|d| safe_to_type(d)) {
        let line = format!("cd -- {} && {cont}", sh_quote(dir));
        if safe_to_type(&line) {
            return Some(line);
        }
    }
    Some(cont.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(current_gib: u64, max_gib: Option<u64>) -> MemSample {
        MemSample {
            current: current_gib * GIB,
            max: max_gib.map(|g| g * GIB),
            swap_current: 0,
            events: MemEventCounts::default(),
        }
    }

    #[test]
    fn parses_events_and_limits() {
        let events = parse_events("low 0\nhigh 7\nmax 9\noom 2\noom_kill 1\n");
        assert_eq!(
            events,
            MemEventCounts {
                high: 7,
                max: 9,
                oom: 2,
                oom_kill: 1
            }
        );
        assert_eq!(parse_events("garbage\nhigh x\n"), MemEventCounts::default());
        assert_eq!(parse_limit("max\n"), None);
        assert_eq!(parse_limit("25769803776\n"), Some(24 * GIB));
        assert_eq!(parse_u64(" 42\n"), Some(42));
    }

    #[test]
    fn finds_the_pane_scope_only_under_the_thegn_slice() {
        let body = "0::/user.slice/user-1000.slice/user@1000.service/thegn.slice/run-p1-i2.scope\n";
        assert_eq!(
            pane_scope_path(body),
            Some("/user.slice/user-1000.slice/user@1000.service/thegn.slice/run-p1-i2.scope")
        );
        assert_eq!(
            pane_scope_path("0::/user.slice/user@1000.service/app.slice/x.scope\n"),
            None
        );
        assert_eq!(
            pane_scope_path("0::/user.slice/thegn.slice/thegn.service\n"),
            None
        );
        assert_eq!(pane_scope_path("1:name=systemd:/x.scope\n"), None);
    }

    #[test]
    fn warns_once_at_the_threshold_and_rearms_below_75() {
        let mut state = WatchState::default();
        assert!(step(&mut state, &sample(10, Some(24))).is_empty());
        // 21/24 = 87%: crossing raises exactly one warning.
        let signals = step(&mut state, &sample(21, Some(24)));
        assert_eq!(
            signals,
            vec![Signal::Warn {
                near_cap: true,
                high_rising: false
            }]
        );
        // Staying above the line does not repeat it.
        assert!(step(&mut state, &sample(22, Some(24))).is_empty());
        // 19/24 = 79%: between the thresholds, still armed-off.
        assert!(step(&mut state, &sample(19, Some(24))).is_empty());
        assert!(step(&mut state, &sample(21, Some(24))).is_empty());
        // 17/24 = 70% re-arms; the next crossing warns again.
        assert!(step(&mut state, &sample(17, Some(24))).is_empty());
        assert_eq!(step(&mut state, &sample(21, Some(24))).len(), 1);
    }

    #[test]
    fn a_rising_high_counter_warns_even_below_the_threshold() {
        let mut state = WatchState::default();
        let mut s = sample(5, Some(24));
        assert!(step(&mut state, &s).is_empty());
        s.events.high = 3;
        assert_eq!(
            step(&mut state, &s),
            vec![Signal::Warn {
                near_cap: false,
                high_rising: true
            }]
        );
        // Still rising: stays quiet, and does not re-arm while it rises.
        s.events.high = 9;
        assert!(step(&mut state, &s).is_empty());
        // Flat and low again: re-armed.
        assert!(step(&mut state, &s).is_empty());
        s.events.high = 10;
        assert_eq!(step(&mut state, &s).len(), 1);
    }

    #[test]
    fn an_uncapped_pane_never_warns_on_percent() {
        let mut state = WatchState::default();
        assert!(step(&mut state, &sample(90, None)).is_empty());
        assert!(step(&mut state, &sample(95, None)).is_empty());
        assert_eq!(sample(1, Some(0)).percent_of_max(), None);
    }

    #[test]
    fn oom_kill_increment_is_detected_and_first_sample_is_baseline() {
        let mut state = WatchState::default();
        let mut s = sample(1, Some(24));
        s.events.oom_kill = 4;
        // Pre-existing history is not an event.
        assert!(step(&mut state, &s).is_empty());
        assert!(step(&mut state, &s).is_empty());
        s.events.oom_kill = 6;
        assert_eq!(step(&mut state, &s), vec![Signal::OomKilled { kills: 2 }]);
        assert!(step(&mut state, &s).is_empty());
    }

    #[test]
    fn formats_numbers_for_the_notice() {
        assert_eq!(format_bytes(24 * GIB), "24G");
        assert_eq!(format_bytes(21 * GIB + 3 * GIB / 10), "21.3G");
        assert_eq!(format_bytes(512 * MIB), "512M");
        let s = MemSample {
            current: 21 * GIB + 3 * GIB / 10,
            max: Some(24 * GIB),
            swap_current: 3 * GIB + GIB / 10,
            events: MemEventCounts::default(),
        };
        assert_eq!(warn_text(&s), "memory 21.3G/24G, swap 3.1G");
        assert_eq!(oom_text(Some(24 * GIB)), "OOM-killed a process at 24G");
        assert_eq!(oom_text(None), "OOM-killed a process at the memory cap");
    }

    #[test]
    fn restart_only_when_the_exit_follows_an_oom_kill() {
        let now = 1_000_000;
        // Crash with a fresh oom_kill: restart.
        assert_eq!(
            restart_verdict(true, Some(137), Some(now - 60_000), None, now),
            RestartVerdict::Restart
        );
        // Unreaped exit status also counts.
        assert_eq!(
            restart_verdict(true, None, Some(now - 1_000), None, now),
            RestartVerdict::Restart
        );
        // A clean exit never restarts, even next to an oom_kill.
        assert_eq!(
            restart_verdict(true, Some(0), Some(now - 1_000), None, now),
            RestartVerdict::NotOom
        );
        // No oom_kill, or one outside the window: not ours.
        assert_eq!(
            restart_verdict(true, Some(137), None, None, now),
            RestartVerdict::NotOom
        );
        assert_eq!(
            restart_verdict(
                true,
                Some(137),
                Some(now - OOM_EXIT_WINDOW_MS - 1),
                None,
                now
            ),
            RestartVerdict::NotOom
        );
        // Disabled.
        assert_eq!(
            restart_verdict(false, Some(137), Some(now - 1_000), None, now),
            RestartVerdict::NotOom
        );
    }

    #[test]
    fn restart_is_rate_limited_to_one_per_ten_minutes() {
        let now = 5_000_000;
        let oom = Some(now - 1_000);
        assert_eq!(
            restart_verdict(true, Some(137), oom, Some(now - 599_000), now),
            RestartVerdict::RateLimited
        );
        assert_eq!(
            restart_verdict(true, Some(137), oom, Some(now - RESTART_COOLDOWN_MS), now),
            RestartVerdict::Restart
        );
    }

    #[test]
    fn badge_sets_at_85_and_clears_below_75() {
        let mut state = WatchState::default();
        step(&mut state, &sample(10, Some(24)));
        assert!(!state.badged());
        step(&mut state, &sample(21, Some(24)));
        assert!(state.badged());
        // 19/24 = 79%: between the thresholds, the badge stays.
        step(&mut state, &sample(19, Some(24)));
        assert!(state.badged());
        step(&mut state, &sample(17, Some(24)));
        assert!(!state.badged());
        // A rising `high` counter alone never badges.
        let mut s = sample(5, Some(24));
        step(&mut state, &s);
        s.events.high = 4;
        step(&mut state, &s);
        assert!(!state.badged());
    }

    #[test]
    fn only_the_fixed_continue_forms_are_typed() {
        let argv = |words: &[&str]| words.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        let seen = recognise_agent(&argv(&["/usr/bin/claude", "--model", "x"])).unwrap();
        assert_eq!(seen.name, "claude");
        assert_eq!(
            typed_relaunch(&seen, None).as_deref(),
            Some("claude --continue")
        );
        assert_eq!(
            typed_relaunch(&seen, Some("/home/u/my repo")).as_deref(),
            Some("cd -- '/home/u/my repo' && claude --continue")
        );
        // A directory with a control character is dropped, not typed.
        assert_eq!(
            typed_relaunch(&seen, Some("/tmp/a\u{3}b")).as_deref(),
            Some("claude --continue")
        );
        let pi = recognise_agent(&argv(&["node", "/opt/pi/bin/pi"])).unwrap();
        assert_eq!(typed_relaunch(&pi, None).as_deref(), Some("pi --continue"));
        // A harness with no continue form types nothing, and its argv is
        // never carried at all.
        let codex = recognise_agent(&argv(&["codex", "--flag", "a\u{3}rm -rf ~"])).unwrap();
        assert_eq!(codex.continue_cmd, None);
        assert_eq!(typed_relaunch(&codex, Some("/tmp")), None);
        // Not an agent.
        assert_eq!(recognise_agent(&argv(&["node", "x.js"])), None);
        assert_eq!(recognise_agent(&[]), None);
    }

    #[test]
    fn typed_text_rejects_control_and_format_characters() {
        assert!(safe_to_type("claude --continue"));
        assert!(!safe_to_type(""));
        assert!(!safe_to_type("a\nb"));
        assert!(!safe_to_type("a\u{1b}[31m"));
        assert!(!safe_to_type("a\u{202e}b"));
    }
}
