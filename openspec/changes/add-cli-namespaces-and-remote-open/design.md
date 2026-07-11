# Design

## Namespaces via retention-variants, not aliases

clap's `visible_alias` cannot alias across nesting levels (`list` → `wt list`),
so the mechanism is retained `Command` variants: legacy `List`/`Diff`/`Disk`/
`Clean`/`Repos`/`Recent` stay in the enum, marked `#[command(hide = true)]`,
dispatching to the same functions as the namespaced forms. Shared
`#[derive(clap::Args)]` structs (`DiffArgs`, `DiskArgs`, `CleanArgs`) are
`#[command(flatten)]`-ed into both so flags can never drift. Output must stay
byte-identical between the two spellings.

## Headless lifecycle reuses the wizard pipeline

`wt new` is the TUI wizard worker minus UI and sandbox prep:
`worktree::branch_name` → `resolve_base`/`--base` → `worktree_path` →
`add_checked` (serial git-mutation lock, `.git/info/exclude`) → `put_worktree`
(+ best-effort `set_worktree_env`). Sandbox prep and direnv warm are skipped —
the compositor prepares lazily on first open. Plain output is the absolute path
only, so `cd $(thegn wt new x)` works.

`wt rm` mirrors `delete_groups`: env-precedence teardown run **synchronously**
(a CLI exiting would orphan the detached thread the TUI uses), then
`worktree::remove`, then DB cleanup. Tab-group rows are deleted by the new
`delete_tab_groups_for_worktree` (keys the `tab_groups.worktree` column) rather
than reconstructing group names — a stale row resurrects the worktree at next
launch.

## Remote open: DB intents mailbox

New `intents` table (v34, additive `CREATE TABLE IF NOT EXISTS`): `id, kind,
payload(JSON), created_at`. `IntentStore::take_intents(kind)` is
claim-and-delete in one transaction, FIFO. The compositor consumes during
`build_model` (off-loop) — `unwrap_or_default()` tolerates a missing table for
DBs stamped by parallel branches — and the run-loop model drain applies the
**last** `focus_workspace` intent via the existing `switch_workspace`. Pickup
latency is the model-refresh tick (~1s), same as notifications.

Live-instance detection: the per-profile flock (`<root>/run/thegn.lock`) is
currently a no-op for the default profile. `acquire_singleton` now always takes
the lock, but default-profile contention still returns `Acquired` silently —
observably identical (the lock was advisory-warn only; nested dev instances
keep working). `instance_running()` is a non-blocking probe that degrades to
`false` (→ launch) on any error.

## Grouped help

clap 4.5 cannot group subcommands (clap-rs/clap#1553). `cli_help::attach`
installs a **top-level-only** `help_template` whose commands block is rendered
at runtime from the live `clap::Command` (skipping `is_hide_set()`), grouped by
a single `GROUPS` table. Names and about-strings come from clap, so text cannot
drift; a unit test enforces every non-hidden command appears in exactly one
group (adding a command without grouping it fails CI).

## Exit-code contract

`0` ok · `1` error · `2` transient/retryable (existing `host provision`
precedent) · `3` target not found. A typed `NotFound` error is downcast in
`main`, so cmd functions stay `anyhow::Result`.
