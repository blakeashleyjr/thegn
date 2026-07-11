//! Pure notification routing (items 420/426/427/429).
//!
//! One function — [`decide`] — maps a notification (its kind, source, message,
//! worktree) plus the ambient context (the local clock, the manual DND toggle,
//! the active routing mode + profile) to a [`RouteDecision`]: which channels
//! fire (inbox record, desktop toast, in-app toast, sound) and the effective
//! priority. It composes three layers on top of the base per-kind priority:
//! user rules (`[[notifications.rules]]`), do-not-disturb (`[notifications.dnd]`),
//! and the sound gate (`[notifications.sound]`).
//!
//! Everything here is pure and substrate-free (no clock reads, no I/O) so it is
//! exhaustively unit-tested against the core coverage gate; the host supplies
//! the clock and applies the decision at its dispatch chokepoint.

use chrono::{Datelike, NaiveDateTime, Timelike, Weekday};

use crate::config::{DndConfig, NotificationRule, NotificationsConfig, SoundConfig, SoundMode};
use crate::notification::{NotificationKind, Priority};

/// The audible cue a decision resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SoundEmit {
    /// Write a terminal `BEL` (`\x07`) on the next render flush.
    Bell,
    /// Run this command line off-thread (best-effort).
    Command(String),
}

/// What should happen to one notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteDecision {
    /// Persist to the inbox DB.
    pub record: bool,
    /// Priority after per-kind override + rules (drives flag/count + toast urgency).
    pub effective_priority: Priority,
    /// Eligible for an OS desktop toast (the `desktop_min_urgency` floor still
    /// applies downstream).
    pub desktop: bool,
    /// Eligible for an in-app toast overlay.
    pub toast: bool,
    /// The audible cue, if any.
    pub sound: Option<SoundEmit>,
}

/// Ambient context for a routing decision (supplied by the host).
#[derive(Debug, Clone, Default)]
pub struct RouteCtx {
    /// Local wall-clock time, for DND window evaluation.
    pub now_local: Option<NaiveDateTime>,
    /// Manual DND toggle: `Some(true)`/`Some(false)` overrides the schedule,
    /// `None` defers to the configured windows.
    pub dnd_forced: Option<bool>,
    /// The active routing mode (`""` = the default/no-mode set of rules).
    pub active_mode: String,
    /// The active profile name (`""` = none), matched by a rule's `profile`.
    pub active_profile: String,
}

/// Decide how to route one notification. `cfg` is the *effective* notification
/// config (global + profile + repo overlay already merged).
pub fn decide(
    kind: NotificationKind,
    source_ref: &str,
    message: &str,
    worktree: &str,
    cfg: &NotificationsConfig,
    ctx: &RouteCtx,
) -> RouteDecision {
    let mut effective = cfg.priority_of(kind);
    // Channel state. Defaults mirror the pre-routing behavior: everything lands
    // in the inbox and is desktop-eligible (the urgency floor still gates
    // downstream); in-app toasts are opt-in via a rule `route`.
    let mut record = true;
    let mut desktop = true;
    let mut toast = false;
    let mut sound_allowed = true;
    // A rule sound override: outer Some = overridden, inner None = explicitly off.
    let mut rule_sound: Option<Option<SoundEmit>> = None;

    for rule in &cfg.rules {
        if !rule_matches(rule, kind, source_ref, message, worktree, effective, ctx) {
            continue;
        }
        if rule.drop {
            return RouteDecision {
                record: false,
                effective_priority: effective,
                desktop: false,
                toast: false,
                sound: None,
            };
        }
        if let Some(p) = rule.set_priority.as_deref().and_then(Priority::parse) {
            effective = p;
        }
        if let Some(chans) = &rule.route {
            record = chan_has(chans, "inbox");
            desktop = chan_has(chans, "desktop");
            toast = chan_has(chans, "toast");
            sound_allowed = chan_has(chans, "sound");
        }
        if rule.mute {
            desktop = false;
            toast = false;
            sound_allowed = false;
        }
        if let Some(s) = rule.sound.as_deref() {
            rule_sound = Some(parse_sound_override(s));
        }
        if rule.stop {
            break;
        }
    }

    // Do-not-disturb: suppress ephemeral channels below the allow threshold.
    let dnd_active = ctx
        .dnd_forced
        .unwrap_or_else(|| scheduled_dnd_active(&cfg.dnd, ctx.now_local));
    if dnd_active {
        let allow = Priority::parse(&cfg.dnd.allow_priority).unwrap_or(Priority::Alert);
        if effective.rank() < allow.rank() {
            desktop = false;
            toast = false;
            sound_allowed = false;
        }
    }

    let sound = if !sound_allowed {
        None
    } else if let Some(over) = rule_sound {
        over
    } else {
        sound_emit(&cfg.sound, effective)
    };

    RouteDecision {
        record,
        effective_priority: effective,
        desktop,
        toast,
        sound,
    }
}

/// Whether a channel token is present in a rule's `route` list (case-insensitive).
fn chan_has(chans: &[String], want: &str) -> bool {
    chans.iter().any(|c| c.trim().eq_ignore_ascii_case(want))
}

/// Parse a rule's `sound` override: `off`/`none` ⇒ silence, `bell` ⇒ bell,
/// anything else ⇒ a command line.
fn parse_sound_override(s: &str) -> Option<SoundEmit> {
    match s.trim().to_ascii_lowercase().as_str() {
        "off" | "none" | "silent" | "" => None,
        "bell" | "beep" | "terminal" => Some(SoundEmit::Bell),
        _ => Some(SoundEmit::Command(s.trim().to_string())),
    }
}

/// The sound a [`SoundConfig`] produces for a given effective priority, before
/// any rule override or DND/channel suppression.
pub fn sound_emit(cfg: &SoundConfig, priority: Priority) -> Option<SoundEmit> {
    if cfg.mode == SoundMode::Off {
        return None;
    }
    let min = Priority::parse(&cfg.min_priority).unwrap_or(Priority::Alert);
    if priority.rank() < min.rank() {
        return None;
    }
    match cfg.mode {
        SoundMode::Off => None,
        SoundMode::Bell => Some(SoundEmit::Bell),
        SoundMode::Command => {
            let cmd = cfg
                .per_priority
                .get(priority_key(priority))
                .cloned()
                .unwrap_or_else(|| cfg.command.clone());
            let cmd = cmd.trim();
            if cmd.is_empty() {
                None
            } else {
                Some(SoundEmit::Command(cmd.to_string()))
            }
        }
    }
}

fn priority_key(p: Priority) -> &'static str {
    match p {
        Priority::Info => "info",
        Priority::Notice => "notice",
        Priority::Alert => "alert",
    }
}

/// Whether a rule's selectors all match. Absent selectors are wildcards.
fn rule_matches(
    rule: &NotificationRule,
    kind: NotificationKind,
    source_ref: &str,
    message: &str,
    worktree: &str,
    base_priority: Priority,
    ctx: &RouteCtx,
) -> bool {
    // kind / kinds (union). Present ⇒ must be in the set.
    if rule.kind.is_some() || !rule.kinds.is_empty() {
        let k = kind.as_str();
        let single = rule.kind.as_deref() == Some(k);
        let listed = rule.kinds.iter().any(|s| s == k);
        if !single && !listed {
            return false;
        }
    }
    if let Some(g) = &rule.worktree
        && !glob_match(g, worktree)
    {
        return false;
    }
    if let Some(pfx) = &rule.source
        && !source_ref.starts_with(pfx.as_str())
    {
        return false;
    }
    if let Some(re) = &rule.message {
        match regex::Regex::new(re) {
            Ok(r) if r.is_match(message) => {}
            _ => return false, // no match, or an invalid pattern ⇒ rule inert
        }
    }
    if let Some(min) = rule.min_priority.as_deref().and_then(Priority::parse)
        && base_priority.rank() < min.rank()
    {
        return false;
    }
    if !rule.modes.is_empty() && !rule.modes.iter().any(|m| m == &ctx.active_mode) {
        return false;
    }
    if let Some(p) = &rule.profile
        && p != &ctx.active_profile
    {
        return false;
    }
    true
}

/// Minimal glob: `*` matches any run (incl. empty), `?` any single char, other
/// chars literal. Enough for worktree-path matching (`*/app`, `/home/*/repo`).
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    glob_rec(&p, &t)
}

fn glob_rec(p: &[char], t: &[char]) -> bool {
    match p.first() {
        None => t.is_empty(),
        Some('*') => {
            // Match zero chars here, or consume one text char and retry `*`.
            glob_rec(&p[1..], t) || (!t.is_empty() && glob_rec(p, &t[1..]))
        }
        Some('?') => !t.is_empty() && glob_rec(&p[1..], &t[1..]),
        Some(c) => t.first() == Some(c) && glob_rec(&p[1..], &t[1..]),
    }
}

// ---------------------------------------------------------------------------
// DND schedule
// ---------------------------------------------------------------------------

/// A parsed quiet window: an optional weekday set plus a minutes-of-day range.
/// `end <= start` means the range wraps past midnight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    /// Empty ⇒ every day.
    pub days: Vec<Weekday>,
    /// Start minute-of-day (0..=1439).
    pub start: u32,
    /// End minute-of-day (0..=1440).
    pub end: u32,
}

impl Window {
    fn wraps(&self) -> bool {
        self.end <= self.start
    }
}

/// Whether the configured DND schedule is active at `now`. No windows or no
/// clock ⇒ inactive.
pub fn scheduled_dnd_active(dnd: &DndConfig, now: Option<NaiveDateTime>) -> bool {
    let Some(now) = now else { return false };
    dnd.windows
        .iter()
        .filter_map(|s| parse_window(s))
        .any(|w| window_active(&w, now))
}

/// Whether `now` falls inside `w`, honoring wrap-past-midnight + weekday sets.
pub fn window_active(w: &Window, now: NaiveDateTime) -> bool {
    let minutes = now.hour() * 60 + now.minute();
    let today = now.weekday();
    let day_ok = |d: Weekday| w.days.is_empty() || w.days.contains(&d);
    if w.wraps() {
        // Post-start portion belongs to `today`; the pre-end (early morning)
        // portion belongs to the window that started *yesterday*.
        (minutes >= w.start && day_ok(today)) || (minutes < w.end && day_ok(today.pred()))
    } else {
        minutes >= w.start && minutes < w.end && day_ok(today)
    }
}

/// Parse a window string: `"HH:MM-HH:MM"` with an optional leading weekday token
/// (`"Sat 22:00-23:00"`, `"mon-fri 09:00-17:00"`, `"sat,sun 00:00-08:00"`).
pub fn parse_window(s: &str) -> Option<Window> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (days, range) = match s.split_once(char::is_whitespace) {
        Some((d, r)) => (parse_days(d.trim())?, r.trim()),
        None => (Vec::new(), s),
    };
    let (a, b) = range.split_once('-')?;
    let start = parse_hhmm(a.trim())?;
    let end = parse_hhmm(b.trim())?;
    Some(Window { days, start, end })
}

fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.split_once(':')?;
    let h: u32 = h.trim().parse().ok()?;
    let m: u32 = m.trim().parse().ok()?;
    if h > 24 || m > 59 {
        return None;
    }
    Some((h * 60 + m).min(1440))
}

/// Parse a weekday token: a single day, a `mon-fri` range, or a `sat,sun` list.
fn parse_days(s: &str) -> Option<Vec<Weekday>> {
    if let Some((a, b)) = s.split_once('-') {
        let start = parse_weekday(a.trim())?;
        let end = parse_weekday(b.trim())?;
        let mut days = Vec::new();
        let mut d = start;
        // Walk forward (inclusive) from start to end, wrapping the week.
        for _ in 0..7 {
            days.push(d);
            if d == end {
                return Some(days);
            }
            d = d.succ();
        }
        return Some(days);
    }
    let mut days = Vec::new();
    for tok in s.split(',') {
        days.push(parse_weekday(tok.trim())?);
    }
    Some(days)
}

fn parse_weekday(s: &str) -> Option<Weekday> {
    match s.trim().to_ascii_lowercase().as_str() {
        "mon" | "monday" => Some(Weekday::Mon),
        "tue" | "tues" | "tuesday" => Some(Weekday::Tue),
        "wed" | "weds" | "wednesday" => Some(Weekday::Wed),
        "thu" | "thur" | "thurs" | "thursday" => Some(Weekday::Thu),
        "fri" | "friday" => Some(Weekday::Fri),
        "sat" | "saturday" => Some(Weekday::Sat),
        "sun" | "sunday" => Some(Weekday::Sun),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DndConfig, NotificationRule, NotificationsConfig, SoundConfig, SoundMode};
    use chrono::NaiveDate;

    fn dt(y: i32, m: u32, d: u32, hh: u32, mm: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(hh, mm, 0)
            .unwrap()
    }

    fn base_cfg() -> NotificationsConfig {
        NotificationsConfig::default()
    }

    fn ctx() -> RouteCtx {
        RouteCtx::default()
    }

    // --- glob ---

    #[test]
    fn glob_matches() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*/app", "/home/x/app"));
        assert!(glob_match("/home/*/repo", "/home/dave/repo"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(!glob_match("*/app", "/home/x/apps"));
        assert!(glob_match("*app*", "/x/app/y"));
    }

    // --- base pass-through ---

    #[test]
    fn default_decision_records_and_desktops() {
        let d = decide(
            NotificationKind::TestFailed,
            "wt",
            "2 failed",
            "/wt/app",
            &base_cfg(),
            &ctx(),
        );
        assert!(d.record);
        assert!(d.desktop);
        assert!(!d.toast); // toast opt-in
        assert_eq!(d.effective_priority, Priority::Alert);
        assert_eq!(d.sound, Some(SoundEmit::Bell)); // alert >= default min_priority
    }

    #[test]
    fn notice_below_sound_threshold_is_silent() {
        let d = decide(
            NotificationKind::AgentDone,
            "wt",
            "done",
            "/wt/app",
            &base_cfg(),
            &ctx(),
        );
        assert_eq!(d.effective_priority, Priority::Notice);
        assert_eq!(d.sound, None);
    }

    // --- rules ---

    #[test]
    fn rule_mute_by_worktree() {
        let mut cfg = base_cfg();
        cfg.rules.push(NotificationRule {
            worktree: Some("*/noisy".into()),
            mute: true,
            ..Default::default()
        });
        let d = decide(
            NotificationKind::TestFailed,
            "wt",
            "boom",
            "/wt/noisy",
            &cfg,
            &ctx(),
        );
        assert!(d.record, "muted still records to inbox");
        assert!(!d.desktop);
        assert!(!d.toast);
        assert_eq!(d.sound, None);
        // A different worktree is unaffected.
        let d2 = decide(
            NotificationKind::TestFailed,
            "wt",
            "boom",
            "/wt/quiet",
            &cfg,
            &ctx(),
        );
        assert!(d2.desktop);
    }

    #[test]
    fn rule_message_regex_drop() {
        let mut cfg = base_cfg();
        cfg.rules.push(NotificationRule {
            message: Some(r"flaky|retry".into()),
            drop: true,
            ..Default::default()
        });
        let d = decide(
            NotificationKind::TestFailed,
            "wt",
            "flaky test retried",
            "/wt/app",
            &cfg,
            &ctx(),
        );
        assert!(!d.record);
        assert!(!d.desktop);
        assert_eq!(d.sound, None);
    }

    #[test]
    fn invalid_regex_makes_rule_inert() {
        let mut cfg = base_cfg();
        cfg.rules.push(NotificationRule {
            message: Some("(unclosed".into()),
            drop: true,
            ..Default::default()
        });
        // Rule cannot match, so the notification survives.
        let d = decide(
            NotificationKind::TestFailed,
            "wt",
            "(unclosed",
            "/wt/app",
            &cfg,
            &ctx(),
        );
        assert!(d.record);
    }

    #[test]
    fn rule_promotes_priority() {
        let mut cfg = base_cfg();
        cfg.rules.push(NotificationRule {
            kind: Some("agent_done".into()),
            set_priority: Some("alert".into()),
            ..Default::default()
        });
        let d = decide(
            NotificationKind::AgentDone,
            "wt",
            "done",
            "/wt/app",
            &cfg,
            &ctx(),
        );
        assert_eq!(d.effective_priority, Priority::Alert);
        assert_eq!(d.sound, Some(SoundEmit::Bell));
    }

    #[test]
    fn rule_route_restricts_channels() {
        let mut cfg = base_cfg();
        cfg.rules.push(NotificationRule {
            kind: Some("test_failed".into()),
            route: Some(vec!["inbox".into(), "toast".into()]),
            ..Default::default()
        });
        let d = decide(
            NotificationKind::TestFailed,
            "wt",
            "x",
            "/wt/app",
            &cfg,
            &ctx(),
        );
        assert!(d.record);
        assert!(d.toast);
        assert!(!d.desktop);
        assert_eq!(d.sound, None, "sound not in route");
    }

    #[test]
    fn rule_stop_halts_evaluation() {
        let mut cfg = base_cfg();
        cfg.rules.push(NotificationRule {
            kind: Some("test_failed".into()),
            set_priority: Some("info".into()),
            stop: true,
            ..Default::default()
        });
        cfg.rules.push(NotificationRule {
            kind: Some("test_failed".into()),
            set_priority: Some("alert".into()),
            ..Default::default()
        });
        let d = decide(
            NotificationKind::TestFailed,
            "wt",
            "x",
            "/wt/app",
            &cfg,
            &ctx(),
        );
        assert_eq!(d.effective_priority, Priority::Info);
    }

    #[test]
    fn rule_mode_selector_gates() {
        let mut cfg = base_cfg();
        cfg.rules.push(NotificationRule {
            modes: vec!["focus".into()],
            mute: true,
            ..Default::default()
        });
        // Inactive mode ⇒ rule skipped.
        let d = decide(
            NotificationKind::TestFailed,
            "wt",
            "x",
            "/wt/app",
            &cfg,
            &ctx(),
        );
        assert!(d.desktop);
        // Active mode ⇒ rule applies.
        let c = RouteCtx {
            active_mode: "focus".into(),
            ..Default::default()
        };
        let d2 = decide(NotificationKind::TestFailed, "wt", "x", "/wt/app", &cfg, &c);
        assert!(!d2.desktop);
    }

    #[test]
    fn rule_profile_selector_gates() {
        let mut cfg = base_cfg();
        cfg.rules.push(NotificationRule {
            profile: Some("work".into()),
            mute: true,
            ..Default::default()
        });
        let personal = RouteCtx {
            active_profile: "personal".into(),
            ..Default::default()
        };
        assert!(
            decide(
                NotificationKind::TestFailed,
                "wt",
                "x",
                "/wt/app",
                &cfg,
                &personal
            )
            .desktop
        );
        let work = RouteCtx {
            active_profile: "work".into(),
            ..Default::default()
        };
        assert!(
            !decide(
                NotificationKind::TestFailed,
                "wt",
                "x",
                "/wt/app",
                &cfg,
                &work
            )
            .desktop
        );
    }

    #[test]
    fn rule_min_priority_selector() {
        let mut cfg = base_cfg();
        cfg.rules.push(NotificationRule {
            min_priority: Some("alert".into()),
            mute: true,
            ..Default::default()
        });
        // Notice base ⇒ below threshold ⇒ rule skipped.
        assert!(
            decide(
                NotificationKind::AgentDone,
                "wt",
                "x",
                "/wt/app",
                &cfg,
                &ctx()
            )
            .desktop
        );
        // Alert base ⇒ matches.
        assert!(
            !decide(
                NotificationKind::TestFailed,
                "wt",
                "x",
                "/wt/app",
                &cfg,
                &ctx()
            )
            .desktop
        );
    }

    // --- DND ---

    #[test]
    fn dnd_forced_suppresses_below_allow() {
        let cfg = base_cfg(); // allow_priority defaults to "alert"
        let c = RouteCtx {
            dnd_forced: Some(true),
            ..Default::default()
        };
        // Notice < alert ⇒ suppressed.
        let d = decide(NotificationKind::AgentDone, "wt", "x", "/wt/app", &cfg, &c);
        assert!(d.record);
        assert!(!d.desktop);
        assert_eq!(d.sound, None);
        // Alert breaks through.
        let d2 = decide(NotificationKind::TestFailed, "wt", "x", "/wt/app", &cfg, &c);
        assert!(d2.desktop);
        assert_eq!(d2.sound, Some(SoundEmit::Bell));
    }

    #[test]
    fn dnd_forced_off_overrides_schedule() {
        let mut cfg = base_cfg();
        cfg.dnd = DndConfig {
            enabled: true,
            windows: vec!["00:00-23:59".into()],
            allow_priority: "alert".into(),
        };
        let c = RouteCtx {
            dnd_forced: Some(false),
            now_local: Some(dt(2026, 6, 30, 12, 0)),
            ..Default::default()
        };
        // Schedule says DND, but the manual toggle is forced off.
        assert!(decide(NotificationKind::AgentDone, "wt", "x", "/wt/app", &cfg, &c).desktop);
    }

    #[test]
    fn scheduled_dnd_wraps_midnight_with_weekday() {
        // A Friday-night window that spills into Saturday morning. The pre-end
        // (early-morning) portion is attributed to the previous day's window.
        let dnd = DndConfig {
            enabled: false,
            windows: vec!["fri 22:00-06:00".into()],
            allow_priority: "alert".into(),
        };
        // 2026-06-26 is a Friday; 2026-06-27 is a Saturday.
        assert!(scheduled_dnd_active(&dnd, Some(dt(2026, 6, 26, 23, 0)))); // Fri late
        assert!(scheduled_dnd_active(&dnd, Some(dt(2026, 6, 27, 5, 0)))); // Sat early ⇒ Fri window
        assert!(!scheduled_dnd_active(&dnd, Some(dt(2026, 6, 27, 23, 0)))); // Sat late ⇒ no window
    }

    #[test]
    fn sound_info_priority_command_arm() {
        // Exercises the Info branch of the per-priority key lookup.
        let sc = SoundConfig {
            mode: SoundMode::Command,
            min_priority: "info".into(),
            command: "default.sh".into(),
            ..Default::default()
        };
        assert_eq!(
            sound_emit(&sc, Priority::Info),
            Some(SoundEmit::Command("default.sh".into()))
        );
    }

    #[test]
    fn scheduled_dnd_wraps_midnight() {
        let dnd = DndConfig {
            enabled: false,
            windows: vec!["22:00-08:00".into()],
            allow_priority: "alert".into(),
        };
        assert!(scheduled_dnd_active(&dnd, Some(dt(2026, 6, 30, 23, 30))));
        assert!(scheduled_dnd_active(&dnd, Some(dt(2026, 6, 30, 6, 0))));
        assert!(!scheduled_dnd_active(&dnd, Some(dt(2026, 6, 30, 12, 0))));
        assert!(!scheduled_dnd_active(&dnd, None));
    }

    #[test]
    fn scheduled_dnd_plain_window() {
        let dnd = DndConfig {
            enabled: false,
            windows: vec!["09:00-17:00".into()],
            allow_priority: "alert".into(),
        };
        assert!(scheduled_dnd_active(&dnd, Some(dt(2026, 6, 30, 12, 0))));
        assert!(!scheduled_dnd_active(&dnd, Some(dt(2026, 6, 30, 8, 0))));
        assert!(!scheduled_dnd_active(&dnd, Some(dt(2026, 6, 30, 17, 0))));
    }

    // --- window / weekday parsing ---

    #[test]
    fn parse_window_forms() {
        assert_eq!(
            parse_window("22:00-08:00"),
            Some(Window {
                days: vec![],
                start: 1320,
                end: 480
            })
        );
        let w = parse_window("mon-fri 09:00-17:00").unwrap();
        assert_eq!(w.days.len(), 5);
        assert_eq!(w.start, 540);
        let sat = parse_window("sat,sun 00:00-08:00").unwrap();
        assert_eq!(sat.days, vec![Weekday::Sat, Weekday::Sun]);
        assert!(parse_window("garbage").is_none());
        assert!(parse_window("99:99-00:00").is_none());
    }

    #[test]
    fn weekday_window_only_active_on_listed_days() {
        // 2026-06-29 is a Monday; 2026-06-27 is a Saturday.
        let dnd = DndConfig {
            enabled: false,
            windows: vec!["sat,sun 00:00-23:59".into()],
            allow_priority: "alert".into(),
        };
        assert!(scheduled_dnd_active(&dnd, Some(dt(2026, 6, 27, 12, 0)))); // Sat
        assert!(!scheduled_dnd_active(&dnd, Some(dt(2026, 6, 29, 12, 0)))); // Mon
    }

    // --- sound config ---

    #[test]
    fn sound_command_mode_uses_per_priority_then_default() {
        let mut sc = SoundConfig {
            mode: SoundMode::Command,
            min_priority: "notice".into(),
            command: "default.sh".into(),
            ..Default::default()
        };
        sc.per_priority.insert("alert".into(), "alert.sh".into());
        assert_eq!(
            sound_emit(&sc, Priority::Alert),
            Some(SoundEmit::Command("alert.sh".into()))
        );
        assert_eq!(
            sound_emit(&sc, Priority::Notice),
            Some(SoundEmit::Command("default.sh".into()))
        );
        assert_eq!(sound_emit(&sc, Priority::Info), None); // below min
    }

    #[test]
    fn sound_off_mode_is_silent() {
        let sc = SoundConfig {
            mode: SoundMode::Off,
            ..Default::default()
        };
        assert_eq!(sound_emit(&sc, Priority::Alert), None);
    }

    #[test]
    fn sound_command_empty_is_none() {
        let sc = SoundConfig {
            mode: SoundMode::Command,
            min_priority: "alert".into(),
            command: String::new(),
            ..Default::default()
        };
        assert_eq!(sound_emit(&sc, Priority::Alert), None);
    }

    #[test]
    fn rule_sound_override_off_and_command() {
        let mut cfg = base_cfg();
        cfg.rules.push(NotificationRule {
            kind: Some("test_failed".into()),
            sound: Some("off".into()),
            ..Default::default()
        });
        assert_eq!(
            decide(
                NotificationKind::TestFailed,
                "wt",
                "x",
                "/wt/app",
                &cfg,
                &ctx()
            )
            .sound,
            None
        );
        let mut cfg2 = base_cfg();
        cfg2.rules.push(NotificationRule {
            kind: Some("agent_done".into()),
            sound: Some("ping.sh".into()),
            ..Default::default()
        });
        assert_eq!(
            decide(
                NotificationKind::AgentDone,
                "wt",
                "x",
                "/wt/app",
                &cfg2,
                &ctx()
            )
            .sound,
            Some(SoundEmit::Command("ping.sh".into()))
        );
    }
}
