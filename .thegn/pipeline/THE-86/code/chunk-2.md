# Chunk 2 — Transport-error retry for headless harnesses

Design: `.thegn/pipeline/THE-86/architect/design.md` §2. Serial order **1 → 2 → 3**;
depends on chunk 1's `cmd/session.rs` factoring (this chunk moves
`IssueFacts`/`stage_task_vars` into the new `stage_prompt.rs` and must not
collide with chunk 1's additions). Adds **db migration v59** — chunk 3's v60
follows this.

## Files touched (exact paths)

| File                                             | Change                                                                                                                                                                |
| ------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `crates/thegn-core/src/pipeline_exit.rs`         | **NEW** — pure classifier + retry decision + signature defaults                                                                                                       |
| `crates/thegn-core/src/lib.rs`                   | `pub mod pipeline_exit;`                                                                                                                                              |
| `crates/thegn-core/src/harness.rs`               | `HarnessCaps::CONTINUE` (bit 32), `continue_command()` optional op, impls for Claude (`claude --continue`) and Pi (`pi --continue`); `names()` table                  |
| `crates/thegn-core/src/config_pipeline.rs`       | `TransportRetry` struct + `Pipeline.transport_retry` + validation                                                                                                     |
| `crates/thegn-core/src/db.rs`                    | `SCHEMA_VERSION` 58 → 59                                                                                                                                              |
| `crates/thegn-core/src/db_migrate.rs`            | migration: `ALTER TABLE agent_dispatches ADD COLUMN note TEXT`; ladder test                                                                                           |
| `crates/thegn-core/src/db_notification.rs`       | `DISPATCH_COLS` + `map_dispatch` + `stamp_dispatch_note(id, note)`                                                                                                    |
| `crates/thegn-core/src/issue.rs`                 | `AgentDispatch.note: Option<String>` (`#[serde(default)]`)                                                                                                            |
| `config/config.toml.example`                     | documented `[pipeline.transport_retry]` block in the pipeline section (~line 1570)                                                                                    |
| `crates/thegn-host/src/stage_prompt.rs`          | **NEW** — `IssueFacts`, `stage_task_vars`, `render_stage` moved verbatim from `cmd/session.rs` (`cmd/session.rs` re-imports; `use` sites updated, no behavior change) |
| `crates/thegn-svc/src/control/mod.rs`            | `AgentLaunch.continue_last: bool` (`#[serde(default)]`); fix every struct literal                                                                                     |
| `crates/thegn-host/src/daemon/agent_open.rs`     | `command_for` handles `continue_last` via `continue_command()` (prompt rides as opening message, same as resume, `agent_open.rs:155-160`)                             |
| `crates/thegn-host/src/daemon/pipeline_retry.rs` | **NEW** — the observer task                                                                                                                                           |
| `crates/thegn-host/src/daemon/mod.rs`            | spawn the task beside `lease_loop` (~line 346): `pipeline_retry::spawn(svc.clone(), events.subscribe());`                                                             |
| `crates/thegn-host/src/daemon/service.rs`        | expose `pub(crate) fn tombstone(&self, id) -> Option<Tombstone>` (read under the tombs lock; used by the observer)                                                    |
| `crates/thegn-host/src/cmd/dispatch.rs`          | `dispatch list`: JSON gains `note`; human table gains a trailing truncated note column                                                                                |
| `docs/cli.md`                                    | one line: headless transport retry + the `note` field                                                                                                                 |
| `test/smoke.sh`                                  | config-acceptance check: append a `[pipeline.transport_retry]` override + `thegn config validate` passes                                                              |

`AgentLaunch` literal sites to fix (grep `AgentLaunch {`):
`cmd/session.rs` ×2 (plain open ~:434, stage dispatch ~:846), wizard, daemon
inbox/tests, `agent_open.rs` tests — compiler will list them; all get
`continue_last: false` except where the observer sets true.

## Approach

1. **Pure core** (`pipeline_exit.rs`, header restates the no-I/O doctrine):

   ```rust
   pub enum ExitClass { Transport { signature: String }, Limit { signature: String } }
   pub struct ExitSignatures { pub transport: Vec<String>, pub limit: Vec<String> }
   pub fn classify(failed: bool, screen: &str, sig: &ExitSignatures) -> Option<ExitClass>;
   pub enum RetryDecision { Retry { attempt: u32, delay_ms: u64 }, Park { note: String }, Exhausted { note: String } }
   pub fn decide(class: &ExitClass, attempt: u32, max_attempts: u32, base_backoff_ms: u64) -> RetryDecision;
   pub const DEFAULT_TRANSPORT_SIGNATURES: &[&str];  // connection error., connection/network/timeout phrases,
                                                     // overloaded_error, 500/502/503/529, bad gateway, service unavailable
   pub const DEFAULT_LIMIT_SIGNATURES: &[&str];      // weekly limit, rate limit, usage limit, limit reached, credit/billing
   pub const MAX_BACKOFF_MS: u64 = 60_000;
   ```

   Substring, case-insensitive; transport tested before limit; first match
   wins; `failed == false` → `None`. `decide`: attempt ≤ max & Transport →
   `Retry { delay = min(base * 2^(attempt-1), MAX_BACKOFF_MS) }`; Transport over
   max → `Exhausted`; Limit → `Park` always. Notes carry the signature and
   attempt counts (exact formats pinned in tests so `dispatch list` output is
   stable).

2. **Harness seam**: follow the RESUME pattern (`harness.rs:47-48`, `:244-246`):
   `CONTINUE` bit in `HarnessCaps` (32), `names()` entry "continue", default
   `continue_command(&self) -> Option<String> { None }`; Claude →
   `Some("claude --continue".into())`, Pi → `Some("pi --continue".into())`.
   The existing `caps_agree_with_ops` loop (`harness.rs:690-710`) must be
   extended to pair the bit with the op — it fails the build otherwise, by
   design.

3. **Config** (`config_pipeline.rs`):

   ```rust
   #[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
   #[serde(default)]
   pub struct TransportRetry {
       pub enabled: bool,                          // true
       pub max_attempts: u32,                      // 3
       pub backoff_ms: u64,                        // 2000
       pub transport_signatures: Vec<String>,      // serde(default = core consts)
       pub limit_signatures: Vec<String>,          // ditto
   }
   ```

   `Pipeline` gains `pub transport_retry: TransportRetry`. Validation
   (`validate_pipeline`): each signature non-empty; `max_attempts >= 1` when
   `enabled`. Depth-2 keys ⇒ **no env knobs needed** (`env_overlay_coverage.rs`
   covers depth ≤ 1 only); the hm module does not render `[pipeline]` ⇒ no nix
   edit. `config.toml.example` block documents every key as commented defaults.

4. **DB v59**: `note TEXT` via ALTER (the `:536-546` idiom, ladder-tested like
   the v55→current test at `db_migrate.rs:638-710`); `DISPATCH_COLS`/`map_dispatch`
   updated together (compile failure enforces the pair); `stamp_dispatch_note`
   writes the column only.

5. **Observer** (`daemon/pipeline_retry.rs`):
   `pub(crate) fn spawn(svc: Arc<DaemonService>, rx: broadcast::Receiver<Arc<EventFrame>>)`.
   Loop on `SessionExit { session, code }`:
   - skip unless `code` is `Some(c)` && `c != 0`;
   - skip when the session has any attached client at exit (adopted pane / human
     watching — the pane path owns those rows); read it from
     `svc.list_sessions()`-equivalent state or the session entry's subscriber
     count, whichever `service.rs` exposes without new locking games (design
     leaves the exact accessor to the coder; it must be a lock-scope read, not
     I/O);
   - tombstone → flatten `final_screen` bytes with the existing
     `crate::cmd::session::snapshot_text(rows, cols, &ansi)` (already
     `pub(crate)`);
   - `db.dispatch_by_session(sid)` — **new db fn**: full row by `session_id`,
     newest first; skip if none or `status.is_terminal()`;
   - `classify(failed=true, screen, &ExitSignatures::from(&svc.config.pipeline.transport_retry))`
     (boot snapshot is fine; `svc.open` re-resolves config per launch);
   - `decide(...)` against the in-memory `Mutex<HashMap<String, u32>>` of
     attempts (keyed by **roster row id**, surviving session-id changes);
   - `Limit` or `Exhausted` or `Retry`: `stamp_dispatch_note(row.id, note)` +
     `update_dispatch_status(row.id, WaitingHuman)` — **never Done/Failed**;
   - `Retry`: `tokio::time::sleep(delay)` (task-local; the daemon has no UI
     loop), then relaunch: harness via the moved `harness_for_agent`
     (make it `pub(crate)` in `agent_open.rs`); if `caps().contains(CONTINUE)` →
     `OpenSpec { agent: AgentLaunch { agent: row.agent_name, prompt:
RETRY_NUDGE (const: "You were interrupted by a transport error; continue
where you left off."), headless: Some(true), continue_last: true, stage:
row.stage.clone(), .. } , worktree: Some(row.worktree_path), adopt: false,
.. }`; else re-render the stage prompt via `stage_prompt::render_stage`
     (issue facts via `svc.issues_get`, branch via the db worktree registry)
     and launch cold with that prompt;
   - success: `stamp_dispatch_run(row.id, &info.id, artifact_same)` +
     `Running`; failure: append `relaunch failed: {err}` to the note, row stays
     `waiting_human`.
     All DB work via the existing `svc.with_db(spawn_blocking)` idiom; the task
     is event-driven with zero timers while idle.

6. **Projection**: no new capability verb (the observer is daemon-internal);
   `catalog_tests` stay green untouched. `dispatch list` note column as in the
   file table. Smoke: config acceptance only (a live retry needs a real
   harness; the classifier and decision are core-table-tested instead).

## Tests

```sh
just quick thegn-core
just quick thegn-host
just quick thegn-svc
cargo nextest run -p thegn-core pipeline_exit harness config_pipeline
cargo nextest run -p thegn-core db_tests::migration
cargo nextest run -p thegn-host agent_open
cargo nextest run -p thegn-svc control_schema
```

- **core**: classification table (transport/limit/none × case × first-match ×
  exit-0 gate); `decide` table (backoff doubling, cap, exhaustion, park);
  `caps_agree_with_ops` extended for CONTINUE; claude/pi continue command
  shapes (and that no id is interpolated); config validation
  (empty signature rejected, `max_attempts = 0` with enabled rejected, defaults
  parse); migration ladder 58→59 preserves rows; note round-trips through
  `put/get/list`.
- **host**: `command_for` with `continue_last` (claude → `claude --continue`,
  prompt appended quoted; aider → error naming the agent); `stage_prompt.rs`
  move keeps `cmd/session.rs` render tests green unchanged.
- **svc**: `control_schema` green (serde-default field, wire back-compat).

## Done criteria

- [ ] `just quick thegn-core / thegn-host / thegn-svc` clean.
- [ ] All scoped nextest filters above green.
- [ ] `thegn config validate` accepts the example (and the smoke override).
- [ ] `dispatch list --json` rows carry `note`; human table shows it truncated.
- [ ] A stub test (daemon `service.rs` tests module) proves the observer stamps
      `waiting_human` + note on a synthetic exit and never `done`/`failed`.
- [ ] Commit subject EXACTLY:
      `feat(pipeline): transport-error retry for headless dispatches (THE-86)`
