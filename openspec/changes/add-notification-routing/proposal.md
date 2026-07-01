# Add notification routing (rules, DND, per-profile, sound)

## Summary

Layer a **routing decision** on top of the existing notification priority model:
user-defined action rules, do-not-disturb / quiet hours, runtime routing modes +
per-profile overlays, and an audible sound/bell channel. A single pure function
(`notification_route::decide`) maps each notification — with the current clock,
DND state, and active routing mode — to which channels fire (inbox, desktop,
in-app toast, sound) and its effective priority. The host applies it at one
dispatch chokepoint and at read time.

## Impact

- **AI 420** — user-defined action rules (`[[notifications.rules]]`): the "no
  user-defined action rules yet" gap in the fixed event→notification mapping.
- **AI 426** — do-not-disturb / quiet hours (`[notifications.dnd]` + a runtime
  toggle).
- **AI 427** — per-profile routing: runtime routing _modes_
  (`[notifications.modes.<name>]` + `active_mode`) plus a
  `[profiles.<p>.notifications]` overlay layering onto the existing
  `active_profile()` mechanism.
- **AI 429** — sound/bell (`[notifications.sound]`: terminal `BEL` default, or a
  configured command).

Extends the `notifications` capability. **No DB schema change** — routing is
derived from the stored row (kind, source_ref, worktree_path, message) at
dispatch and read time, so a config change reclassifies live.

## Rationale

Today the only personalization is per-kind priority + a desktop urgency
threshold. There is no way to mute by worktree or message text, silence
overnight, switch behavior for heads-down vs watching, or get an audible cue.
A pure decision function keeps the event-loop (~0% idle, no polling timer) and
read-time-derivation invariants intact while unifying every delivery channel
behind one testable rule set.

## Non-goals

- A stored routing/priority column (would freeze the decision at insert time).
- Push-to-phone (ntfy / Telegram — tasks 422/423), deferred.
- Hard dependency on the OS-level profiles/subprofiles feature: the
  `[profiles.<p>.notifications]` overlay rides the lightweight existing
  `[profiles]`/`active_profile()` layering and degrades to a no-op when unset.
