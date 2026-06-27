# Design

## Tiers (derived from kind, overridable)

| Priority   | Drives                                    | Default kinds                                                                                     |
| ---------- | ----------------------------------------- | ------------------------------------------------------------------------------------------------- |
| **Alert**  | red ⚑ flag + Critical toast               | AgentFailed, TestFailed, LogError, ProcessFailed                                                  |
| **Notice** | neutral unread count, Normal toast        | Assigned, Mentioned, StatusChanged, BlockerResolved, PrLinked, Overdue, PrStateChanged, AgentDone |
| **Info**   | inbox list only, never counted, Low toast | WorktreeCreated, ProcessExited                                                                    |

Priority is **derived from `kind` at read time** (no stored column, no
`user_version` bump) and overridable per-kind via `[notifications.priority]`. The
red flag counts Alert-unread only; the unread count = Alert+Notice; Info never
increments a counter but still appears in the inbox.

## Where it lands

- `notification.rs`: `enum Priority { Info, Notice, Alert }` + `NotificationKind::
{ ALL, as_str, default_priority }`.
- `event_bus.rs`: `Priority::urgency()` (Alert→Critical, Notice→Normal, Info→Low),
  agreeing with `NotificationUrgency::from_event` on overlapping events.
- `config.rs` `NotificationsConfig`: `priority: BTreeMap<String,String>` +
  `priority_of`, `alert_kind_names`, `counted_unread_kind_names`.
- `db.rs`: `get_alert_counts_by_worktree(&[&str])` / `get_unread_counts_by_worktree
(&[&str])` build a dynamic `kind IN (?, …)` clause from the passed sets (so a
  config remap reclassifies historical rows live, keeping the grouped SQL counts).
- `hydrate.rs` + `panel/`: compute `alert_notifications` vs `unread_notifications`;
  header shows red `⚑ {alert}` when alerts exist, else neutral unread.

## Invariants

Core priority logic is unit-tested against the 95% gate; sidebar badges already
split alert vs unread per row and inherit the corrected counts with no logic change.
