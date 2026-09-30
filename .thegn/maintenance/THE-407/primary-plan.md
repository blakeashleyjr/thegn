# Primary decisions — THE-407 (unblocks row 665)

Good, precise investigation: `BUILTIN_TABS = [work]` in core, the host registry
separately registering `observe`, and `run.rs` building `AppHost` once without
reconciling it on a successful reload. Three decisions.

## DECISION 1 — `run.rs` is in scope, narrowly

My brief excluded it and you are right that complete reload handling needs it.
Approved, with a boundary: touch **only** the reload-reconciliation path, name the
function in your report, and add nothing else to that file. It is one of this
repo's acknowledged god-files and the standing instruction is not to grow it — a
helper in a sibling module called from `run.rs` is better than logic inside it.

No other lane in this batch touches `run.rs`, so there is no contention today.

## DECISION 2 — built-in tabs do NOT project from the capability catalog

You flagged schema projection as unspecified. Deciding it: **the capability catalog
is not the right home.** `thegn_core::capability::CATALOG` exists so the control
API, gRPC, CLI verbs, MCP tools and plugin host calls all project one list of
_verbs_. A UI tab is a surface, not a capability, and putting it there would make
every control surface inherit a concept none of them can invoke.

Instead: **core owns the built-in tab set** (it is what `[apps]` validates against),
and the **host registry projects from core** — never the reverse. That keeps
`thegn-core` substrate-free, keeps config validation in the crate that owns config,
and does not move the catalog across the seam in either direction.

## DECISION 3 — an unknown tab is a specific config error, not a silent drop

"Actionable validation diagnostics" means: an `[apps]` entry naming a tab that does
not exist **fails validation, names the unknown tab, and lists the known ones**.
Silently dropping it is how the current bug feels to a user — they wrote config and
nothing happened. A refusal that says `unknown app tab "observe-2"; known tabs are
work, observe` is the whole value of this change.

## Reload constraints

- A config change is a **chrome** change, so the render decision is `Full`. Do not
  let an `AppHost` rebuild produce a per-pane recompose.
- **No blocking I/O on the loop.** If the rebuild needs I/O, it happens off-thread
  and hands back over a channel with a `TerminalWaker` pulse.
- Reload must be **idempotent**: reloading unchanged config must not rebuild, and
  must not reorder or reset tabs the user is looking at. A rebuild that silently
  changes the active tab is worse than not reloading.

## Validation

Attempt `nix develop --command cargo check -p <crate> --all-targets` and a narrow
`cargo nextest run -p <crate> <filter>`. **The pipeline sandbox mounts
`/nix/store` read-only, so this usually fails outright** — say exactly that and
stop if it does. The primary runs clippy, the crate-wide nextest and the ratchets
centrally.

Never report a verdict for code you could not compile; state what you could not
run.
