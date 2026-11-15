# Sidebar row fields

`WorktreeField` is a resident-only plugin contribution for small, live values
such as CI state. It is deliberately an on-demand surface: the host starts no
timer or per-plugin scheduler for it.

Declare API `0.5.0`, the `surface:rowfield` capability, resident mode, and a
`WorktreeField` contribution. The contribution's `caps` names the field and
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
