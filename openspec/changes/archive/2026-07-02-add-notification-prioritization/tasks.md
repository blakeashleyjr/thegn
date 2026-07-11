# Tasks

## 1. Core priority model (thegn-core)

- [ ] 1.1 `Priority { Info, Notice, Alert }` + `NotificationKind::{ALL, as_str,
default_priority}` — **unit tests**: total over ALL, Alert = the four failure
      kinds, worktree/process-exited = Info.
- [ ] 1.2 `NotificationsConfig.priority` map + `priority_of` (override + fallback) +
      `alert_kind_names`/`counted_unread_kind_names` (exclude Info) — **unit tests**
      honoring a live override.
- [ ] 1.3 `event_bus.rs` `Priority::urgency()` — **test** agreement with
      `NotificationUrgency::from_event` on overlapping events.

## 2. Counts + panel (host)

- [ ] 2.1 `db.rs` dynamic `kind IN (?, …)` count queries from passed sets — **test**
      Info excluded + live demotion reclassifies.
- [ ] 2.2 `hydrate.rs`/`panel`: `alert_notifications` vs `unread_notifications`;
      header red `⚑ {alert}` only when alerts exist — **render test** (Info-only ⇒ no
      flag; add test_failed ⇒ red ⚑ 1).
- [ ] 2.3 Document `[notifications.priority]` in `config.toml.example`.

## 3. Validate

- [ ] 3.1 Run `just ci` (includes `openspec-validate`).
