# Tasks

## 1. Core config (superzej-core)

- [x] 1.1 Extend `NotificationsConfig` with `rules`, `dnd`, `sound`, `modes`,
      `active_mode` (all `serde(default)`); new sub-structs `NotificationRule`,
      `DndConfig`, `SoundConfig`, `NotificationMode` + `SoundMode` `config_enum!`
      — **unit tests**: defaults parse, unknown values warn-and-default.
- [x] 1.2 `ProfileConfig.notifications` overlay (`NotificationsOverlay`) +
      `apply(&mut NotificationsConfig)` + `Config::effective_notifications(repo_root)`
      layering (profile → global → repo overlay) — **unit tests** for precedence + every-field apply.

## 2. Core decision (superzej-core)

- [x] 2.1 `notification_route.rs`: `RouteDecision`, `SoundEmit`, `RouteCtx`,
      `decide()` — **unit tests**: each selector (kind/worktree glob/source
      prefix/message regex/min_priority/modes/profile), `set_priority`, `route`,
      `mute`, `drop`, `stop`.
- [x] 2.2 `scheduled_dnd_active` + `parse_window` (chrono, midnight-wrap, weekday
      tokens) + `allow_priority` break-through — **unit tests** across wrapping,
      plain, and weekday windows.
- [x] 2.3 `sound_emit` gating (`mode`, `min_priority`, per-priority override) —
      **unit tests**.

## 3. Host (superzej-host)

- [x] 3.1 `notify.rs` `NotifyState` chokepoint (`decide` + `record`); route the
      live emit sites (run.rs agent/process/test/worktree) through it — record
      gate + desktop-vs-publish gate + sound. Background inbox-only hydrate sites
      keep recording (documented boundary: rules govern _live delivery_).
- [x] 3.2 Sound emission in `notify.rs`: `SoundEmit::Command` spawned off-thread;
      `SoundEmit::Bell` latched (`pending_bell`) + written by the render flush.
- [x] 3.3 Runtime `dnd_forced`/`active_mode` state; DND-toggle (Ctrl+Alt+d) +
      mode-cycle (Ctrl+Alt+m) keybindings + palette (via `palette: true`);
      statusbar chip (chrome.rs) — **render test** for the chip.
- [~] 3.4 Inbox badge priority stays `[notifications.priority]`-driven (rules
  govern live delivery, not read-time reclassification) — deliberate scope
  boundary; documented in design.md + config.toml.example.

## 4. Docs + validate

- [x] 4.1 Document `[notifications.rules]`/`.dnd`/`.sound`/`.modes` + the
      `[profiles.<p>.notifications]` overlay in `config/config.toml.example`.
- [x] 4.2 Gates green: `cargo fmt --check`, `clippy --workspace --all-targets
-D warnings`, core+host tests, `just coverage` (core ≥95%),
      `openspec validate --strict`. (`smoke` / `nix-build` not run in this
      sandbox — read-only nix store.)
