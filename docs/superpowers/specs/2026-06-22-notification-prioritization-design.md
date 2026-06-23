# Notification prioritization — design

**Date:** 2026-06-22
**Status:** approved

## Problem

superzej has no real notification priority model. Priority is fragmented across
three ad-hoc mechanisms that disagree:

- **Desktop toasts** gate on `NotificationUrgency` (Low/Normal/Critical) computed
  from `Event` in `event_bus.rs` — independent of the inbox.
- **Sidebar alert badge** counts a _hardcoded_ kind list in SQL
  (`db::get_alert_counts_by_worktree`: `test_failed, agent_failed, log_error,
process_failed`).
- **Panel header flag** (`panel/sections/mod.rs`, `Section::Notifications` arm)
  renders a red `⚑ {unread}` from `panel.unread_notifications` — the count of **all**
  unread. So an informational `worktree_created` + `process_exited` shows as a red
  "⚑ 2" attention flag.

Result: lifecycle/informational events (a worktree created, a process exited) raise
the same red attention flag as a test failure. The user wants those events to be
non-flag-worthy.

## Goal

A single source of truth for notification priority on `NotificationKind` that
coherently drives the red flag, the neutral unread count, and desktop toasts.

## Model (three tiers)

| Priority   | Drives                                                               | Default kinds                                                                                       |
| ---------- | -------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------- |
| **Alert**  | red ⚑ flag + Critical desktop toast                                  | `AgentFailed, TestFailed, LogError, ProcessFailed`                                                  |
| **Notice** | neutral unread count, no red flag, Normal toast                      | `Assigned, Mentioned, StatusChanged, BlockerResolved, PrLinked, Overdue, PrStateChanged, AgentDone` |
| **Info**   | inbox list only — never counted, Low toast (below default threshold) | `WorktreeCreated, ProcessExited`                                                                    |

Decisions:

- The red flag counts **Alert-priority unread only**.
- **Info** never increments any counter, but still appears in the inbox list as
  history.
- Priority is **overridable per-kind** in config (`[notifications.priority]`).
- The flag threshold stays fixed at `Alert`; a per-kind remap gives full control
  without a second knob.

## Approach

Priority is **derived from `kind` at read time**, not stored. No DB schema change:
notifications already persist `kind`. A `Priority` enum + `NotificationKind::
default_priority()` table live in `superzej-core`; config can remap any kind. The
per-worktree count queries take config-derived kind-name sets, so a config remap
reclassifies counts live (including historical rows) while keeping the grouped
counts in SQL.

Rejected alternatives: a `priority` column (needs a `user_version` bump + backfill,
and freezes priority at insert so config remaps wouldn't apply); pure in-Rust
bucketing (loses the efficient grouped-by-worktree counts hydrate relies on).

## Changes (by file)

- **`superzej-core/src/notification.rs`** — `enum Priority { Info, Notice, Alert }`
  (`rank`, `parse`); `NotificationKind::ALL` + `as_str` (serde snake_case names);
  `NotificationKind::default_priority`.
- **`superzej-core/src/event_bus.rs`** — `Priority::urgency()` (Alert→Critical,
  Notice→Normal, Info→Low). Keep the `Event`-based desktop path; add a test that the
  kind-derived priority agrees with `NotificationUrgency::from_event` for the
  overlapping events.
- **`superzej-core/src/config.rs`** (`NotificationsConfig`) — `priority:
BTreeMap<String,String>`; `priority_of(kind) -> Priority`; helpers
  `alert_kind_names()` and `counted_unread_kind_names()` (= Alert+Notice, Info
  excluded) that classify `NotificationKind::ALL` via `priority_of`.
- **`superzej-core/src/db.rs`** — `get_alert_counts_by_worktree(alert_kinds:
&[&str])` and `get_unread_counts_by_worktree(counted_kinds: &[&str])` build a
  dynamic `kind IN (?, …)` clause from the passed slice (empty slice → empty map).
- **`superzej-host/src/hydrate.rs`** — pass the config-derived sets to the two
  queries; compute `panel.alert_notifications` (unread Alert) and
  `panel.unread_notifications` (unread Alert+Notice) via `priority_of` over the
  loaded inbox.
- **`superzej-host/src/panel/mod.rs`** — add `alert_notifications: usize`.
- **`superzej-host/src/panel/sections/mod.rs`** — header shows red `⚑ {alert}` when
  alerts exist, else neutral `{unread} unread`, else `inbox zero`.
- **`superzej-host/src/panel/sections/notifications.rs`** — full-view header sources
  "needs attention" from `alert_notifications`.
- **`config/config.toml.example`** — document `[notifications.priority]`.

Sidebar badges (`sidebar.rs` / `chrome.rs`) already split `alert_count` vs
`unread_count` per row; they inherit the corrected counts with no logic change.

## Testing

Core (≥95% line gate): `default_priority` total over `ALL`, Alert = the four
failure kinds, worktree/process-exited = Info; `priority_of` honors override +
falls back on missing/garbage; kind-name helpers exclude Info and reflect
overrides; the count queries exclude Info and honor a live demotion;
`Priority::urgency` agrees with `from_event`.

Host: a panel-header render test — an Info-only inbox yields no red ⚑; adding a
`test_failed` yields red `⚑ 1`.

Manual: `just start name=dev`, create a worktree / exit a pane → header stays
neutral; trigger a failure → red ⚑ + sidebar alert badge.
