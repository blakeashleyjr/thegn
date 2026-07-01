# Design

## The decision (pure, core)

`notification_route::decide(kind, source_ref, message, worktree, cfg, ctx) ->
RouteDecision` where:

- `RouteDecision { record: bool, effective_priority: Priority, desktop: bool,
toast: bool, sound: Option<SoundEmit> }`
- `SoundEmit { Bell, Command(String) }`
- `RouteCtx { now_local: chrono::NaiveDateTime, dnd_forced: Option<bool>,
active_mode: String }`

Steps:

1. Base priority = `cfg.priority_of(kind)` (existing per-kind override).
2. Walk `cfg.rules` in order. A rule matches when **every present** selector
   matches: `kind`/`kinds`, `worktree` (glob), `source` (prefix), `message`
   (regex), `min_priority`, `modes` (active-mode membership), `profile`. On
   match, apply the action — `set_priority`, `route` (channel subset),
   `mute` (no ephemeral channels), `drop` (also skip the inbox record),
   `sound` (override) — and stop if `stop = true`.
3. DND active = `ctx.dnd_forced.unwrap_or(cfg.dnd.active_at(ctx.now_local))`.
   When active and `effective_priority < cfg.dnd.allow_priority`, force
   `desktop=false, toast=false, sound=None`; the inbox record is kept.
4. Sound fires only if the channel is allowed, `cfg.sound.mode != Off`, and
   `effective_priority >= cfg.sound.min_priority`.

`Dnd::active_at` + `parse_window` (chrono) handle `"HH:MM-HH:MM"` windows
including wrap-past-midnight and optional weekday tokens. All pure, unit-tested
against the 95% core gate.

## Config (superzej-core, config.rs)

`NotificationsConfig` gains `serde(default)` fields — existing configs keep
working: `rules: Vec<NotificationRule>`, `dnd: DndConfig`, `sound: SoundConfig`,
`modes: BTreeMap<String, NotificationMode>`, `active_mode: String`. `SoundMode`
(`Off|Bell|Command`, default `Bell`) uses `config_enum!`; the sub-structs follow
the `MediaConfig` struct/Default idiom.

Per-profile routing: `ProfileConfig` gains a `notifications` overlay with
`apply(&mut base)`, mirroring `profile.sandbox.apply` / `profile.keybinds`.
`Config::effective_notifications(repo_root, slug)` layers
profile → global → workspace → repo-root overlay, following `effective_keybinds`
/ `repo_sandbox`.

## Host

- **One chokepoint** `notify::dispatch(...)` replaces the fragmented
  `db.put_notification` + `event_bus.publish_with_notification` pairs at the ~8
  emit sites; it calls `decide`, then conditionally records, toasts, desktops,
  and sounds.
- **Sound** (`src/sound.rs`): off-thread dispatcher modeled on
  `desktop_notify::spawn`. `Command` spawns detached; `Bell` sets
  `app.pending_bell` + pulses `TerminalWaker` so the **render block writes
  `\x07` on the next flush** — the loop owns stdout, so BEL is never written
  from a side thread.
- **Runtime state**: `dnd_forced: Option<bool>`, `active_mode: String`; a
  DND-toggle + mode-cycle keybinding (keymap.rs) + palette commands + a
  statusbar chip (chrome.rs, `warm N/M` precedent), repainted via the master
  `dirty` channel.
- **Read-time counts** (hydrate.rs): reclassify inbox alert/unread with the
  rule-aware effective priority (all selectors are stored on the row). The SQL
  kind-level per-worktree fast path stays when `rules` is empty; otherwise
  aggregate per-worktree in Rust over loaded rows.

## Invariants

- **Event loop**: no new timer. DND is read from the clock at dispatch; the chip
  refreshes on the existing 2s ticker. Sound/desktop stay off-thread; BEL rides
  the render flush.
- **Render**: the DND/mode chip is chrome — a `dirty` (chrome) repaint, never a
  per-pane-output recompose; render_plan invariants unchanged.
- **State**: no `user_version` bump; routing derived from stored rows.
