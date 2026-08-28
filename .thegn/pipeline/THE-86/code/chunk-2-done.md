# Chunk 2 done — Transport-error retry for headless harnesses (THE-86)

Coder stage complete. Commits on `tg/the-86-pipeline-v3`, in order (final code
commit carries the exact required subject):

| Commit     | Subject                                                                                       | Files                                                                                                                                                         |
| ---------- | --------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `c0ebde9e` | feat(pipeline): pure exit classifier + harness CONTINUE cap (THE-86 chunk 2)                  | `pipeline_exit.rs` (new), `harness.rs`, `lib.rs`                                                                                                              |
| `2c727565` | feat(pipeline): [pipeline.transport_retry] config surface (THE-86 chunk 2)                    | `config_pipeline.rs`, `config/config.toml.example`                                                                                                            |
| `b8121cb7` | feat(pipeline): db v59 note column + dispatch_by_session/stamp_dispatch_note (THE-86 chunk 2) | `db.rs`, `db_migrate.rs`, `db_notification.rs`, `store/notification.rs`, `issue.rs`, `db_tests.rs` + host test-literal fixes (compiler-enforced `note: None`) |
| `845d9476` | refactor(pipeline): move stage-prompt composition to stage_prompt.rs (THE-86 chunk 2)         | `stage_prompt.rs` (new), `cmd/session.rs`, `main.rs`                                                                                                          |
| `181a5cdc` | feat(pipeline): AgentLaunch.continue_last + command_for continue form (THE-86 chunk 2)        | `thegn-svc/control/mod.rs`, `daemon/agent_open.rs`, `cmd/mcp.rs`, `docs/api/control-v1.json`, `cmd/session.rs`                                                |
| `9b0a448c` | feat(pipeline): daemon transport-retry observer (THE-86 chunk 2)                              | `daemon/pipeline_retry.rs` (new), `daemon/service.rs`, `daemon/tombstone.rs`, `daemon/session.rs`, `daemon/mod.rs`                                            |
| `c545afef` | **feat(pipeline): transport-error retry for headless dispatches (THE-86)**                    | `cmd/dispatch.rs`, `docs/cli.md`, `test/smoke.sh`                                                                                                             |

## What was implemented (per the chunk spec)

- **`pipeline_exit.rs` (pure core, no I/O)** — `ExitClass` (Transport/Limit),
  `ExitSignatures` (+ `From<&TransportRetry>`, `defaults()`), `classify`
  (substring, case-insensitive, transport-before-limit, first match wins,
  `failed == false ⇒ None`), `decide` (backoff `base * 2^(n-1)` capped at
  `MAX_BACKOFF_MS = 60_000`; Transport over max ⇒ `Exhausted`; Limit ⇒ `Park`
  always), `retry_note` (`transport: <sig> (attempt N/M)` — format pinned by
  test), `RETRY_NUDGE`, default signature lists (transport: connection/timeout
  phrases, `overloaded_error`, bad gateway, service unavailable, HTTP
  429/5xx-by-name; limit: weekly/rate/usage limits, quota/credit/billing).
- **Harness seam** — `HarnessCaps::CONTINUE` (bit 32), `names()` entry
  "continue", default `continue_command() -> Option<String> { None }`; Claude →
  `claude --continue`, Pi → `pi --continue` (pi's first continue form);
  `caps_agree_with_ops` extended for the new bit⇔op pair (id-free by design —
  no session id is interpolated anywhere).
- **Config** — `TransportRetry` (enabled=true, max_attempts=3, backoff_ms=2000,
  signature lists defaulting to the core consts; overrides REPLACE) +
  `Pipeline.transport_retry`; `validate_pipeline` rejects empty signatures and
  `max_attempts = 0` while enabled. Depth-2 table ⇒ no env knob; no hm module
  change. `config.toml.example` documents every key as commented defaults.
- **DB v59** — `SCHEMA_VERSION` 58→59; idempotent `ALTER TABLE agent_dispatches
ADD COLUMN note TEXT` (ladder-tested: a v58 DB gains the column with rows
  intact); `DISPATCH_COLS`/`map_dispatch` updated together; trait +
  impl of `stamp_dispatch_note(id, note)` (replace semantics; caller composes
  appends) and `dispatch_by_session(sid)` (newest stamp wins);
  `AgentDispatch.note: Option<String>` `#[serde(default)]` (wire back-compat).
- **`stage_prompt.rs` (new)** — `IssueFacts`, `stage_task_vars` moved verbatim
  from `cmd/session.rs` (+ `number_only`, `needs_tracker` helpers, and
  `render_stage` — the render + empty-prompt refusal extracted from
  `open_stage`'s render step so the CLI and the daemon render identically).
  `cmd/session.rs` re-imports; all use sites unchanged; the module's render
  tests pass unchanged (18/18).
- **Control API** — `AgentLaunch.continue_last: bool` (`#[serde(default)]`);
  all struct literals fixed (`continue_last: false` everywhere but the
  observer); `docs/api/control-v1.json` regenerated (additive, defaulted).
- **`agent_open.rs`** — `command_for` resolves `continue_last` through
  `continue_command()` with the prompt riding as the opening message (same
  shape as resume, no id to validate); refusal names the agent; no-continue
  harnesses relaunch cold. `harness_for_agent` is now `pub(crate)`.
- **Observer (`daemon/pipeline_retry.rs`)** — event-driven on
  `svc.events.subscribe()` (zero timers while idle, backoff sleep is the only
  timer): nonzero exits only → tombstone (one lock-scope read) → skip when
  anyone was attached at exit → row by session id → skip terminal rows →
  classify → `decide` against an in-task attempt map keyed by ROW id →
  **every** outcome stamps `waiting_human` + note; Retry additionally sleeps
  the backoff and relaunches via `svc.open(OpenSpec)` (CONTINUE harness ⇒
  nudge prompt + `continue_last: true`; else cold + re-rendered stage prompt),
  success re-stamps the SAME row (`stamp_dispatch_run` + `Running`), failure
  appends `relaunch failed: …` under the note. All DB work via
  `svc.with_db(spawn_blocking)`; `with_db` is now `pub(crate)`.
- **Tombstone carries `attached` at death** — recorded by `build_tombstone`
  from `LiveMeta` under the same lock it already took (the actor buries the
  tombstone BEFORE the exit reaches the feed, so the observer's single read is
  race-free; `Tombstone::info()` still reports 0 for the listing, which is a
  "now" answer).
- **`daemon/mod.rs`** — `pipeline_retry::spawn(svc.clone(), svc.events.subscribe())`
  beside the lease loop.
- **`dispatch list`** — JSON rows carry `note` (serde, verified live: fresh row
  → `null`); human table gains a trailing collapsed/truncated (32 chars + `…`)
  note column, `-` when absent (tested).
- **`docs/cli.md`** — one line: retry note on `dispatch list` +
  `[pipeline.transport_retry]`.
- **`test/smoke.sh`** — config-acceptance check: isolated dir with a
  `[pipeline.transport_retry]` override (incl. replaced signature lists) passes
  `thegn config validate`.

## Verification (scoped, per dev-loop policy — no full-workspace gates)

- `just quick thegn-core` — clean. `just quick thegn-svc` — clean.
  `just quick thegn-host` — clean (after chunk 3's concurrent literal sweep
  settled; see Shared-worktree notes).
- `cargo nextest run -p thegn-core pipeline_exit harness config_pipeline` —
  58/58 (classification table × case × first-match × exit-0 gate; `decide`
  table incl. doubling/cap/exhaustion/park; `caps_agree_with_ops` extended;
  claude/pi continue shapes + no-id; config defaults/overrides/rejections).
- `cargo nextest run -p thegn-core db_migrate` — 11/11 (incl. the new
  `pre_v59_db_gains_the_dispatch_note_column_without_resetting_anything` and
  the note round-trip + `dispatch_by_session` test; note: the chunk's filter
  string `db_tests::migration` has no such module — the ladder tests live in
  `db_migrate.rs::tests`, which is what was run).
- `cargo nextest run -p thegn-core agent_dispatch_roundtrip` — 2/2.
- `cargo nextest run -p thegn-host agent_open` — 14/14 (incl. the two new
  continue tests: claude/pi continue forms with prompt quoted; aider refuses
  with the agent named).
- `cargo nextest run -p thegn-host cmd::dispatch` — 8/8;
  `cmd::session` — 18/18 (the move kept them green unchanged);
  `daemon::tombstone` — 4/4; observer stub — 1/1.
- `cargo test -p thegn-svc --test control_schema` — green after regeneration
  (additive, defaulted field).
- `thegn config validate` accepts `config.toml.example` **and** the smoke
  override (run live against an isolated `XDG_CONFIG_HOME`).
- `dispatch list --json` / human table verified live against an isolated
  `XDG_STATE_HOME` (note field present; trailing `-` column on a fresh row).
- Observer stub test (in `daemon/service.rs` tests module): synthetic nonzero
  exits through the REAL path (real tombstone + real in-memory db + pure core,
  no PTY/harness) stamp `waiting_human` + note for both a transport Retry and
  a Limit Park, and never `done`/`failed`. The aider transport row also
  exercises the relaunch-failure append path (cold re-render refuses the
  unconfigured stage; note gains `relaunch failed: …`).

## Shared-worktree notes (concurrent coders on chunks 1/3)

- `lib.rs` staged alone via a single-line `git apply --cached` (chunk 1/3 lines
  never entered my commits); `cmd/session.rs`, `cmd/dispatch.rs`, `docs/cli.md`,
  `test/smoke.sh` staged hunk-by-hunk for the same reason. Chunk 1's commit
  `9aa9d9c6` includes the `note: None`/`continue_last: false` literal lines the
  compiler forced into their new code (required for compilation).
- One mid-flight collision (both chunk 1's coder and I added
  `continue_last: false` to the same literals within minutes) was resolved by
  deduplicating — final literals carry one field + one comment each.
- `render_stage` did not exist at chunk start (it is chunk 1's extraction per
  design §1.2); I extracted it verbatim from `open_stage`'s committed render
  step myself. Chunk 1's uncommitted `resume_work` render still calls
  `render_prompt` inline rather than `render_stage` — behaviorally identical,
  flagged here so the review stage can fold it onto the shared helper if
  desired.

## Unverified

- **Live relaunch with a real harness** (`svc.open` spawning `claude
--continue` / `pi --continue`, and the cold re-render relaunch against a
  configured pipeline): needs a real agent; per the chunk spec the classifier
  and decision are core-table-tested instead, and the stub test covers the
  stamp/append paths with the relaunch entry point reached but refused before
  any spawn. The `Retry` arm's success path (`stamp_dispatch_run` + `Running`
  on the relaunched session) is code-reviewed but not executed end-to-end.
- **`just quick thegn-host` at the exact final commit `c545afef`** — the shared
  worktree was mid-sweep by chunk 3 (their v60 `chunk_path` literals) when the
  heavy checks would have run; it passed cleanly once their sweep settled
  (against a tree that includes their uncommitted-then-committed work). My
  commits were each compile-verified with `cargo check -p thegn-host --bins`
  and scoped nextest filters at commit time.
- **e2e** — not run (per Lead addendum).
- **`just test` / `just ci` / coverage** — deliberately not run (Lead addendum:
  no >10-minute builds; pre-push gate owns them).
