//! Reload classes: what a config edit does to a running thegn.
//!
//! The config watcher hands the UI loop a fresh [`Config`] on every save, and
//! the loop applies a hand-picked set of swaps. A key no swap reads used to
//! wait silently for a restart. This module makes that explicit:
//!
//! - [`RELOAD_CLASSES`] classifies every config key as [`ReloadClass::Live`]
//!   (applied by the reload, naming the [`ReloadHook`] that applies it),
//!   [`ReloadClass::Restart`] (read once at launch) or
//!   [`ReloadClass::NextUse`] (read afresh by each operation, so the next
//!   worktree/launch/queue run sees it without any reload work).
//! - [`changed_keys`] diffs two configs into dotted leaf paths, and [`plan`]
//!   buckets them so the loop runs only the hooks whose keys changed and the
//!   status line can say what applied and what needs a restart.
//!
//! The `config_reload_classes` integration test walks the `Config` schema and
//! fails when a key has no row (and when a row matches no key), so the table
//! cannot drift from the structs.
//!
//! Pure: no I/O, no clock.

use super::Config;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// How a changed key reaches a running instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReloadClass {
    /// Applied by the config reload itself (see the row's [`ReloadHook`]).
    Live,
    /// Read once at launch (or by the daemon): takes effect after a restart.
    Restart,
    /// Read afresh by each operation that needs it (worktree creation, a
    /// launch, a queue run): the next use sees the new value.
    NextUse,
}

impl ReloadClass {
    /// One-line, user-facing description (used by `thegn config set`).
    pub fn describe(self) -> &'static str {
        match self {
            Self::Live => "applies live in a running thegn",
            Self::Restart => "takes effect after a restart",
            Self::NextUse => "used the next time it is needed",
        }
    }
}

/// Which reload swap group applies a live key. One id per swap group in the
/// UI loop's reload handler; [`ReloadHook::None`] marks keys applied by the
/// swaps that run on every reload (the `current_config` swap and the cheap
/// unconditional stores such as the scrollback cap and the resident-pool
/// limit). The `config_reload_classes` gate keeps `Live` + `None` rows to an
/// explicit allowlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReloadHook {
    Weather,
    Sidebar,
    Theme,
    Bars,
    Calendar,
    Keymap,
    Panel,
    Help,
    Voice,
    Lsp,
    Preview,
    Automation,
    AppHost,
    Layout,
    Caps,
    /// The shared refresh ticker's cadences (`ScheduleConfig::from_config`).
    Schedule,
    /// Notification routing + push workers (`install_push_workers`).
    Notifications,
    /// The plugin host restart.
    Plugins,
    /// The media watcher restart + album-art toggle.
    Media,
    /// The `[metrics]` supervisor re-arm (a fresh slot + supervisor; the old
    /// supervisor exits on `consumer_gone`).
    Metrics,
    /// The monitor's per-tab defaults and window ladder
    /// (`MonitorPrefs::reapply_config`); applies to the next monitor open.
    Monitor,
    None,
}

impl ReloadHook {
    /// Short label for the reload status line.
    pub fn label(self) -> &'static str {
        match self {
            Self::Weather => "weather",
            Self::Sidebar => "sidebar",
            Self::Theme => "theme",
            Self::Bars => "bars",
            Self::Calendar => "calendar",
            Self::Keymap => "keys",
            Self::Panel => "panel",
            Self::Help => "help",
            Self::Voice => "voice",
            Self::Lsp => "lsp",
            Self::Preview => "preview",
            Self::Automation => "automations",
            Self::AppHost => "apps",
            Self::Layout => "layout",
            Self::Caps => "terminal caps",
            Self::Schedule => "refresh cadences",
            Self::Notifications => "notifications",
            Self::Plugins => "plugins",
            Self::Media => "media",
            Self::Metrics => "metrics",
            Self::Monitor => "monitor",
            Self::None => "settings",
        }
    }
}

use ReloadClass::{Live, NextUse, Restart};
use ReloadHook as H;

/// The classification table: dotted key patterns, most specific match wins.
///
/// Pattern grammar (segments split on `.`):
/// - a literal segment matches itself; `*` matches any one segment (a
///   map-keyed table such as `env.<name>`); `prefix*` matches a segment that
///   starts with `prefix` (`ui.sidebar_*`);
/// - every pattern also covers everything beneath it, and a trailing `.*`
///   covers the section itself, so `env.*` matches `env`, `env.dev` and
///   `env.dev.sandbox.backend` (a changed array or a whole new map entry is
///   one key).
///
/// Precedence: an exact (wildcard-free, same-length) match wins, else the row
/// with the most literal characters.
///
/// Policy: when unsure whether a running instance re-reads a key, the row says
/// `Restart` — honest feedback beats a "live" claim that does not hold.
pub const RELOAD_CLASSES: &[(&str, ReloadClass, ReloadHook)] = &[
    // --- root scalars ---
    ("worktrees_dir", NextUse, H::None),
    ("projects_dir", NextUse, H::None),
    ("base_branch", NextUse, H::None),
    // Window geometry is fixed at launch (unsure it is re-read: Restart).
    ("window_margin", Restart, H::None),
    ("branch_prefix", NextUse, H::None),
    ("default_folder", NextUse, H::None),
    ("picker", NextUse, H::None),
    ("worktree_mode", NextUse, H::None),
    ("name_scheme", NextUse, H::None),
    ("auto_remove_worktree", NextUse, H::None),
    ("confirm_delete", NextUse, H::None),
    ("repo_roots", NextUse, H::None),
    ("repo_scan_depth", NextUse, H::None),
    // The selected profile shapes the whole effective config at launch.
    ("profile", Restart, H::None),
    // The keymap is rebuilt from the new config on every reload.
    ("keymap_preset", Live, H::Keymap),
    // --- arrays of tables ---
    ("agents", NextUse, H::None),
    ("tools", NextUse, H::None),
    ("accounts", NextUse, H::None),
    // The pin strip is seeded at launch (unsure: Restart).
    ("pins", Restart, H::None),
    ("update.*", Restart, H::None),
    ("tasks", NextUse, H::None),
    ("worktree_templates", NextUse, H::None),
    ("presets", NextUse, H::None),
    ("actions", Live, H::Keymap),
    ("git_commands", Live, H::Keymap),
    // A changed plugin list tears the plugin set down and starts it from the
    // new specs (the plugin host restart in run.rs's reload handler).
    ("plugins", Live, H::Plugins),
    ("forges", NextUse, H::None),
    // --- [ui] ---
    // Unlisted `[ui]` keys default to Restart (unsure they are re-read).
    ("ui.*", Restart, H::None),
    ("ui.sidebar_*", Live, H::Sidebar),
    ("ui.fullscreen_*", Live, H::Layout),
    ("ui.confirm_delete_project", NextUse, H::None),
    ("ui.confirm_quit", NextUse, H::None),
    ("ui.dismiss_overlay_on_click_outside", NextUse, H::None),
    // --- sub-tables ---
    ("git.*", NextUse, H::None),
    // The auto-fetch cadence re-arms through `ScheduleConfig::from_config`
    // (`auto_fetch_every_slots`) on reload.
    ("git.auto_fetch", Live, H::Schedule),
    ("git.auto_fetch_interval_secs", Live, H::Schedule),
    ("editor.*", NextUse, H::None),
    ("theme.*", Live, H::Theme),
    ("monitor.*", Restart, H::None),
    // The ladder and per-tab defaults are re-applied to the loop's
    // `MonitorPrefs` (the next monitor open sees them; user-changed tabs keep
    // their choice).
    ("monitor.window_ladder", Live, H::Monitor),
    ("monitor.default_window", Live, H::Monitor),
    ("monitor.default_style", Live, H::Monitor),
    ("monitor.default_scale", Live, H::Monitor),
    // Read from `current_config` on every tick (`wants_process_scan`).
    ("monitor.processes", Live, H::None),
    ("stats.*", Live, H::Bars),
    // The Metrics hook re-arms the supervisor from the reloaded section.
    ("metrics.*", Live, H::Metrics),
    ("apps.*", Live, H::AppHost),
    ("observe.*", Restart, H::None),
    ("bars.*", Live, H::Bars),
    // The clock tick period follows the formats (`clock_period_secs`: 1s when
    // a format shows seconds); the Schedule hook also refreshes the bar
    // configs, so the new format repaints.
    ("bars.clock_format", Live, H::Schedule),
    ("bars.date_format", Live, H::Schedule),
    ("calendar.*", Live, H::Calendar),
    ("weather.*", Live, H::Weather),
    // The daemon arms the inbox producer at install (THE-2214).
    ("inbox.*", Restart, H::None),
    ("pr.*", NextUse, H::None),
    ("issues.*", NextUse, H::None),
    ("ci.*", NextUse, H::None),
    // `ci_every_slots` in `ScheduleConfig::from_config`.
    ("ci.poll_interval_secs", Live, H::Schedule),
    ("watch.*", Restart, H::None),
    // `pr_every_slots` in `ScheduleConfig::from_config` (the PR refresh and
    // main-checkout heal cadence); the rest of `[watch]` stays Restart.
    ("watch.pr_interval_secs", Live, H::Schedule),
    ("log.*", Restart, H::None),
    ("diagnostics.*", Restart, H::None),
    ("database.*", Restart, H::None),
    // Sandbox policy (including `sandbox.git.*`) is resolved per launch.
    ("sandbox.*", NextUse, H::None),
    ("hooks.*", NextUse, H::None),
    ("toolchain.*", NextUse, H::None),
    ("limits.*", Restart, H::None),
    ("disk.*", Restart, H::None),
    // `disk_every_slots` in `ScheduleConfig::from_config`.
    ("disk.scan_interval_secs", Live, H::Schedule),
    ("loc.*", NextUse, H::None),
    // `loc_every_slots` in `ScheduleConfig::from_config`.
    ("loc.enabled", Live, H::Schedule),
    ("loc.scan_interval_secs", Live, H::Schedule),
    ("session.*", Restart, H::None),
    ("session.scrollback_lines", Live, H::None),
    ("session.resident_pool_limit", Live, H::None),
    ("activity.*", Restart, H::None),
    ("drawer.*", Restart, H::None),
    // Notification routing and push workers are republished by the
    // Notifications hook (`install_push_workers`).
    ("notifications.*", Live, H::Notifications),
    ("automations.*", Live, H::Automation),
    ("strip.*", Restart, H::None),
    ("panel.*", Live, H::Panel),
    ("search.*", NextUse, H::None),
    // The palette's sources are wired at launch (unsure: Restart).
    ("palette.*", Restart, H::None),
    ("lsp.*", Live, H::Lsp),
    ("semantic.*", Restart, H::None),
    ("daemon.*", Restart, H::None),
    ("serve.*", Restart, H::None),
    ("merge_queue.*", NextUse, H::None),
    ("pr_queue.*", NextUse, H::None),
    // `prq_every_slots` in `ScheduleConfig::from_config` (the enabled flag also
    // gates the panel's PR-queue section, which the panel hook re-resolves).
    ("pr_queue.enabled", Live, H::Schedule),
    ("pr_queue.poll_interval_secs", Live, H::Schedule),
    ("autopilot.*", Restart, H::None),
    // Read when `thegn job run` starts and when the daemon reconciles at start.
    ("jobs.*", NextUse, H::None),
    ("pipeline.*", NextUse, H::None),
    ("skills.*", NextUse, H::None),
    // Newly spawned panes pick up the reloaded replay settings.
    ("replay.*", NextUse, H::None),
    ("recording.*", Restart, H::None),
    ("clipboard.*", NextUse, H::None),
    // The Media hook restarts the watcher (`restart_media_watch`) and
    // re-applies `show_art`.
    ("media.*", Live, H::Media),
    ("voice.*", Live, H::Voice),
    ("usage.*", Live, H::Bars),
    // The model-proxy supervisor re-reads its section on every reload.
    ("model_proxy.*", Live, H::None),
    ("remote.*", NextUse, H::None),
    ("network.*", Restart, H::None),
    ("host_discovery.*", Restart, H::None),
    ("share.*", Restart, H::None),
    ("forward.*", Restart, H::None),
    ("preview.*", Live, H::Preview),
    ("lifecycle.*", NextUse, H::None),
    ("placement.*", NextUse, H::None),
    ("keybinds.*", Live, H::Keymap),
    // --- map-keyed tables ---
    ("profiles.*", NextUse, H::None),
    ("profile_order", NextUse, H::None),
    ("identities.*", NextUse, H::None),
    // Includes the per-project `project.<slug>.worktree_roots`.
    ("project.*", NextUse, H::None),
    // Daemon launches re-read `[env.*]` (and `sandbox.default_env`/`main_env`)
    // per request via `pipeline_run::with_fresh_registry`.
    ("env.*", NextUse, H::None),
    ("host.*", NextUse, H::None),
    ("bundle.*", NextUse, H::None),
    ("zone.*", NextUse, H::None),
    ("managed_tools.*", NextUse, H::None),
    // MCP servers/proxy are wired at launch (unsure: Restart).
    ("mcp_servers.*", Restart, H::None),
    ("mcp_proxy.*", Restart, H::None),
    ("lead.*", Restart, H::None),
    ("mcp.*", Restart, H::None),
    ("secrets.*", NextUse, H::None),
    ("credentials.*", NextUse, H::None),
    ("program_keybinds.*", Live, H::Keymap),
    // Program remaps are installed with the panes (unsure: Restart).
    ("program_remap.*", Restart, H::None),
];

/// Does one pattern segment match one key segment?
fn segment_matches(pattern: &str, key: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    match pattern.strip_suffix('*') {
        Some(prefix) => key.starts_with(prefix),
        None => pattern == key,
    }
}

/// Does `pattern` cover `key` (itself or anything beneath it)?
pub fn pattern_matches(pattern: &str, key: &str) -> bool {
    let pat: Vec<&str> = pattern.split('.').collect();
    let segs: Vec<&str> = key.split('.').collect();
    let Some((last, head)) = pat.split_last() else {
        return false;
    };
    // A trailing `*` covers the section itself as well as everything below.
    let (head, last) = if *last == "*" {
        (head, None)
    } else {
        (head, Some(*last))
    };
    let needed = head.len() + usize::from(last.is_some());
    if segs.len() < needed {
        return false;
    }
    let head_ok = head.iter().zip(&segs).all(|(p, k)| segment_matches(p, k));
    head_ok && last.is_none_or(|last| segment_matches(last, segs[head.len()]))
}

/// Specificity rank of a matching row: exact beats everything, then more
/// literal (non-`*`) characters.
fn specificity(pattern: &str, key: &str) -> (bool, usize) {
    let exact = pattern == key && !pattern.contains('*');
    let literal = pattern.chars().filter(|c| *c != '*').count();
    (exact, literal)
}

/// The class and hook of a dotted key (`ui.sidebar_width`,
/// `env.dev.sandbox.backend`), or `None` when no row covers it.
pub fn class_for(key: &str) -> Option<(ReloadClass, ReloadHook)> {
    RELOAD_CLASSES
        .iter()
        .filter(|(pattern, _, _)| pattern_matches(pattern, key))
        .max_by_key(|(pattern, _, _)| specificity(pattern, key))
        .map(|(_, class, hook)| (*class, *hook))
}

/// The most changed keys [`changed_keys`] reports; past it the list ends with
/// [`TRUNCATED`] (inside the cap).
pub const MAX_CHANGED_KEYS: usize = 512;

/// The marker closing a truncated [`changed_keys`] list.
pub const TRUNCATED: &str = "…";

fn join(path: &str, segment: &str) -> String {
    if path.is_empty() {
        segment.to_string()
    } else {
        format!("{path}.{segment}")
    }
}

/// A JSON value as an (optional) table: `Some(Some(map))` for an object,
/// `Some(None)` for an absent/null table, `None` for anything else.
type MaybeTable<'a> = Option<Option<&'a serde_json::Map<String, Value>>>;

fn as_table(value: &Value) -> MaybeTable<'_> {
    match value {
        Value::Object(map) => Some(Some(map)),
        Value::Null => Some(None),
        _ => None,
    }
}

fn diff_into(path: &str, old: &Value, new: &Value, out: &mut Vec<String>) {
    if out.len() > MAX_CHANGED_KEYS || old == new {
        return;
    }
    let tables = match (old, new) {
        // Two tables, or a table appearing/disappearing (a skipped-if-empty
        // section): recurse so the leaves are named.
        (Value::Object(_), _) | (_, Value::Object(_)) => as_table(old).zip(as_table(new)),
        _ => None,
    };
    let Some((old_map, new_map)) = tables else {
        // Scalars and arrays compare whole: a changed list is one key.
        out.push(path.to_string());
        return;
    };
    let before = out.len();
    let mut keys: Vec<&String> = old_map
        .into_iter()
        .chain(new_map)
        .flat_map(|map| map.keys())
        .collect();
    keys.sort();
    keys.dedup();
    for key in keys {
        let a = old_map.and_then(|map| map.get(key)).unwrap_or(&Value::Null);
        let b = new_map.and_then(|map| map.get(key)).unwrap_or(&Value::Null);
        diff_into(&join(path, key), a, b, out);
    }
    // `{}` vs `null` differ but have no leaves: name the table itself.
    if out.len() == before && !path.is_empty() {
        out.push(path.to_string());
    }
}

/// Diff two JSON trees into dotted leaf paths (map keys become segments,
/// arrays compare whole), capped at [`MAX_CHANGED_KEYS`] entries with a
/// trailing [`TRUNCATED`] marker inside the cap.
pub fn diff_values(old: &Value, new: &Value) -> Vec<String> {
    let mut out = Vec::new();
    diff_into("", old, new, &mut out);
    if out.len() > MAX_CHANGED_KEYS {
        out.truncate(MAX_CHANGED_KEYS - 1);
        out.push(TRUNCATED.to_string());
    }
    out
}

/// The dotted keys whose values differ between `old` and `new`.
pub fn changed_keys(old: &Config, new: &Config) -> Vec<String> {
    let (Ok(a), Ok(b)) = (serde_json::to_value(old), serde_json::to_value(new)) else {
        return vec![TRUNCATED.to_string()];
    };
    diff_values(&a, &b)
}

/// A reload's changed keys, bucketed by what applies them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReloadPlan {
    /// Live keys by the hook that applies them.
    pub live: BTreeMap<ReloadHook, Vec<String>>,
    /// Keys that take effect after a restart.
    pub restart: Vec<String>,
    /// Keys read afresh by their next use.
    pub next_use: Vec<String>,
    /// Keys no table row covers (the schema gate keeps this empty).
    pub unclassified: Vec<String>,
    /// The diff was capped (or could not be computed): callers run every hook.
    pub truncated: bool,
}

impl ReloadPlan {
    /// Should `hook` run for this reload? True when one of its keys changed,
    /// and always when the diff is incomplete.
    pub fn runs(&self, hook: ReloadHook) -> bool {
        self.truncated || self.live.contains_key(&hook)
    }

    /// Did anything change at all?
    pub fn is_empty(&self) -> bool {
        !self.truncated
            && self.live.is_empty()
            && self.restart.is_empty()
            && self.next_use.is_empty()
            && self.unclassified.is_empty()
    }
}

/// Bucket a list of changed keys (the testable core of [`plan`]).
pub fn plan_keys(keys: Vec<String>) -> ReloadPlan {
    let mut plan = ReloadPlan::default();
    for key in keys {
        if key == TRUNCATED {
            plan.truncated = true;
            continue;
        }
        match class_for(&key) {
            Some((ReloadClass::Live, hook)) => plan.live.entry(hook).or_default().push(key),
            Some((ReloadClass::Restart, _)) => plan.restart.push(key),
            Some((ReloadClass::NextUse, _)) => plan.next_use.push(key),
            None => plan.unclassified.push(key),
        }
    }
    plan
}

/// The reload plan for moving from `old` to `new`.
pub fn plan(old: &Config, new: &Config) -> ReloadPlan {
    plan_keys(changed_keys(old, new))
}

/// The status line width [`summary`] stays within.
pub const SUMMARY_COLS: usize = 120;

/// Append `label: a, b +N more` to `out` while it fits in `budget` columns.
fn push_section(out: &mut String, label: &str, items: &[String], budget: usize) {
    if items.is_empty() {
        return;
    }
    let mut section = format!(" · {label}: ");
    let mut shown = 0;
    for item in items {
        let sep = if shown == 0 { "" } else { ", " };
        let rest = items.len() - shown - 1;
        let tail = if rest == 0 {
            0
        } else {
            format!(" +{rest} more").chars().count()
        };
        let need = out.chars().count()
            + section.chars().count()
            + sep.chars().count()
            + item.chars().count()
            + tail;
        if need > budget {
            break;
        }
        section.push_str(sep);
        section.push_str(item);
        shown += 1;
    }
    let hidden = items.len() - shown;
    if hidden > 0 {
        if shown == 0 {
            write!(section, "{hidden} keys").expect("writing to String cannot fail");
        } else {
            write!(section, " +{hidden} more").expect("writing to String cannot fail");
        }
    }
    out.push_str(&section);
}

/// The reload status line: `None` when nothing changed, else e.g.
/// `Config reloaded · live: sidebar, weather · restart required: daemon.enabled`,
/// bounded to [`SUMMARY_COLS`] with `+N more`.
pub fn summary(plan: &ReloadPlan) -> Option<String> {
    if plan.is_empty() {
        return None;
    }
    let mut out = String::from("Config reloaded");
    let mut live: Vec<String> = plan.live.keys().map(|h| h.label().to_string()).collect();
    live.sort();
    push_section(&mut out, "live", &live, SUMMARY_COLS);
    push_section(&mut out, "restart required", &plan.restart, SUMMARY_COLS);
    push_section(&mut out, "next use", &plan.next_use, SUMMARY_COLS);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exact_beats_prefix_and_longest_prefix_wins() {
        assert_eq!(class_for("ui.language"), Some((Restart, H::None)));
        assert_eq!(
            class_for("ui.sidebar_detail_fields"),
            Some((Live, H::Sidebar))
        );
        assert_eq!(
            class_for("ui.fullscreen_keep_masthead"),
            Some((Live, H::Layout))
        );
        assert_eq!(
            class_for("ui.confirm_delete_project"),
            Some((NextUse, H::None))
        );
        assert_eq!(class_for("session.scrollback_lines"), Some((Live, H::None)));
        assert_eq!(class_for("session.anything_else"), Some((Restart, H::None)));
        assert_eq!(class_for("base_branch"), Some((NextUse, H::None)));
        assert_eq!(class_for("weather.location"), Some((Live, H::Weather)));
        assert_eq!(class_for("daemon.enabled"), Some((Restart, H::None)));
        // The daemon arms its producers at start; rule edits re-arm live.
        assert_eq!(
            class_for("daemon.headless_producers"),
            Some((Restart, H::None))
        );
        // Same class as `merge_queue.gate_on`: read at the next gate.
        assert_eq!(
            class_for("merge_queue.gate_sandbox"),
            class_for("merge_queue.gate_on")
        );
        assert_eq!(class_for("nonsense_root_key"), None);
        assert_eq!(class_for(""), None);
    }

    #[test]
    fn map_keyed_tables_match_one_segment_and_the_section() {
        assert_eq!(
            class_for("env.dev.sandbox.backend"),
            Some((NextUse, H::None))
        );
        assert_eq!(class_for("env.*.sandbox.backend"), Some((NextUse, H::None)));
        assert_eq!(class_for("env"), Some((NextUse, H::None)));
        assert_eq!(
            class_for("project.acme.worktree_roots"),
            Some((NextUse, H::None))
        );
        assert_eq!(class_for("sandbox.git.push"), Some((NextUse, H::None)));
        assert_eq!(class_for("plugins"), Some((Live, H::Plugins)));
    }

    #[test]
    fn cadence_and_restart_hooks_classify_live() {
        for key in [
            "bars.clock_format",
            "bars.date_format",
            "ci.poll_interval_secs",
            "pr_queue.enabled",
            "pr_queue.poll_interval_secs",
            "git.auto_fetch",
            "git.auto_fetch_interval_secs",
            "disk.scan_interval_secs",
            "loc.enabled",
            "loc.scan_interval_secs",
            "watch.pr_interval_secs",
        ] {
            assert_eq!(class_for(key), Some((Live, H::Schedule)), "{key}");
        }
        // Only the PR cadence is live; the rest of `[watch]` restarts.
        assert_eq!(class_for("watch.anything_else"), Some((Restart, H::None)));
        // Section defaults stay honest beside the cadence rows.
        assert_eq!(class_for("bars.top_left"), Some((Live, H::Bars)));
        assert_eq!(class_for("ci.ttl_secs"), Some((NextUse, H::None)));
        assert_eq!(class_for("git.merge_guard"), Some((NextUse, H::None)));
        assert_eq!(class_for("disk.anything"), Some((Restart, H::None)));
        assert_eq!(class_for("loc.other"), Some((NextUse, H::None)));
        assert_eq!(class_for("pr_queue.watch"), Some((NextUse, H::None)));
        assert_eq!(class_for("calendar.week_start"), Some((Live, H::Calendar)));
        assert_eq!(class_for("usage.enabled"), Some((Live, H::Bars)));
        assert_eq!(
            class_for("notifications.active_mode"),
            Some((Live, H::Notifications))
        );
        assert_eq!(class_for("media.show_art"), Some((Live, H::Media)));
        assert_eq!(class_for("media"), Some((Live, H::Media)));
        assert_eq!(class_for("session.scrollback_lines"), Some((Live, H::None)));
    }

    #[test]
    fn metrics_and_monitor_rows_classify() {
        assert_eq!(class_for("metrics.interval_secs"), Some((Live, H::Metrics)));
        assert_eq!(class_for("metrics.targets"), Some((Live, H::Metrics)));
        assert_eq!(class_for("metrics"), Some((Live, H::Metrics)));
        for key in [
            "monitor.window_ladder",
            "monitor.default_window",
            "monitor.default_style",
            "monitor.default_scale",
        ] {
            assert_eq!(class_for(key), Some((Live, H::Monitor)), "{key}");
        }
        assert_eq!(class_for("monitor.processes"), Some((Live, H::None)));
        assert_eq!(class_for("monitor.proc_rows"), Some((Restart, H::None)));
        assert_eq!(class_for("monitor.system"), Some((Restart, H::None)));
        assert_eq!(H::Metrics.label(), "metrics");
        assert_eq!(H::Monitor.label(), "monitor");
    }

    #[test]
    fn plan_routes_metrics_and_monitor_hooks() {
        let base = parse("");
        let new = parse(
            "[metrics]\ninterval_secs = 7.5\n\n[monitor]\ndefault_style = \"line\"\nproc_rows = 3\n",
        );
        let plan = plan(&base, &new);
        assert!(plan.runs(H::Metrics), "{plan:?}");
        assert!(plan.runs(H::Monitor), "{plan:?}");
        assert_eq!(
            plan.live.get(&H::Metrics).cloned().unwrap_or_default(),
            ["metrics.interval_secs"]
        );
        assert_eq!(
            plan.live.get(&H::Monitor).cloned().unwrap_or_default(),
            ["monitor.default_style"]
        );
        assert!(
            plan.restart.contains(&"monitor.proc_rows".to_string()),
            "{plan:?}"
        );
        let only_rows = plan_keys(vec!["monitor.proc_rows".into()]);
        assert!(!only_rows.runs(H::Monitor));
        assert!(!only_rows.runs(H::Metrics));
    }

    #[test]
    fn summary_names_the_new_hook_labels() {
        let plan = plan_keys(vec![
            "ci.poll_interval_secs".into(),
            "notifications.active_mode".into(),
            "plugins".into(),
            "media.enabled".into(),
        ]);
        let text = summary(&plan).expect("something changed");
        assert_eq!(
            text,
            "Config reloaded · live: media, notifications, plugins, refresh cadences"
        );
        assert_eq!(H::Schedule.label(), "refresh cadences");
    }

    #[test]
    fn pattern_grammar() {
        assert!(pattern_matches("env.*", "env"));
        assert!(pattern_matches("env.*", "env.a.b"));
        assert!(pattern_matches("env.*.sandbox", "env.a.sandbox.backend"));
        assert!(!pattern_matches("env.*.sandbox", "env.a"));
        assert!(!pattern_matches("env.*.sandbox", "env.a.git"));
        assert!(pattern_matches("ui.sidebar_*", "ui.sidebar_width"));
        assert!(!pattern_matches("ui.sidebar_*", "ui.language"));
        assert!(!pattern_matches("ui.sidebar_*", "ui"));
        assert!(pattern_matches("pins", "pins"));
        assert!(pattern_matches("pins", "pins.label"));
        assert!(!pattern_matches("pins", "pinsx"));
        assert!(!pattern_matches("", "x"));
        assert!(segment_matches("*", "anything"));
    }

    #[test]
    fn every_row_is_unique_and_hooks_label() {
        let mut seen = std::collections::BTreeSet::new();
        for (pattern, class, hook) in RELOAD_CLASSES {
            assert!(seen.insert(*pattern), "duplicate row {pattern}");
            assert!(!class.describe().is_empty());
            assert!(!hook.label().is_empty());
            if *class != Live {
                assert_eq!(*hook, H::None, "{pattern}: only live rows name a hook");
            }
        }
        for hook in [
            H::Weather,
            H::Sidebar,
            H::Theme,
            H::Bars,
            H::Calendar,
            H::Keymap,
            H::Panel,
            H::Help,
            H::Voice,
            H::Lsp,
            H::Preview,
            H::Automation,
            H::AppHost,
            H::Layout,
            H::Caps,
            H::Schedule,
            H::Notifications,
            H::Plugins,
            H::Media,
            H::Metrics,
            H::Monitor,
            H::None,
        ] {
            assert!(!hook.label().is_empty());
        }
        assert_eq!(Live.describe(), "applies live in a running thegn");
        assert_eq!(Restart.describe(), "takes effect after a restart");
        assert_eq!(NextUse.describe(), "used the next time it is needed");
    }

    #[test]
    fn diff_values_names_nested_leaves_and_compares_arrays_whole() {
        let old = json!({"a": 1, "t": {"x": 1, "y": [1, 2]}, "m": {"k1": {"v": 1}}});
        let new =
            json!({"a": 1, "t": {"x": 2, "y": [1, 2, 3]}, "m": {"k1": {"v": 1}, "k2": {"v": 3}}});
        assert_eq!(diff_values(&old, &new), ["m.k2.v", "t.x", "t.y"]);
        assert!(diff_values(&old, &old).is_empty());
    }

    #[test]
    fn diff_values_handles_appearing_tables_and_type_changes() {
        assert_eq!(diff_values(&json!({}), &json!({"s": {}})), ["s"]);
        assert_eq!(diff_values(&json!({"s": {"k": 1}}), &json!({})), ["s.k"]);
        assert_eq!(
            diff_values(&json!({"s": {"k": 1}}), &json!({"s": 3})),
            ["s"]
        );
        assert_eq!(diff_values(&json!(1), &json!(2)), [""]);
    }

    #[test]
    fn diff_values_caps_with_a_marker_inside_the_cap() {
        let old = json!({});
        let mut big = serde_json::Map::new();
        for i in 0..2000 {
            big.insert(format!("k{i:05}"), json!(i));
        }
        let keys = diff_values(&old, &Value::Object(big));
        assert_eq!(keys.len(), MAX_CHANGED_KEYS);
        assert_eq!(keys.last().map(String::as_str), Some(TRUNCATED));
        let plan = plan_keys(keys);
        assert!(plan.truncated);
        assert!(plan.runs(H::Weather));
        assert!(!plan.is_empty());
    }

    fn parse(toml_text: &str) -> Config {
        toml::from_str(toml_text).expect("config parses")
    }

    #[test]
    fn changed_keys_on_pretty_and_minified_toml() {
        let base = parse("");
        let pretty = parse(
            "[ui]\nsidebar_detail_fields = [\"branch\", \"age\"]\n\n[weather]\nlocation = \"Oslo\"\n\n[env.dev.sandbox]\nbackend = \"docker\"\n",
        );
        let minified = parse(
            "ui={sidebar_detail_fields=[\"branch\",\"age\"]}\nweather={location=\"Oslo\"}\nenv={dev={sandbox={backend=\"docker\"}}}",
        );
        let a = changed_keys(&base, &pretty);
        let b = changed_keys(&base, &minified);
        assert_eq!(a, b);
        assert!(a.contains(&"ui.sidebar_detail_fields".to_string()), "{a:?}");
        assert!(a.contains(&"weather.location".to_string()), "{a:?}");
        assert!(
            a.iter().any(|k| k.starts_with("env.dev")),
            "map keys become segments: {a:?}"
        );
        assert!(changed_keys(&Config::default(), &Config::default()).is_empty());
    }

    #[test]
    fn plan_buckets_and_summary() {
        let base = parse("");
        let new = parse(
            "[ui]\nsidebar_detail_fields = [\"branch\"]\n\n[weather]\nlocation = \"Oslo\"\n\n[daemon]\nenabled = false\n\n[sandbox]\nbackend = \"docker\"\n",
        );
        let plan = plan(&base, &new);
        assert!(plan.runs(H::Sidebar));
        assert!(plan.runs(H::Weather));
        assert!(!plan.runs(H::Lsp));
        assert!(
            plan.restart.contains(&"daemon.enabled".to_string()),
            "{plan:?}"
        );
        assert!(
            plan.next_use.contains(&"sandbox.backend".to_string()),
            "{plan:?}"
        );
        assert!(plan.unclassified.is_empty(), "{plan:?}");
        let text = summary(&plan).expect("something changed");
        assert!(
            text.starts_with("Config reloaded · live: sidebar, weather"),
            "{text}"
        );
        assert!(text.contains("restart required: daemon.enabled"), "{text}");
        assert!(text.contains("next use: sandbox.backend"), "{text}");
        assert!(text.chars().count() <= SUMMARY_COLS);
        assert_eq!(summary(&ReloadPlan::default()), None);
        assert!(ReloadPlan::default().is_empty());
    }

    #[test]
    fn plan_keys_collects_unclassified() {
        let plan = plan_keys(vec!["nonsense".into(), "theme.accent".into()]);
        assert_eq!(plan.unclassified, ["nonsense"]);
        assert_eq!(plan.live.get(&H::Theme).map(Vec::len), Some(1));
        assert_eq!(
            summary(&plan_keys(vec!["nonsense".into()])).as_deref(),
            Some("Config reloaded")
        );
    }

    #[test]
    fn summary_truncates_with_more() {
        let restart: Vec<String> = (0..40).map(|i| format!("daemon.key_{i:02}")).collect();
        let plan = ReloadPlan {
            restart,
            ..ReloadPlan::default()
        };
        let text = summary(&plan).unwrap();
        assert!(text.chars().count() <= SUMMARY_COLS, "{text}");
        assert!(text.contains("daemon.key_00"), "{text}");
        assert!(text.contains(" more"), "{text}");

        // A section that cannot fit even one item collapses to a count.
        let long = "x".repeat(200);
        let plan = ReloadPlan {
            next_use: vec![long.clone(), long],
            ..ReloadPlan::default()
        };
        let text = summary(&plan).unwrap();
        assert!(text.ends_with("next use: 2 keys"), "{text}");
    }
}
