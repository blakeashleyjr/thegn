# THE-76 — security / test / bug review verdict

- **Branch:** `tg/the-76-pipeline-v2`, reviewed at `ff61a179`
- **Reviewer:** security/test/bug stage (THE-76 lane)
- **Base:** `main` re-merged per the lead addendum (`6d780b31` — 76 commits from
  main since the branch's own merge: THE-64/65/67/75/77/85 all landed)

## Verdict

PASS

Ready for the merge queue (`thegn integrate` — not run by this review).

## What was reviewed

- The re-merge of `main` (one conflict, `CHANGELOG.md` — both feature sections
  kept; treefmt-clean) and the **full branch diff** `git diff main...HEAD`
  (31 files: `pipeline_run.rs` (new, pure), `cmd/dispatch.rs`, `cmd/session.rs`,
  `agent_task.rs`, `config_pipeline.rs`, `capability.rs`, `control.rs`,
  `db_notification.rs`, `completion/catalog.rs`, `agent_open.rs` (comment-only),
  config example, help pages, openspec deltas, smoke, CHANGELOG, lane docs).
- All three chunk done-reports and every "Unverified" item in them, plus the
  architect-review verdict's follow-ups and accepted deviations.
- Adversarial pass over the addendum's named risk surface: traversal/symlink on
  the artifact gate, prompt-render injection, wait blocking/DB-holding,
  empty-prompt rejection, close/liveness truthfulness, catalog coverage.

## Gates run (scoped per the dev-loop policy)

- **Mandatory (addendum):**
  `cargo nextest run -p thegn-core -E "test(env_overlay) | test(config_example) | test(control_schema) | test(capability)"` — **26/26 green**;
  `cargo nextest run -p thegn-host -E "test(complete) | test(help) | test(catalog_tests) | test(mq_assets)"` — **91/91 green**.
- `just quick thegn-core` / `just quick thegn-host` — clean.
- `cargo clippy -p thegn-host --tests` — clean.
- `cargo nextest run -p thegn-core pipeline_run` (14) and
  `-p thegn-host "test(session) | test(dispatch) | test(open_stage) | test(session_line) | test(agent_open)"` (137) — all green.
- `cli_control_verbs_cover_catalog` run explicitly — green.
- `test/ratchet.sh ignored-result` (325 pinned) and `forge-leak` (4 pinned) —
  clean; `cmd/session.rs`'s one new `let _ =` carries the sanctioned
  best-effort comment (the failed-stamp on the dispatch Err path).
- `nix run .#openspec -- validate --all --strict` — **171/171**.
- Not re-run here (pre-push/CI gates per policy, or architect-stage-covered):
  full `just test`, `just coverage`, `just smoke` (the architect stage ran it
  green; this review's one code fix is unit-tested), e2e.

## Findings

### F1 (fixed) — the artifact gate followed a symlink at the artifact path

`verify_facts` computed `exists` with `Path::is_file()`, which **follows the
final symlink**. A worker can commit a _symlink_ at the artifact path
(`.thegn/pipeline/<ISSUE>/<stage>/<row>.md → /home/blake/.aws/credentials`):
the link itself is in the index, so `tracked` is true, and `is_file()` on the
target made `exists` true — the `set-status done` gate **passed**, and the
Lead's skill then `cat`s the artifact path and reads the symlink _target_
(arbitrary local file read into the supervising agent's context). A hostile or
compromised worker should not be able to satisfy the handoff gate with anything
but a regular file.

- **Fix:** `exists` now uses `symlink_metadata().is_ok_and(|m| m.is_file())`
  (final component must be a regular file); a committed symlink reads as
  `exists=false` / "does not exist", with `--force` the deliberate override.
  Intermediate-component symlinks are held by the `tracked` check (`git
ls-files` matches index paths; paths under a linked directory are not in the
  index) — noted in the code comment.
- **Test:** `the_done_gate_refuses_a_symlinked_artifact_even_when_committed`
  (commits the link _without_ `commit_artifact`, whose `fs::write` would write
  through the symlink); the test fails on the pre-fix code.
- Commit `ff61a179` `fix(the-76): the artifact gate refuses a symlinked
artifact, not its target (review)`.

### F2 (not a bug — verified) — path traversal and git-arg injection in the gate

The row's `artifact_path` column is caller-supplied (`dispatch put --artifact`),
not thegn-generated. Checked: an **absolute** artifact path makes `wt.join`
escape the worktree for the `exists` probe (trivial existence oracle for a
local caller), but `tracked` runs `git ls-files --error-unmatch -- <path>`
_inside_ the worktree with `--` separation — an absolute or `..` path names no
index entry, so `ok = exists && tracked` cannot be reached from outside the
repo. No option injection (everything after `--`). The done-gate holds; only
the informational `verify` line can report `exists=yes` for an out-of-tree
path, which a local caller can already learn. No action.

### F3 (verified) — prompt rendering is injection-safe

Issue bodies (hostile text: literal braces, `{{`, unclosed placeholders,
GraphQL) are substituted **verbatim and never re-parsed** — the engine walks
`parse(template)` once and pushes values as data. Pinned by
`a_value_full_of_braces_is_never_reparsed` (incl. through the built-in Issue
prompt) and `braces_in_a_value_cannot_inject_a_placeholder`; the dispatch
prompt crosses the control socket as JSON and reaches the harness as argv, no
shell anywhere. The rendered prompt is also never written to a path — the
artifact path is thegn-computed from whitelist-sanitized components
(`[A-Za-z0-9._-]`, runs collapsed, edges trimmed, empty → fallback), so
`..`, `/`, `\` and control characters cannot survive into the path.

### F4 (verified, accepted deviation) — `dispatch wait` timeout and DB

The addendum asks for "a hard timeout" and "never hold the DB". `--timeout
<ms>` exists and composes the daemon's `sessions.wait` deadline (exit 2,
`{"matched":false}`); omitting it waits forever — that is the **documented,
spec'd composition** (`cli/spec.md`, help pages, same semantics as
`session wait`), and a supervisor legitimately blocks for an hour on a coder
stage. A daemon death wakes the wait (socket error → `gone`), so there is no
unbounded hang past daemon liveness. The SQLite handle is open during the
block but **idle** — no transaction, no journal/WAL lock — and the selection
errors are answered from the roster _before_ connecting. Accepted as designed;
no change.

### F5 (accepted residuals, per design D5 and the lane's own reports)

- Row-before-session ordering (D5): a crash between `put` and a successful
  open leaves a visible `queued` row (re-drivable) — deliberate. Two smaller
  windows are absorbed by the same doctrine: a crash between a _successful_
  open and the stamp leaves a live agent behind a `queued` row (Lead would
  re-drive and spawn a second worker — recoverable, sub-millisecond window,
  `session list --live` + `dispatch list` reveal the mismatch); stamp→running
  is two statements, not one transaction (same shape, self-correcting via
  `set-status`). Not blocking; a future wrap of the two UPDATEs in one
  transaction would be a trivial hardening.
- A genuinely live stage dispatch with a real agent binary remains unexercised
  (no agent CLI in the replay env) — the architect stage's residual risk note
  stands: the first real `/pipeline` run should watch the first dispatch's
  argv/env (`THEGN_LOG=thegn::agent=debug`).
- The chunk-3 "daemon-backed close/liveness" pass was a manual live replay,
  not a smoke.sh section (nothing in smoke starts a daemon — pre-existing
  shape). `session close`/`--live` happy paths are unit-tested (liveness token
  formatting, idempotent close via tombstone) but only daemon-free offline
  checks live in smoke. Acceptable; noting the gap honestly.

### F6 (note) — unsigned commits

The merge (`6d780b31`), this review's fix, and the verdict commit are unsigned:
gpg-agent pinentry times out in this non-interactive session (same as the
chunk-3 and architect-review commits, documented in their verdicts). Signed
coverage is not a gate; amend/re-sign if it ever becomes one.

## Frame-affecting changes / e2e

THE-76's own diff touches no chrome or frame path (`cmd/`, `thegn-core`,
docs, smoke, openspec) — **no snapshot re-recording is owed by this lane**.
The merge does bring main's THE-64 sidebar visual changes, whose baseline debt
(`sidebar__*`, `panel_*`, `themes__*`, …) is already documented in main's
CHANGELOG entry — that re-record remains owed, inherited, not THE-76's.

## Checklist against the lane docs

- Lead addendum merge: done (`6d780b31`), full diff reviewed.
- Chunk-1 "Unverified": heavy gates not run (pre-push will); `let _ =` count
  verified by the ignored-result ratchet; `with_fresh_registry`/CLI seeder
  deletions confirmed in the tree (the merge kept main's single-seeder and
  `config_source` paths).
- Chunk-2 "Unverified": smoke now validated by this stage's gates + architect
  run; tombstone-TTL expiry as the gone-wake trigger accepted (the unknown-
  session daemon-error branch is the same path; a 10-minute CI wait is not
  proportionate).
- Chunk-3 "Unverified": boot-`--set` override survival now carried by main's
  `config_source::install/fresh` (strictly better than the deleted
  `with_fresh_registry`); the `agent_open::resolve` comment documents the
  freshness contract at the one place a future edit would look.
- Every verb projects `capability::CATALOG` (`cli_control_verbs_cover_catalog`
  green; `dispatches.verify`/`dispatches.wait` declared CLI-only, no
  SURFACE_GAPS excuse), is in `docs/help/cli.md` + `docs/help/
daemon-and-sessions.md`, and is smoke-checked (daemon-free sections).
- Completion-catalog classification (`908cafb7`) intact post-merge: `DispatchRow`
  and `Freeform` reserved sources carry on-the-record justifications.

## Commits from this review

- `6d780b31` — merge of `main` (lead addendum, CHANGELOG conflict resolved).
- `ff61a179` — `fix(the-76): the artifact gate refuses a symlinked artifact,
not its target (review)` (F1 + test).
- This verdict: `docs(the-76): security/test/bug review verdict`.
