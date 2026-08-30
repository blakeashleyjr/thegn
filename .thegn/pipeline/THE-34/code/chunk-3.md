# THE-34 chunk 3 — CLI event-tail projection

## Scope

Expose the already-authorized `events.subscribe` capability through a small
CLI reference client. This pays the actual missing catalog cell and documents
the feed without creating a second control API.

## Exact files touched

- `crates/thegn-core/src/capability.rs` (`events.subscribe` adds `Surface::Cli`)
- `crates/thegn-core/src/completion/catalog.rs` (classify `events tail` value
  slots using existing source kinds)
- `crates/thegn-host/src/cmd/mod.rs` (new sibling module)
- `crates/thegn-host/src/cmd/events.rs` (new `events tail` parser/runner/tests)
- `crates/thegn-host/src/main.rs` (command and dispatch arms)
- `crates/thegn-host/src/cmd/session.rs` (CLI coverage ledger)
- `crates/thegn-host/src/cmd/api.rs` (runtime coverage ledger)
- `docs/help/cli.md` (command and JSON streaming usage)
- `docs/help/daemon-and-sessions.md` (thin-client/event-feed usage)
- `docs/ARCHITECTURE.md` (capability projection and feed contract note)

`test/completion-slot-ratchet.txt` is intentionally not edited: the new
`--kinds` and `--session` slots are classified immediately in the core catalog,
so the shrink-only ratchet remains valid. `test/surface-gaps-ratchet.txt` and
the help ratchets likewise should remain unchanged; the test runs are the
ratchet update for a no-new-debt projection. No config key is added.

## Approach

1. Add `cmd/events.rs` as a sibling, using the typed options-bearing
   `ControlClient` from chunk 2. `events tail` has `--kinds`, `--session`,
   `--signal-lag`, and `--json`; it waits on the stream rather than polling.
   Use one formatter: human lines for the default and one-frame-per-line JSON
   for `--json`. Preserve `Hello` as the first frame and render the lag marker
   explicitly.
2. Reuse the existing session connection/discovery/auth path. Unix sockets use
   the current same-user policy; remote TCP uses the existing bearer token.
   A missing daemon returns a concise recoverable CLI error. The command never
   enables session input and must not interact with `--allow-session-input`.
3. Add the catalog CLI surface and both ledger entries/tests. Add completion
   rows for the two value arguments with the appropriate existing source kind
   (`Structural` for free-form/narrowing syntax or `Session` where the live
   session source is safe); do not pin new debt in the completion ratchet.
4. Update the authored help pages and architecture capability section. Explain
   filters, opt-in loss signaling, auth, no replay, and the `sessions.list` /
   `worktrees.list` resync path. Do not add a generated page by hand or a new
   config reference.

## Dependency/overlap

Serial after chunk 2 because the command consumes its subscription options and
canonical formatter. It is file-disjoint from chunks 1 and 2 except for no
transport files; catalog and help files are owned only here. The lead can run
no chunk in parallel with this one because the dependency is semantic even
where paths do not overlap.

## Tests to run

- `just quick thegn-core`
- `cargo nextest run -p thegn-core capability`
- `cargo nextest run -p thegn-core completion`
- `just quick thegn-host`
- `cargo nextest run -p thegn-host events`
- `cargo nextest run -p thegn-host completion_slots_are_bound_or_pinned`
- `cargo nextest run -p thegn-host cli_control_verbs_cover_catalog`
- `cargo nextest run -p thegn-host help`
- `git diff --check`

Do not run the proposed live-daemon smoke test: the issue's architecture
constraints prohibit e2e and live state-DB use. Cover no-daemon behavior and
frame formatting with unit tests. Any manual invocation must use a fresh
temporary `XDG_STATE_HOME`.

## Done criteria

- `events.subscribe` is declared and implemented on HTTP, gRPC, plugin, and
  CLI through the one catalog; no new surface-gap line exists.
- `thegn events tail --json` is a streaming NDJSON reference client using the
  existing socket/auth path, with filter and lag flags matching the transport
  contract. Human mode and no-daemon degradation are tested.
- New value-taking CLI arguments are classified in
  `thegn_core::completion::CATALOG`; completion, surface-gap, and help ratchets
  pass with no new pinned debt.
- Help/API architecture docs describe auth, interlocks, bounded loss, and
  resync. No config or DB/render changes land.
- Commit exactly as: `feat(the-34): expose filtered event feed through CLI`
