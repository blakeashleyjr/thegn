# Chunk 2 — host move verb, daemon boundary, docs, and ratchet verification

## Scope

Add the auditable host command and documentation on top of chunk 1. The command is cold-safe, target-first, resumable, and does not reroot the process or expose migration as a daemon API. Keep the implementation in a new focused module; `session.rs` receives only the enum/dispatch hook needed to reach it.

## Files touched

- `crates/thegn-host/src/cmd/mod.rs`
- `crates/thegn-host/src/cmd/session.rs`
- `crates/thegn-host/src/cmd/session_move.rs` (new)
- `docs/help/cli.md`
- `docs/help/daemon-and-sessions.md`
- `test/smoke.sh` (only if an existing isolated CLI smoke harness needs the new daemon-free cases)

Ratchet/snapshot obligations in this chunk are verification obligations, not extra files: do not edit `docs/api/control-v1.json`, `test/completion-slot-ratchet.txt`, `test/help-ratchet.txt`, or `test/help-prose-ratchet.txt` unless a scoped ratchet test identifies an actual permitted shrink. The expected result is unchanged control/help snapshots and no new completion/help debt. No `config/config.toml.example` change is needed because no config key is introduced.

## Approach

1. Add `SessionAction::Move` with positional `worktree: String`, required `--to-profile <name>`, `--kill`, `--dry-run`, and `--json`. Add its completion IDs exactly as chunk 1 specified. Keep `--profile` as the source selector.
2. Dispatch move before `session::connect`; it must work when no daemon exists. Open source with the active `Db`, resolve/open the target with the path-only profile helper and `Db::open_at`, and run synchronous DB work off the event-loop boundary according to existing host command conventions.
3. In a new `session_move` module, build the core source bundle and pure plan. Refuse an actively running source compositor with `profile::instance_running()`. Preflight source daemon liveness by discovering the source control endpoint from the active DB, listing sessions, and unioning exact-worktree `SessionInfo` rows with IDs in selected pane maps/dispatch rows. If the source daemon is registered but unreachable while a referenced ID exists, fail closed. With `--kill`, call the existing client kill seam for each live source ID and confirm a second list has no survivors. Dry-run performs none of these mutations and prints the full plan.
4. Preflight target conflicts and resume fingerprints before writes. Import using the chunk-1 store transaction, read back the sanitized rows/fingerprint, and only then call the source cleanup transaction. On source cleanup failure, return the existing retryable error with `target_committed=true`, `target_confirmed=true`, and `source_deleted=false`; a retry must recognize the target import and finish cleanup rather than duplicate it.
5. After confirmed source cleanup, discover a target daemon from the target DB's own daemon registry and target state scope. Send the existing `notify.push` message if reachable; absence/unreachability is a warning in human and JSON output, never a primary migration failure. Do not load target config or set target credential environment. The target's next resurrection uses the existing session reconstruction path with cleared source pane-session IDs, causing fresh target daemon sessions to be created/adopted.
6. Render a stable human summary and JSON audit object containing source/target profile, exact worktree, groups, row counts, live/killed IDs, dry-run/resumed state, target commit/confirmation, source deletion, and notification status. Redact all row payloads, commands, scrollback, reports, notes, tokens, and credential paths. Use the host's retryable exit contract for partial completion.
7. Document syntax, profile selection, what moves (worktree registration, groups/tabs, sidebar pins/collapse/layout, dispatch ledger), what does not (git files, config/credentials, global layout/cache, source daemon identity), cold/live/`--kill`/dry-run/resume behavior, and target notification degradation in both help pages. Do not add an `ACTION_SPECS` palette action: this is a process-boundary admin CLI operation.

## Tests to add/run

Host tests must use temporary explicit state roots and in-memory/fake control seams. They must not open the user's live `$XDG_STATE_HOME` and must not run a real migration against it.

- CLI parse/help tests for `session move`, required target profile, flags, JSON mode, and completion IDs.
- Daemon-free dry-run tests with `XDG_STATE_HOME` set to a fresh temp directory: missing worktree, missing target, source==target, cold valid plan, target group/UI conflict, and no source daemon.
- Orchestration tests with a fake control client: live session blocks without `--kill`, kill/re-list confirmation, unreachable referenced daemon fails closed, target-first import, read-back confirmation, source-delete failure returns retryable partial state, retry resumes, and notification failure is only a warning.
- Audit-output tests assert required fields and assert no pane command, scrollback, report/note, token, or credential path is emitted.
- Run the control schema snapshot test to prove no route/wire change, completion-slot ratchet, capability catalog coverage, and help ratchets. The snapshot/ratchet files should remain unchanged.

Run only:

```text
just quick thegn-host
cargo nextest run -p thegn-host session_move
cargo nextest run -p thegn-host completion_slots_are_bound_or_pinned
cargo nextest run -p thegn-host action_docs_ratchet
cargo nextest run -p thegn-svc control_wire_matches_the_committed_snapshot
```

If the smoke harness is updated, run only its targeted move cases with a fresh temporary `XDG_STATE_HOME`; never run e2e or a live-state invocation. Do not run `just test`, `just ci`, or a full-workspace compile.

## Dependency/overlap

This chunk depends on chunk 1's public migration bundle/plan/store and must run serially after it. Files are disjoint from chunk 1: do not edit core files, the capability catalog, completion catalog, snapshot files, or ratchet files here. The implementation may add no daemon route or service file.

## Done criteria

- `thegn session move <worktree> --to-profile <name>` works cold, refuses unsafe live/source-concurrent states, supports explicit `--kill` and side-effect-free `--dry-run`, and never reroots or loads target credentials/config.
- Target import is confirmed before source deletion; interruption is resumable and partial completion is retryable and auditable.
- Source daemon IDs are never written to target pane/dispatch state; target resurrection can create fresh daemon sessions; git worktrees are untouched.
- Human/JSON output is stable, complete, and redacted; target notification degrades with an explicit warning.
- CLI/help/completion tests, capability coverage, help ratchets, completion-slot ratchet, and the unchanged control-schema snapshot pass under the scoped commands.
- No palette action, config key, DB schema bump, control route, credential transfer, e2e test, or live-state invocation was introduced.
- Commit exactly as:

```text
feat(host): add session profile move CLI and docs
```
