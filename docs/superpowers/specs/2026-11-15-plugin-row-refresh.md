# Plugin row refresh after a glyph scan, and one-shot plugin rows

Status: design (THE-2003 follow-up slice). No code lands with this note; the
implementation is the next slice of THE-2003, which stays open.
Evidence is cited against `f99a583e7`.

## Problem

A `WorktreeField` contribution puts a plugin-owned value on each sidebar
worktree row. The value changes only when the plugin pushes an `update`
notification. The host never asks for a refresh: `docs/extending/row-field.md`
says the surface is on demand and that a plugin "must not spawn an idle
timer". A plugin whose value depends on git state (for example "N commits
unpushed to the review remote", or "rebase in progress") therefore goes stale
whenever the user commits, and it has no signal that tells it when to look
again.

Two related needs:

1. A refresh signal that fires when the host already knows a worktree's git
   state changed, without adding a wake source.
2. Values that should not outlive their moment ("deployed 30s ago",
   "rebase in progress"), so a plugin does not have to remember to clear them.

## Current state (file:line)

- Contribution caps: `WorktreeFieldCaps { field, slot }` at
  `crates/thegn-core/src/plugin_api.rs:308`; slot enum `RowFieldSlot` at :318.
- Row value on the wire: `RowUpdate { worktree, text, role }` at
  `crates/thegn-core/src/plugin_api.rs:300`. It has no
  `#[serde(deny_unknown_fields)]`, so an unknown field is silently ignored by
  the current host.
- Loop-side store: `RowValue { text, role }` and
  `RowFieldSnapshot = BTreeMap<(plugin, field), BTreeMap<worktree key, RowValue>>`
  at `crates/thegn-host/src/handlers/plugins.rs:55` and :60; held in
  `PluginsState::row_fields` (:106). `apply_row_update` (:1254) replaces a
  field's full value set (insert/remove at :1372-1374). The worktree key is an
  absolute path or a branch name.
- Projection: `plugin_row_sync::project_plugin_fields`
  (`crates/thegn-host/src/plugin_row_sync.rs:27`) resolves path or branch keys
  onto rows. It is called by `sync_plugin_fields` (`run.rs:1707`) only after
  plugin damage (`run.rs:14553`) or a model swap (`run.rs:12891`).
- Glyph scan: the dirty/ahead/behind scan runs off-loop inside hydration
  (`collect_sidebar_status`, `crates/thegn-host/src/hydrate.rs:1612`; scan jobs
  at :1976-2030; rows land in `status.git` from :2099). Only the active
  worktree is rescanned on its window (`should_rescan_glyphs`, :142); other rows
  are reused from `hydrate::glyph_cache` (`reused`, :192-223). The result
  reaches the loop as a whole `FrameModel` on `model_rx`, drained at
  `run.rs:12468`, where stale generations are dropped (`generation !=
hydration_gen`) before anything is applied. `SidebarStatus::git` is
  `BTreeMap<String, GitGlyphs>` (`sidebar.rs:431`) and `GitGlyphs` is
  `PartialEq` (`sidebar.rs:116`).
- Plugin event delivery: the only host-to-plugin event stream today is the
  daemon control feed. `feed_hub::daemon_source`
  (`crates/thegn-host/src/plugins/feed_hub.rs:35`) subscribes to the daemon,
  and each feed frame is wrapped as
  `{"kind": EventKind::Custom("control.feed"), "payload": …}` and sent by
  `SessionWriter::notify(PluginCallback::OnEvent, …)`
  (`handlers/plugins.rs:1786-1795`). The typed variants `FocusChanged`,
  `WorktreeChanged` and so on (`plugin_api.rs:2105-2122`) are not emitted by
  any host site.
- Subscriptions: the `subscribe` verb (`handlers/plugins.rs:1581`) records
  `(PluginId, EventKind)` pairs in `PluginRuntime::subscriptions`
  (`plugin_api.rs:1232`, set at :1261), bounded by `MAX_PLUGIN_SUBSCRIPTIONS =
64` (:907) and `MAX_EVENT_KIND_BYTES = 128` (:908). `EventKind::Custom` is
  `#[serde(untagged)]`, so a plain string such as `"thegn.worktree_scanned"`
  in the `events` list deserializes to `Custom(..)`. The core test
  `subscriptions_are_bounded_per_plugin_and_duplicate_is_idempotent`
  (`plugin_api.rs:5120`) already subscribes `Custom` kinds. A host-originated
  custom event therefore needs no wire-type change.
- Per-plugin outbound queue: `SessionWriter`
  (`crates/thegn-svc/src/plugin/session/admission.rs:165`) admits frames FIFO
  under `MAX_QUEUED_FRAMES = 32` and `MAX_QUEUED_BYTES`
  (`crates/thegn-svc/src/plugin/session.rs:18-19`), with two frames and 64 KiB
  reserved for control traffic. `notify_coalesced(callback, key, params)`
  (`admission.rs:287`) replaces a still-queued frame with the same key
  (`replacement_index`, :31), but only after the last non-replaceable frame, so
  ordering against other traffic is preserved. Live tiles already use it
  (`cmd/plugin/live_tile.rs:125-138`).
- Deadline wake precedent: the loop has one timed poll site
  (`test/idle-poll-guard.sh`). It folds an absolute plugin deadline into the
  timeout with `idle_poll::fold_deadline(timeout,
plugins_state.next_app_deadline(), now)` (`run.rs:17202`;
  `next_app_deadline` at `handlers/plugins.rs:166`). `None` keeps the true
  idle block.

## Decision 1: refresh path

**Chosen: a host-injected `EventKind::Custom("thegn.worktree_scanned")` event,
sent per worktree whose glyph scan landed with a changed result.**

### Trigger

In the model-swap block (`run.rs:12468`), after the generation check accepts
`next_model` and before `model` is replaced:

1. If no plugin is eligible (see Recipients), do nothing. This check is a
   cached `bool` on `PluginsState`, recomputed on subscribe, register, exit
   and reload. It is not recomputed per swap, so a session with no
   subscriber does no per-swap work at all.
2. For each path in `next_model.sidebar_status.git`, compare it with
   `model.sidebar_status.git`. Emit when the value differs, or when the path
   has no prior value. A failed hydration keeps the last good sidebar
   (`retain_model_for_hydration_failure`, `run.rs:12502`), so it yields no diff and no event. A stale generation is
   dropped before this point, so it yields none either.

The diff compares `GitGlyphs` values. It does not compare the set of paths
the scan actually ran on, because reused rows come from the cache unchanged
and so never differ. That keeps the trigger correct without threading a
"freshly scanned" set out of `hydrate.rs`. A later lane can add that set if a
workspace switch (where every row is new to `model`) turns out to cause a
burst. See open question 2.

### Recipients

A plugin receives the event only if all three hold:

- it has an accepted `WorktreeField` contribution in `entry.contributions`
  (the same filter `sync_plugin_fields` uses at `run.rs:1711-1722`);
- `entry.runtime.subscriptions()` contains
  `(pid, EventKind::Custom("thegn.worktree_scanned"))`;
- it has a live resident `writer` (`entry.writer.is_some()`, not `disabled`).

A plugin that only subscribes, without a `WorktreeField`, gets nothing. The
event exists to refresh row fields and is not a general git-watch API.

### Frame

```json
{
  "method": "on_event",
  "params": {
    "kind": "thegn.worktree_scanned",
    "payload": { "path": "/abs/worktree", "branch": "feat/x" }
  }
}
```

`branch` is the row's branch (`SidebarRow::branch`), or `null` for a detached
HEAD. Glyph values are not included. The plugin computes its own value, and
keeping the payload to two keys keeps the contract small. Fields can be added
later without a wire change.

### Coalescing and bounds

Send with `writer.notify_coalesced(PluginCallback::OnEvent, key, params)`,
where `key = "thegn.worktree_scanned\0" + path`. A second changed scan of the
same worktree, arriving before the plugin drains the first, replaces it in
place. That gives at most one pending frame per (plugin, worktree), keeping
the latest. Different worktrees do not coalesce with each other, and the
existing per-plugin bound (`MAX_QUEUED_FRAMES` minus the control reserve)
still caps the total. `SessionFailure::Full` drops the event, counts the drop
in the plugin stats totals (`PluginsState::counters`), and is not retried. A
missed refresh self-heals on the next changed scan. Admission is a mutex push
and a `Notify`, with no I/O, so it is safe on the loop.

### Idle cost

None. The event rides a wake the loop already took, the `model_rx` drain
(`WakeSource::Model`). It adds no timer, no thread, and no poll deadline. A
session with no eligible plugin pays nothing beyond the cached-bool check.

### Rejected: plugin-scheduled refresh

The alternative is to let each plugin declare `CadenceHint::Interval` on its
`WorktreeField` (`plugin_api.rs:326`) or run its own sleep loop and re-push.
That costs one wake per plugin per interval while the user is idle, which
breaks the 0%-idle contract. It also refreshes when nothing changed and lags
when something did. `docs/extending/row-field.md` already forbids the
plugin-side timer, and the host deliberately runs no per-plugin scheduler for
this surface. Rejected.

### Rejected: reuse the daemon feed (`control.feed`)

Plugins can already subscribe to the daemon event feed. The glyph scan is
host-side hydration state, though, not a daemon event. Routing it through the
daemon would add a socket round-trip and a second producer for one fact, and
it would deliver to every feed subscriber (up to `MAX_FEED_SUBSCRIBERS = 8`,
`feed_hub.rs:14`) whether or not it owns a row field. Rejected.

## Decision 2: one-shot plugin rows

### Wire

`RowUpdate` gains two optional fields:

```rust
pub struct RowUpdate {
    pub worktree: String,
    pub text: String,
    pub role: RowRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u32>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub until_scan: bool,
}
```

- `ttl_secs`: the host drops the value this many seconds after it was
  admitted. Valid range is `1..=86_400`. `0` is rejected with `Invalid`, so
  zero can never be read as "never expires". Out-of-range values are
  rejected the same way.
- `until_scan: true`: the host drops the value the next time this worktree
  produces a `thegn.worktree_scanned` trigger (a changed glyph scan, as in
  Decision 1). This is evaluated whether or not the plugin subscribed to the
  event.
- Both may be set. The value is dropped on whichever fires first.
- Neither set: today's behaviour. The value persists until the next push or
  the plugin's exit.

### Host behaviour

- `RowValue` gains `expires_at: Option<Instant>` and `until_scan: bool`,
  filled in by `apply_row_update` from `Instant::now() + ttl`.
- **TTL expiry.** Add `PluginsState::next_row_expiry() -> Option<Instant>`,
  the minimum `expires_at` across the store. Fold it into the existing single
  timed poll site as `min(next_app_deadline(), next_row_expiry())` at
  `run.rs:17202`. Do not add a second site, because
  `test/idle-poll-guard.sh` pins exactly one. With no TTL rows the result is
  `None`, so idle stays a true block. With TTL rows there is exactly one wake
  per distinct expiry instant: a one-shot deadline, not a cadence. On each
  wake, `expire_rows(now)` removes due values and returns
  `PluginDamage::SidebarContent` if it removed any. The existing
  damage-to-`sync_plugin_fields` path (`run.rs:14540-14553`) then repaints the
  sidebar row only.
  - Lazy expiry at the next render was rejected. On an idle screen nothing
    renders, so a "30s ago" value would stay visible indefinitely, which
    defeats the TTL.
- **until_scan.** In the Decision 1 trigger loop, for each triggering path,
  remove `until_scan` values keyed by that path, or by the row's branch,
  because values may be keyed by branch name (`plugin_row_sync.rs:44`). Do the
  removal before sending the event, so the plugin's re-push lands after the
  drop and is not erased by it. This runs even when no plugin is subscribed,
  so the cached "eligible" bool from Decision 1 must also be true when any
  `until_scan` value is stored.
- **Identical re-push.** Re-sending the same text and role with a fresh
  `ttl_secs` refreshes `expires_at`. It is not sidebar damage, which keeps the
  invariant of the existing test
  `row_field_updates_replace_clear_and_skip_identical_damage`
  (`handlers/plugins.rs:4007`). Compare `(text, role)` for damage, not the
  whole `RowValue`.
- The existing caps (`MAX_ROW_WORKTREES_PER_FIELD`, text sanitising) apply
  unchanged.

### API version impact

**Minor bump: 0.5.0 to 0.6.0.** The change is additive, but a bump is still
required. `RowUpdate` does not deny unknown fields, so a 0.5 host would
silently accept `ttl_secs` and keep the value forever, which is exactly the
failure a transient value must avoid. `HostContract::negotiate` refuses a
manifest whose minor exceeds the host's (`plugin_api.rs:686`). So a plugin
that declares `api = "0.6.0"` fails closed on an old host instead of leaving
a permanent "rebase in progress". 0.5 manifests negotiate unchanged.

Concretely, the implementing lane:

- sets `API_VERSION` to 0.6.0 (`plugin_api.rs:49`), adds a v0.6 line to its
  doc comment, and updates the version assertion at `plugin_api.rs:2725`;
- regenerates the wire snapshot with
  `THEGN_UPDATE_SNAPSHOTS=1 cargo test -p thegn-core --test plugin_api_wire`.
  This produces `docs/api/plugin-api-0.6.json`, required by
  `snapshot_file_is_named_for_the_current_version`
  (`crates/thegn-core/tests/plugin_api_wire.rs:157`). The 0.5 file is kept.
  The diff should show only the two new optional `RowUpdate` properties;
  `RowUpdate` is already in the snapshot's type list (:50).

The Decision 1 event alone changes no wire type and no
`x-thegn-*-support` table, so on its own it would leave the snapshot and
version unchanged. Shipping both in one slice means one bump.

## Files a later lane changes

| File                                                                              | Change                                                                                                                                                                        |
| --------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `crates/thegn-core/src/plugin_api.rs`                                             | `RowUpdate` optional fields; `API_VERSION` 0.6.0 and doc; a `pub const WORKTREE_SCANNED_EVENT: &str = "thegn.worktree_scanned"`; version test                                 |
| `docs/api/plugin-api-0.6.json`                                                    | new snapshot (regenerated, reviewed)                                                                                                                                          |
| `crates/thegn-host/src/handlers/plugins.rs`                                       | `RowValue` expiry fields; `apply_row_update` validation; `next_row_expiry`, `expire_rows`, `drop_until_scan`, `emit_worktree_scanned`, the cached eligibility bool            |
| `crates/thegn-host/src/plugins/feed_hub.rs`                                       | no behaviour change; a module-doc note that host-originated events do not go through the hub, so nobody later routes them there                                               |
| `crates/thegn-host/src/run.rs` (glyph-scan completion = the model swap at :12468) | one call into the handler module after the generation check, plus one `min` in the `fold_deadline` argument at :17202. Logic stays in `handlers/plugins.rs`, not the god-file |
| `crates/thegn-host/src/plugin_row_sync.rs`                                        | a pure helper that resolves `path` to the store keys (path and branch), shared by `drop_until_scan` and the projection so the two cannot drift                                |
| `docs/extending/row-field.md`                                                     | subscribe example for `thegn.worktree_scanned`, the `ttl_secs` / `until_scan` fields, `api = "0.6.0"` for one-shot use                                                        |

`hydrate.rs` is unchanged unless open question 2 is taken.

## Tests

Host (`handlers/plugins.rs` tests, using the existing `row_spec` fixture at
:3320 and a test `SessionWriter`):

- `worktree_scanned_coalesces_per_plugin_and_path`: two changed swaps for the
  same path leave one queued frame carrying the latest payload. A second path
  queues a second frame.
- `worktree_scanned_without_subscriber_is_a_no_op`: (a) a `WorktreeField`
  plugin that is not subscribed and (b) a subscribed plugin without a
  `WorktreeField` both receive nothing, and the eligibility bool is false.
- `worktree_scanned_skips_unchanged_and_stale_scans`: equal `GitGlyphs` give
  no frame. A rejected generation gives no frame.
- `worktree_scanned_full_queue_drops_and_counts`: drop on `Full`, with the
  counter incremented.
- `row_ttl_zero_and_out_of_range_are_rejected`.
- `row_ttl_expires_at_deadline_and_damages_sidebar`: `next_row_expiry`
  returns the minimum; `expire_rows` at that instant returns `SidebarContent`
  and removes only due values.
- `row_ttl_refresh_with_identical_text_is_not_damage`.
- `until_scan_row_dropped_on_changed_scan_keyed_by_path_or_branch`, including
  a check that the drop happens before the event is queued.

Idle zero-wake:

- `next_row_expiry_is_none_without_ttl_rows`, and a `fold_deadline` case
  showing `None` stays `None` with an eligible subscriber present and no
  scans.
- `test/idle-poll-guard.sh` must stay green (still one timed poll site).
- One `just bench-soak` run on the implementing branch, with a subscribed
  `WorktreeField` plugin and no TTL rows, to confirm wake and spawn ceilings
  are unchanged.

Core:

- `RowUpdate` round-trip with and without the new fields; an old-shape JSON
  value still deserializes.
- `plugin_api_wire` snapshot (0.6).

## Help and ratchet steps

- `plugin_api_wire`: regenerate and review as above. It is run as a
  named contract gate in the justfile (:525-526).
- No new action, keybind, config key or panel section, so no
  `ACTION_SPECS` help claim and no config-key ratchets.
- `docs/help/plugins.md:149-153` mentions `WorktreeField`. If the
  implementing lane adds a sentence there about the refresh event, run
  `just help-ratchet-update` only if a ratchet file changes. Prose that
  mentions nothing new needs no ratchet edit. Otherwise keep the user-facing
  detail in `docs/extending/row-field.md`, which no help ratchet covers.
- New `let _ =` / `.ok()` in `handlers/plugins.rs`: use the `// best-effort:`
  comment, and check `test/ignored-result-ratchet.txt` for the file's count.
- No new thread or timer, so no QoS or thread-name ratchet change. No DB
  schema change.
- CHANGELOG `[Unreleased]` entry in the implementing slice, not here.

## Out of scope

- **Author field.** A per-row "last commit author" value, or any host-supplied
  row data, is a separate host feature, not a plugin row refresh. It is not
  designed here.
- A general host-to-plugin git or worktree event API (typed `WorktreeChanged`
  / `FocusChanged` emission). This note adds one narrowly-gated custom event.
- Refresh triggers other than the glyph scan (PR/CI hydration, fs-watch).
- Per-row rate limiting of plugin pushes beyond the existing caps.

## Open questions

1. `until_scan` drops on the next _changed_ scan, the same trigger as the
   event. An unchanged rescan keeps the value. Is "any rescan" wanted instead?
   That would need the freshly-scanned path set from `hydrate.rs`, because
   the swap diff cannot see unchanged rescans.
2. Workspace switch: every row of the newly hydrated workspace has no prior
   value in `model`, so the first swap emits one event per worktree (bounded
   by `MAX_ROW_WORKTREES_PER_FIELD` and the queue cap, and coalesced). If this
   burst is unwanted, emit only for paths in a freshly-scanned set carried on
   `SidebarStatus`. Doing so touches `hydrate.rs`.
3. Should the payload carry the new `GitGlyphs` (dirty/ahead/behind) so simple
   plugins need no git call of their own? This is additive later, so it is
   left out for now.
