# Sidebar row fields

`WorktreeField` is a resident-only plugin contribution for small, live values
such as CI state. It is deliberately an on-demand surface: the host starts no
timer or per-plugin scheduler for it.

Declare API `0.5.0` (or `0.6.0` to use [transient values](#transient-values)),
the `surface:rowfield` capability, resident mode, and a `WorktreeField`
contribution. The contribution's `caps` names the field and
its slot:

```toml
id = "ci"
name = "CI row field"
version = "0.1.0"
api = "0.5.0"
capabilities = ["surface:rowfield"]
command = ["sh", "./ci-rowfield.sh"]
mode = "resident"

[[contributions]]
id = "ci.status"
extension_point = "WorktreeField"
label = "CI"
surface = "ci/status"
[contributions.cadence]
kind = "on_demand"
[contributions.caps]
field = "check"
slot = "detail" # or "badge"
```

After `activate`, send an `update` notification:

```json
{
  "method": "update",
  "params": {
    "surface": "ci/status",
    "rows": [{ "worktree": "/repo/wt", "text": "passing", "role": "Good" }]
  }
}
```

Each update replaces the complete field value set; `rows: []` clears it.
`worktree` may be an absolute worktree path or its branch name. The host strips
controls and bidi markers, keeps one display line, and caps values at 48
display columns (badge values are capped to 12 at render time). It maps
`Good`, `Warn`, and `Bad` through the active theme; unknown roles are dim and
absent values render nothing.

Add `plugin:<plugin>/<field>` to `[ui].sidebar_detail_fields` to enable the
field and preserve its configured position. Badge entries also need that list
entry. Values are resident-only and disappear when the plugin exits. A plugin
should push its initial snapshot after `activate`, then wait on stdin; it must
not spawn an idle timer.

## Refreshing on git changes

A value that depends on git state (unpushed commits, a rebase in progress)
goes stale when the user commits. Instead of polling, subscribe to the
host-originated `thegn.worktree_scanned` event:

```json
{ "method": "subscribe", "params": { "events": ["thegn.worktree_scanned"] } }
```

The host then sends an `on_event` notification for each worktree whose git
glyph scan (dirty, ahead/behind, line counts) landed with a changed result:

```json
{
  "method": "on_event",
  "params": {
    "kind": "thegn.worktree_scanned",
    "payload": { "path": "/repo/wt", "branch": "feat/x" }
  }
}
```

`branch` is the worktree's live `HEAD` branch, or `null` for a detached HEAD.
Recompute the value for that worktree and push a fresh `update`.

- **Eligibility.** Only a plugin with an accepted `WorktreeField` contribution
  that has subscribed, and is running, receives the event. A subscription
  alone, without a row field, gets nothing; this is not a general git-watch
  API.
- **Changed results only.** The event fires when a scan's result differs from
  the one on screen, or a worktree appears for the first time (including every
  row after a workspace switch). An unchanged scan, a failed hydration, and a
  stale scan superseded by a newer one send nothing.
- **Coalescing.** At most one event per worktree is pending for a plugin; a
  newer scan of the same worktree replaces it in place, so you always see the
  latest. Under backpressure (your stdin queue is full) an event may be
  dropped without retry; the next changed scan of that worktree self-heals it.

The event rides the host's existing hydration wake, so it costs nothing while
the user is idle. The rule above still holds: do not add an idle timer of your
own.

## Transient values

API `0.6.0` adds two optional per-row fields for values that should not
outlive a moment, such as "rebase in progress" or "pushed 30s ago":

```json
{
  "method": "update",
  "params": {
    "surface": "ci/status",
    "rows": [
      {
        "worktree": "/repo/wt",
        "text": "rebasing",
        "role": "Warn",
        "until_scan": true
      },
      { "worktree": "feat/x", "text": "pushed", "role": "Good", "ttl_secs": 30 }
    ]
  }
}
```

- **`ttl_secs`** (`1..=86400`): the host drops the value this many seconds
  after it admitted the update and repaints the row. `0` and anything above a
  day are rejected as `Invalid` (the whole update is refused and the previous
  values stay), so a zero is never read as "never expires". The expiry is a
  one-shot deadline folded into the host's single poll site: one wake per
  distinct expiry, no cadence, and nothing at all when no value carries a TTL.
- **`until_scan: true`**: the host drops the value on that worktree's next
  _changed_ git glyph scan, the same trigger as `thegn.worktree_scanned`
  above. It is matched by the value's key, path or branch name, and applies
  whether or not you subscribed to the event. An unchanged scan keeps it.
- **Both set**: whichever fires first drops the value.
- **Neither set**: the value persists until your next push or your exit, as
  before.

The drop happens **before** the host queues the `thegn.worktree_scanned`
event for that scan, so a value you re-push in answer to the event lands after
the drop and is never erased by it.

Re-pushing the same text and role with a fresh `ttl_secs` (or a changed
`until_scan`) refreshes the stored deadline without repainting the sidebar.

Declare `api = "0.6.0"` to use either field. A host older than 0.6 does not
know them and would keep the value forever; a 0.6 manifest makes such a host
refuse the plugin instead, so the transient value fails closed. A `0.5.0`
manifest keeps working unchanged on a 0.6 host.
