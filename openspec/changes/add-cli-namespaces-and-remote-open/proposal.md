# Add CLI namespaces, headless worktree lifecycle, and remote open

## Summary

Restructure the `szhost` CLI around the domain model: a `wt` namespace giving
the worktree — the app's core noun — the same noun-verb grammar and **headless
lifecycle** (`wt new` / `wt rm`) every other noun already has; a `repo`
namespace collapsing `repos`/`recent`; blanket `--json` on every list-shaped
read surface plus a documented exit-code contract; grouped `--help` (clap 4.5
has no native subcommand grouping — a runtime-rendered template does it,
drift-guarded by a unit test); shell completions via `clap_complete`; and
`szhost open <repo>` — a remote control that focuses a running compositor
through a DB `intents` mailbox (no IPC) or launches focused when none runs.
Legacy bare verbs (`list`, `diff`, `disk`, `clean`, `repos`, `recent`) stay
functional forever but are hidden from help.

## Impact

- **A 6** (one core, many front doors) — the CLI becomes a first-class front
  door: scripts and agents can drive the full worktree lifecycle without the
  TUI.
- **AK 454** (headless CLI) — `wt new`/`wt rm`/`open --no-launch` are the
  concrete headless seams, ahead of any HTTP API.
- **D 41/47** (create/delete worktree) — the existing TUI-only lifecycle gains
  CLI parity by reusing the same `superzej_core::worktree` pipeline.
- **State DB** — one additive migration (v34: `intents` table, the CLI→
  compositor mailbox). `CREATE TABLE IF NOT EXISTS` style; hydration tolerates
  the table's absence so parallel-branch DBs keep working.
- No AI-layer dependency; everything here is AI-free shell surface.

## Rationale

The CLI grew organically: every noun except the worktree got a namespace, so
worktree operations are scattered as bare top-level verbs and a newcomer cannot
guess that `list` lists worktrees. Worse, the lifecycle is TUI-only — an agent
or script cannot create a worktree headlessly, which undercuts the
parallel-worktrees use case superzej exists for. Machine output is incidental
(`--json` on five commands) for a tool whose users are half machines. And 21
visible commands render as one flat `--help` list. The remote-control `open`
uses the established DB-as-mailbox pattern (`notify push` precedent): the
model-refresh ticker picks intents up within ~1s, no IPC, consistent with the
no-daemon architecture.

## Non-goals

- **No breaking changes** — legacy verbs keep working with byte-identical
  output; only their help visibility changes.
- **No interactive CLI** — richer interaction belongs in the TUI; the CLI stays
  script-safe (`--force`/`--yes` bypasses, no prompts on `--json` paths).
- **No output templating / jsonpath** (gh/kubectl style) — `--json` piped to
  `jq` covers it.
- **No HTTP/gRPC API** — that remains AK; this change only firms up the CLI
  seams such an API would later share.
- **No sandbox provisioning from `wt new`** — creation registers the worktree;
  the compositor prepares sandboxes lazily on first open, as it does today.
