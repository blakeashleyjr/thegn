# Chunk 3 — daemon spawn, CLI/MCP/UI, and docs

## Scope

Implement the host-side operation and user-facing flows after chunks 1 and 2.
The daemon is the spawn owner; the UI only requests work and consumes the
existing placement channel.

## Files touched

- `crates/thegn-host/src/daemon/fork.rs` (new)
- `crates/thegn-host/src/daemon/service.rs`
- `crates/thegn-host/src/daemon/agent_open.rs`
- `crates/thegn-host/src/daemon/session.rs`
- `crates/thegn-core/src/models.rs` (shared `AdoptIntent` placement payload)
- `crates/thegn-host/src/cmd/mod.rs`
- `crates/thegn-host/src/cmd/session.rs`
- `crates/thegn-host/src/cmd/session_fork.rs` (new)
- `crates/thegn-host/src/cmd/mcp.rs`
- `crates/thegn-host/src/handlers/adopt.rs`
- `crates/thegn-host/src/handlers/session_fork.rs` (new)
- `crates/thegn-host/src/handlers/mod.rs`
- `crates/thegn-host/src/run.rs`
- `crates/thegn-host/src/keymap.rs`
- `crates/thegn-host/src/keymap_specs.rs`
- `docs/help/daemon-and-sessions.md`
- `docs/help/configuration.md`
- `docs/cli.md`
- `test/help-ratchet.txt`
- `test/help-prose-ratchet.txt`
- `test/smoke.sh` (scoped CLI smoke only; no e2e)

`config/config.toml.example` is intentionally not changed: there is no new
config key. If the configuration help adds a capability explanation, keep it
descriptive and do not imply a TOML knob.

## Approach

1. Add `daemon/fork.rs` as a focused orchestration helper. Extend the live
   `SessionEntry` with a memory-only resolved recipe and safe fork metadata;
   keep `SessionMeta`, tombstones, `SessionInfo` responses, and DB free of
   argv/env. Add an actor message/oneshot for the bounded history tail. Do not
   clone the emulator or replay source bytes into the child screen.
2. Implement `DaemonService::fork` beside `open`: validate the source and
   native id, call the core plan, fresh-resolve configured agents through
   `daemon/agent_open.rs`, reapply sandbox/cap wrapping, allocate a new id, and
   use the existing registration/event/lease path. Set identity env values in
   one helper, with `THEGN_FORKED_FROM` and optional owner-only scrollback file.
   Best-effort cleanup belongs to fork lifecycle, not tombstone recipe state.
   Dead/tombstoned sessions return a clear error naming `sessions.open`; no
   child process is created on any validation failure.
3. Extend `AdoptIntent`/planner only as needed for `tab` placement and use the
   existing `graft` primitive. Add the pane action through a new small handler
   and a thin `run.rs` dispatch hook. It must use the existing channel/waker
   path and never call git, DB, or subprocess synchronously in the render loop.
4. Add `session fork` parsing/output and native-source selection. For
   `--fork-worktree`, call the existing worktree creation operation first,
   remap a cwd inside the old root to the new root, then call `sessions.fork`.
   Report a surviving worktree after a second-step failure; never implicitly
   delete it. Show `forked_from` in text, JSON, and session picker/list output.
5. Add MCP state-tool dispatch for `sessions.fork` using the catalog-declared
   args, with no raw env/argv path. Keep daemon-disabled and non-session pane
   errors explicit. Add the CLI/UI/help documentation and the help/action
   ratchets. The smoke addition must set `XDG_STATE_HOME` to a fresh temp dir.

## Overlap/dependency

No file overlaps chunks 1 or 2. This chunk depends on both prior commits:
chunk 1 supplies policy/harness/record/catalog contracts and chunk 2 supplies
wire/client/proto types. The Lead must run all three serially. The conditional
`models.rs` ownership is here if placement needs `AdoptIntent.tab`; chunk 1
must not touch it in that case.

## Tests to run

- `just quick thegn-host`
- `cargo nextest run -p thegn-host daemon::fork`
- `cargo nextest run -p thegn-host daemon::service`
- `cargo nextest run -p thegn-host session_fork`
- `cargo nextest run -p thegn-host adopt`
- `cargo nextest run -p thegn-host help`

For any CLI smoke invocation, use an isolated state directory, for example:

```sh
XDG_STATE_HOME="$(mktemp -d)" target/debug/thegn session fork --help
```

Use the already-built scoped binary only if present; do not start a long build
or run e2e. Do not run `just test`, `just ci`, a live-state migration, or a
binary against the normal `$XDG_STATE_HOME`.

## Done criteria

- Live raw sessions fork to a new id/pid without disturbing the source;
  native harness sessions use only the harness optional operation; unsupported
  and dead sources fail without spawning.
- Recipe is memory-only; `SessionInfo`, tombstones, DB rows, MCP, CLI JSON,
  and control wire contain no env/argv/credential/transcript payload.
- Identity and optional scrollback handoff are correct, owner-only, bounded,
  and cleaned up best-effort. Sandbox/cap wrapping is reapplied.
- Adopt placement supports sibling split and requested new tab through the
  existing path; worktree creation/fork failure domains match the design; the
  disabled-daemon message is explicit.
- CLI, MCP, palette/pane action, session list/picker, help pages, help ratchets,
  control-schema/completion/env-overlay checks, and scoped smoke are green.
- Commit exactly as: `feat(the-29): daemon session fork CLI and UI`.
