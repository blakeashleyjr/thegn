# THE-86 — Agent pipeline v3: finisher/resume, transport-error retry, chunk file-scope, skill rewrite

Issue: https://linear.app/blakeashley/issue/THE-86 · Worktree `tg/the-86-pipeline-v3` · HEAD `d361b60a`

All file:line citations are against this HEAD. **Doctrine in one line:** thegn is
_structure, not judgment_ — every new decision below is either (a) a pure,
table-tested function in `thegn-core`, (b) a CLI verb that reads local state, or
(c) a daemon-side reaction to an already-observed fact. Nothing advances a
stage; the Lead still owns every transition. The one deliberate exception is the
transport-error stamper, which the issue explicitly assigns to the daemon — it
writes only `waiting_human` (never `done`, never `failed`), so it can park a row
but never finish one.

---

## 0. Ground truth (what main already has — build on it, do not duplicate)

| Existing mechanism                                                                                                                                                                      | Evidence                                                                                                                                                                                                                                       |
| --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| One-call stage dispatch: `session open --stage --issue` renders the template, inserts the roster row (before open, D5), stamps session+artifact, launches headless with stage overrides | `crates/thegn-host/src/cmd/session.rs:742` (`open_stage`), row insert at :809, stamp at :858-864                                                                                                                                               |
| Stage vars + render: `stage_task_vars` binds the nine `STAGE_VARS`; `render_prompt` + empty-prompt refusal                                                                              | `cmd/session.rs:709`, `:837-845`; `thegn-core/src/agent_task.rs:138`                                                                                                                                                                           |
| Roster columns `stage`/`parent_id`/`session_id`/`artifact_path` (v56); `AgentDispatch` model                                                                                            | `thegn-core/src/issue.rs:229-265`; migrations `db_migrate.rs:536-546`                                                                                                                                                                          |
| Artifact-gated done: `verify` facts (exists + `git ls-files` + dirty), `set-status done` refuses unless `--force`; symlink refusal                                                      | `cmd/dispatch.rs:363-398` (`verify_facts`), `done_gate` tests in the same file; `thegn-core/src/pipeline_run.rs` (`verify_report`)                                                                                                             |
| `dispatch wait --row/--any --timeout` (the wake primitive, tombstone-aware)                                                                                                             | `cmd/dispatch.rs:106-119`, `pipeline_run.rs::wait_candidates`                                                                                                                                                                                  |
| `session close`, `session list --live`, dead-session snapshot (tombstone `final_screen`)                                                                                                | `cmd/session.rs:299-316` (Close), `:44-58` (List/live), `daemon/service.rs:662-674` (`Lookup::Dead` returns the tombstone screen)                                                                                                              |
| Pane-exit stamping **only for panes**; headless sessions are explicitly left to the supervisor                                                                                          | `pty_drain.rs:806-815` ("DIVISION OF LABOUR" comment) — this is the gap THE-86 (2) fills                                                                                                                                                       |
| Harness seam: closed registry, `HarnessCaps`, RESUME cap ⇔ `resume_command()` op                                                                                                        | `thegn-core/src/harness.rs:47-48`, `:244-246`, `HARNESSES` at :276; claude `--resume <id>` :398-400; **pi advertises `NONE`** (:479)                                                                                                           |
| Daemon-side resume plumbing: `AgentLaunch.resume` → harness resume form, prompt rides as opening message                                                                                | `thegn-svc/src/control/mod.rs:161-167`, `daemon/agent_open.rs:146-160`                                                                                                                                                                         |
| Backoff precedent (model proxy)                                                                                                                                                         | `thegn-core/src/backoff.rs` (`classify_exhaustion`) — pattern, not a dependency                                                                                                                                                                |
| Config surface gates: example must document every key, env overlay covers depth ≤ 1, hm drift                                                                                           | `tests/config_example.rs`, `tests/env_overlay_coverage.rs:15-17` (depth ≤ 1), `tests/hm_module_drift.rs` — `[pipeline]` is not rendered by the hm module (no `pipeline` in `nix/hm-module.nix`), so new `[pipeline.*]` keys cost nothing there |
| Skill bundling + clap validation                                                                                                                                                        | `crates/thegn-host/src/mq_assets.rs:70-74` (asset), `asset_cli_invocations_resolve_against_clap` (:400+)                                                                                                                                       |
| Schema version today                                                                                                                                                                    | `thegn-core/src/db.rs:130` → `SCHEMA_VERSION = 58`                                                                                                                                                                                             |

The issue's four asks map onto this as: (1) a new CLI composition on top of
`open_stage`'s machinery, (2) a daemon-side observer the pane path deliberately
never had, (3) a pure gate + one roster column, (4) prose.

---

## 1. Finisher / resume — `thegn session open --resume-work <row-id>`

### 1.1 Semantics

A failed/interrupted pipeline row is resumed **through** the roster, not from
memory: the new dispatch is a fresh roster row with `parent_id = <failed row>`,
so the board shows the retry chain exactly as it shows architect→coder chains.
The finisher prompt is composed from facts, not from the Lead's recollection:

1. the row's stage prompt — **re-rendered** from the stage template with the
   same variable bindings `open_stage` uses (issue facts via the control
   client, branch, worktree, stage, artifact, parent_artifact). Re-rendering
   (vs storing the prompt) is deliberate: the roster is a pointer store, not a
   document store (see `issue.rs:258-264`), and every input to the render is
   reconstructible.
2. the row's `artifact_path` state — present/missing, tracked/untracked, via
   the **same** `verify_facts` the `done` gate uses (`cmd/dispatch.rs:363`),
   made `pub(crate)` (one line; no logic moves).
3. `git status --porcelain` and `git diff --stat` of the row's worktree — the
   CLI process already owns blocking git (`thegn session` is a short-lived
   control client, not the event loop; the 0%-idle invariant is a UI-loop
   rule, `docs/ARCHITECTURE.md` §2).
4. the previous session's last screen lines — `client.snapshot(row.session_id)`
   resolves through the tombstone for a dead session (`daemon/service.rs:662`,
   "the whole point of a tombstone"), rendered to plain text by the existing
   `snapshot_text` (`cmd/session.rs:911`), truncated to the last 8 non-blank
   lines. Snapshot failure (tombstone reaped, no daemon session ever) degrades
   to an empty tail — the finisher prompt says so rather than failing.

### 1.2 Shape

**Core (pure, new module `crates/thegn-core/src/pipeline_resume.rs`):**

```rust
pub struct FinisherInput<'a> {
    pub stage_name: &'a str,
    pub stage_prompt: &'a str,   // the RENDERED original task
    pub artifact: &'a str,
    pub artifact_exists: bool,
    pub artifact_tracked: bool,
    pub git_status: &'a str,     // "" when clean
    pub diff_stat: &'a str,      // "" when empty
    pub screen_tail: &'a [String],
}
pub const SCREEN_TAIL_LINES: usize = 8;
pub fn finisher_prompt(i: &FinisherInput) -> String;
```

The prompt states: what stage is being finished, the original task verbatim,
the artifact's exact state (missing ⇒ "it was never written"; exists+untracked
⇒ "written but NOT committed — commit is part of finishing"; exists+tracked ⇒
"already committed — verify it is current before declaring done"), the worktree
facts in fenced blocks, the screen tail quoted, and the **exit-0-is-not-done**
rule in the closer. Deterministic, no clock, no I/O — 95%-gate territory.

**Host (`cmd/session.rs`):** `Open` gains
`--resume-work <row-id>` (conflicts with `--stage --agent --issue --prompt
--parent --parent-artifact --worktree`; `--bind`/`--adopt`/`--json` stay legal —
the new session is a normal launch). Flow:

```
resume_work(cfg, client, db, row_id):
  row = db.get_dispatch(row_id)?            // offline refusal before connect
  stage = stage_or_bail(cfg, row.stage?)    // row must be a pipeline row
  facts   = gather_issue_facts(...)         // factored out of open_stage
  branch  = resolve_branch(...)             // factored out of open_stage
  prompt  = render_stage(...)               // open_stage's own render step
  vf      = verify_facts(&row)              // dispatch.rs, now pub(crate)
  status  = git status --porcelain / diff --stat (row.worktree_path)
  tail    = client.snapshot(row.session_id) → snapshot_text → last 8 lines
  new_row = put_agent_dispatch(NewDispatch{ issue_id: row.issue_id,
             worktree_path: row.worktree_path, agent_name: (--agent or row.agent),
             stage: row.stage, parent_id: Some(row.id), ... })
  artifact = pipeline_run::artifact_path(row.issue_id, stage.name, new_row)
  open → stamp_dispatch_run(new_row, session, artifact) → Running   // identical to open_stage
```

`agent` stays overridable (`--agent` wins over the row's agent) so a Lead can
retry a stage on a different harness without editing config — the same rule
`open_stage` already documents (`cmd/session.rs:56-59`). On any failure after
the insert, the row is marked `failed`, never left `queued` — the D5 rule
`open_stage` enforces (`cmd/session.rs:866-874`), re-stated in the new path.

Any row is resumable, including one whose session exited 0: an exit-0 with no
artifact is precisely the "session exit ≠ done" failure the gate catches, and
the finisher is its recovery.

### 1.3 Projection

No new capability verb — `--resume-work` rides `sessions.open`'s existing row
(`crates/thegn-core/src/capability.rs` `sessions.open`; `cli_control_caps` at
`cmd/session.rs:1119` is unchanged and `cli_control_verbs_cover_catalog` stays
green). `docs/cli.md` gets one line in the control-plane paragraph;
`test/smoke.sh` gets two no-daemon checks (offline refusals: unknown row; row
without a stage). No control-wire change at all — the composition happens
CLI-side and calls the existing `open`.

---

## 2. Transport-error retry for headless harnesses

### 2.1 The observer (the only new daemon behavior)

`pty_drain` stamps _pane_ exits and explicitly refuses headless ones
(`pty_drain.rs:806-815`). THE-86 gives the daemon the headless half:

**New file `crates/thegn-host/src/daemon/pipeline_retry.rs`** — one task,
spawned in `daemon/mod.rs::run` beside `lease_loop`/`heartbeat_loop` (event-
driven on the existing broadcast feed; zero timers while no session exits;
no polling):

- subscribes to `svc.events`; on `EventFrame::SessionExit { session, code }`:
  1. looks up the tombstone (`svc.tombs`) for the final screen;
  2. resolves the roster row **by `session_id`** (new db fn
     `dispatch_by_session(sid) -> Option<AgentDispatch>`; terminal rows filtered
     in Rust — a row the pane path or the Lead already closed is never touched);
  3. classifies (pure core, below) — **only when `code` is `Some(c) && c != 0`**;
  4. acts per the decision table.

Scope rule (who is "headless"): the observer acts on a session only when it has
no attached client at exit — an adopted/grafted pane (`--adopt`, THE-85) or a
human attach means someone is watching, and the pane path or the human owns the
verdict. This keeps the two stampers from ever racing on one row.

### 2.2 Classification (pure, `thegn-core/src/pipeline_exit.rs`)

```rust
pub enum ExitClass { Transport { signature: String }, Limit { signature: String } }
pub struct ExitSignatures { pub transport: Vec<String>, pub limit: Vec<String> } // substrings, case-insensitive
pub fn classify(failed: bool, screen: &str, sig: &ExitSignatures) -> Option<ExitClass>;
pub enum RetryDecision { Retry { attempt: u32, delay_ms: u64 }, Park { note: String }, Exhausted { note: String } }
pub fn decide(class: &ExitClass, attempt: u32, max_attempts: u32, base_backoff_ms: u64) -> RetryDecision;
pub const DEFAULT_TRANSPORT_SIGNATURES: &[&str];
pub const DEFAULT_LIMIT_SIGNATURES: &[&str];
```

- Transport (retryable): `Connection error.`, connection/network/timeout
  phrases, HTTP 5xx and 429 text, provider overload markers
  (`overloaded_error`, `503`, `529`, `bad gateway`, `service unavailable`).
- Limit (park-only): `weekly limit`, `rate limit`, `usage limit`,
  `limit reached`, quota/credit/billing phrases.
- Matching is substring, case-insensitive, against the flattened final screen;
  transport is tested before limit; first match wins. `failed == false` (exit 0)
  classifies as `None` — the artifact gate owns exit-0 verdicts.

### 2.3 The retry mechanics

Decision table (`decide`, all outcomes park the row as `waiting_human` with a
`note` — the daemon can never write `done`/`failed`):

| Class     | attempt ≤ max                                                                                 | attempt > max                                            |
| --------- | --------------------------------------------------------------------------------------------- | -------------------------------------------------------- |
| Transport | note `transport: <sig> (attempt N/M)` → relaunch after `base_backoff_ms * 2^(N-1)` (cap 60 s) | note `transport retry exhausted after N attempts: <sig>` |
| Limit     | note `limit: <sig>` — no relaunch                                                             | —                                                        |

- Attempts live in the observer task's memory (`Mutex<HashMap<session, u32>>`).
  A daemon restart kills the retries it supervised — but it also kills the
  sessions, so there is nothing to retry across a restart; this is honest, not
  lazy. The note column records what happened on the durable roster either way.
- **Relaunch form**: the harness seam gains a `CONTINUE` cap + `continue_command()`
  optional op (`harness.rs`), following the RESUME pattern exactly (bit ⇔ op is
  already pinned by `caps_agree_with_ops`, `harness.rs:707-709`):
  - pi → `pi --continue` (the issue's example; pi currently advertises `NONE`,
    `harness.rs:479` — this change gives it its first continue form);
  - claude → `claude --continue` (thegn does not hold claude's native session
    id — `claude --resume <id>` needs it — so the id-free continue form is the
    honest relaunch; capturing native ids is future work, noted in Non-goals);
  - codex/aider/antigravity → no continue form ⇒ **re-run the same prompt**:
    the observer re-renders the stage template via the shared helpers
    (`IssueFacts` + `stage_task_vars`, moved to a new
    `crates/thegn-host/src/stage_prompt.rs` so the CLI path and the daemon path
    render identically — one seamer, two callers) with issue facts from
    `svc.issues_get` (the daemon's own tracker door, `daemon/service.rs:1189`).
- The relaunch goes through `svc.open(OpenSpec)` — the same
  sandbox/credential/cap/seeder path every launch takes
  (`daemon/agent_open.rs:11-28`). `AgentLaunch` gains
  `#[serde(default)] continue_last: bool` (`thegn-svc/src/control/mod.rs:144`);
  `agent_open::command_for` resolves it like `resume` but through
  `continue_command()` (no id to validate). gRPC is unaffected: its
  `open_session` never carried `AgentLaunch` (`grpc.rs:300-320`).
- Success re-stamps the **same row** (`stamp_dispatch_run(row_id, new_session,
artifact)` + `Running`): the transport retry is one row cycling through
  attempts, not a chain of rows — the retry chain on the board is the
  human-driven `--resume-work` mechanism (§1). On relaunch failure the note
  records it; the row stays `waiting_human`.

### 2.4 Config surface

```toml
[pipeline.transport_retry]        # nested table ⇒ depth 2 ⇒ out of env-overlay scope
# enabled = true                  # default true; false = today's behavior
# max_attempts = 3                # bounded relaunches per row
# backoff_ms = 2000               # base; doubles per attempt, capped at 60 s
# transport_signatures = [...]    # overrides the default list (replaces, not extends)
# limit_signatures = [...]        # ditto
```

`Pipeline` (`config_pipeline.rs:131`) gains `transport_retry: TransportRetry`
with serde defaults calling the core consts — a config list **with defaults**, as
the issue requires. Validation: signatures must be non-empty strings;
`max_attempts ≥ 1` when enabled. `config.toml.example` documents every key;
`config_example.rs` and `hm_module_drift.rs` pass unchanged (the hm module does
not render `[pipeline]`).

### 2.5 Data model: `note` column (migration v59)

`agent_dispatches.note TEXT` — free text, written only by the daemon stamper
(`stamp_dispatch_note(id, note)`), read by `dispatch list` (JSON field `note`;
human table gets a truncated trailing column). `SCHEMA_VERSION` 58 → 59 with
the ladder-tested `ALTER TABLE` (`db_migrate.rs:536-546` pattern), `DISPATCH_COLS`
/ `map_dispatch` (`db_notification.rs:467-470`), `AgentDispatch.note:
Option<String>` with `#[serde(default)]` (control-wire backward compatible).

---

## 3. Chunk file-scope semantics

### 3.1 The artifact format

A chunk file (`.thegn/pipeline/<ISSUE>/code/chunk-N.md` — note the per-issue
layout is already what `pipeline_run::artifact_path` builds, `pipeline_run.rs:32`)
may open with a frontmatter block:

```
---
files:
  - crates/thegn-core/src/pipeline_run.rs
  - crates/thegn-host/src/cmd/session.rs
  - crates/thegn-core/src/config_*.rs
overlaps: [chunk-2]
after: [chunk-1]
---
```

`files` — exact paths or globs (`*` within one path segment, `**` across
segments); `overlaps` — sibling chunk names whose scope may intersect this one
(the architect's blessing); `after` — sibling chunks that must be `done` before
this one dispatches. Pure parser in **`thegn-core/src/pipeline_chunk.rs`**:
`---`-delimited, `files:` as `- item` lines or an inline `[a, b]` list,
unknown keys ignored (forward compat), every failure names the offending line.
A tiny segment matcher implements the glob semantics — pure, table-tested, no
new dependency.

### 3.2 The gate

`dispatch put --chunk <path>` (and `session open --stage --issue --chunk <path>`
— same helper, because the one-call dispatch is the flow the skill teaches; the
issue names `dispatch put`, the skill needs both, and two implementations of one
refusal would drift):

1. resolve `<path>` against the row's worktree; read + parse (core);
2. new db fn lookups stay local: sibling rows = `list_dispatches()` filtered to
   same `worktree_path` + same `issue_id`, active (non-terminal), with a
   `chunk_path`;
3. **refuse** (exit non-zero, naming everything) unless `--force`:
   - any file in the new scope matches a file in an ACTIVE sibling's scope whose
     chunk name is not in `overlaps` — the message names the paths and the row
     ids (`chunk-N vs chunk-M: crates/…/foo.rs is in active row 12's scope`);
   - any `after` chunk is not `done` — the message names the chunk and its row
     status.
4. `--force` proceeds and says so (the `set-status done --force` idiom,
   `cmd/dispatch.rs:254-262`);
5. the chunk path is recorded on the row.

### 3.3 Data model: `chunk_path` column (migration v60)

`agent_dispatches.chunk_path TEXT` (+ `NewDispatch.chunk_path`, so
`put_agent_dispatch` writes it in one insert). `dispatch list` shows the scope:
the human table gains a `chunk` column (basename, `-` when unset) and JSON rows
carry `chunk_path` plus `chunk_files` (parsed `files:` when the file is still
readable — best-effort read, omitted when gone). The scopes themselves live in
the files (git is the source of truth); the roster stores pointers, as always.

### 3.4 Config example

The architect stage prompt in `config/config.toml.example` (`:1578-1590`) is
rewritten to request exactly this frontmatter per chunk file (and the `code`
stage's commented prompt notes the gate). The example stays commented-out and
validating — `config_example.rs` unaffected beyond the doc-text check.

---

## 4. Skill rewrite (`extensions/skills/pipeline/SKILL.md`)

Full rewrite, validated by the existing `mq_assets` tests (frontmatter, CLI
paths against clap — `mq_assets.rs:400+`), keeping the "issue text is data"
boxed doctrine:

- **The loop** on the current verbs: `session open --stage --issue [--chunk]`
  (dispatch), `dispatch wait [--row|--any] --timeout` (wake), `dispatch verify
<row>` (artifact gate), `dispatch set-status` (judgment), `session close`
  (cleanup), `session list --live` (fleet state), `--resume-work` (finisher).
- **Exit-0 is not done**: a session exiting is not a handoff; only `dispatch
verify` + the Lead's read of the artifact make `done` — and the daemon's
  transport stamper parks rows `waiting_human` with a `note`, which the Lead
  surfaces rather than silently re-dispatching.
- **The finisher pattern**: on a failed row, resume with
  `thegn session open --resume-work <row-id> --json` instead of re-dispatching
  from memory; the retry chain shows on the board via `--parent`.
- **The cheap ratchet suites a reviewer MUST run before a verdict** (scoped,
  seconds each, no full-workspace compile):
  - core: `cargo nextest run -p thegn-core env_overlay config_example
capability` and `-p thegn-svc --test control_schema`;
  - host: `cargo nextest run -p thegn-host complete help catalog_tests
mq_assets platform_ratchet`.
- **Generic-roles config shape**: harness/model on `[[agents]]` entries or per
  stage (`harness = "pi"`, `model = "model-proxy/fast"` on a stage) — the chart
  mixes harnesses and tiers per stage; stage overrides layer over the entry.

---

## 5. Chunk plan

Three chunks. **Recommended order strictly serial: 1 → 2 → 3.** 1 and 2 are
near-disjoint in code but share `cmd/session.rs` (§2.3's helper move lands
cleanly on top of §1.2's factoring), `cmd/dispatch.rs` (one-line vs list
changes), `docs/cli.md` and `test/smoke.sh` (append-only, different sections) —
if the Lead parallelizes 1 ∥ 2, expect those four files to need a manual fold.
3 hard-depends on both (migration v60 follows v59; the skill documents every
verb from 1+2).

|            | Chunk 1                                      | Chunk 2                                                                                                                         | Chunk 3                                                           |
| ---------- | -------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------- |
| Core       | `pipeline_resume.rs`                         | `pipeline_exit.rs`, `harness.rs`, `config_pipeline.rs`, db v59                                                                  | `pipeline_chunk.rs`, db v60, `issue.rs`                           |
| Host       | `cmd/session.rs`, `cmd/dispatch.rs` (1 line) | `daemon/pipeline_retry.rs`, `daemon/mod.rs`, `daemon/agent_open.rs`, `stage_prompt.rs`, `svc/control/mod.rs`, `cmd/dispatch.rs` | `cmd/dispatch.rs`, `cmd/session.rs`                               |
| Docs/tests | `docs/cli.md`, `test/smoke.sh`               | `config.toml.example`, `docs/cli.md`, `test/smoke.sh`                                                                           | `config.toml.example`, `SKILL.md`, `docs/cli.md`, `test/smoke.sh` |

Every chunk: `just quick <crate>` while iterating; the scoped nextest filters
named in the chunk file; the pre-push hook is the full gate. No `just test` /
`just ci` / e2e inside the headless turns (heavy-guard policy,
`CLAUDE.md`).

### Constraints checklist (per issue)

- **Catalog**: no new verbs ⇒ `capability::CATALOG`, `cli_control_caps`,
  `docs/cli.md`, `test/smoke.sh` stay consistent without new rows; both new CLI
  flags ride documented verbs and gain smoke checks + cli.md lines.
- **Pure policy in core with tests**: `pipeline_resume`, `pipeline_exit`,
  `pipeline_chunk` — no I/O, no subprocess, no tokio (`pipeline_run.rs`
  doctrine, restated in each module header).
- **No blocking work on the loop**: the observer is a daemon task; the finisher
  composition is a short-lived CLI process; nothing touches the compositor.
- **Not duplicated**: dispatch verify/wait, the artifact gate, `session close`,
  `--live`, and `--stage --issue` are consumed, not reimplemented.

### Non-goals (recorded so nobody "helpfully" adds them)

- Capturing harness-native session ids (would enable `claude --resume <id>`;
  needs the headless output-format change — follow-up).
- Stamp-on-exit for non-matching exits (the Lead's judgment, as today).
- A scheduler for `concurrency`/`timeout_secs`/`next` (rejected in
  `config_pipeline.rs:1-20`; stays rejected).
- openspec deltas: the Lead opens the change folder (`openspec/changes/`) at
  apply time, citing this design; spec deltas land with the code chunks.

## Risks

- **Two stampers, one row** (pane path vs daemon observer): mitigated by the
  no-attached-clients scope rule (§2.1) + terminal-row skip; the db fn filters
  non-terminal rows and the pane path wins by construction for watched panes.
- **Signature drift**: harnesses reword their errors. The lists are config;
  defaults are generous substrings; a miss degrades to today's behavior (Lead
  sees the tombstone via `dispatch wait`).
- **Frontmatter parser vs agent creativity**: strict-but-forgiving (unknown
  keys ignored, both list styles accepted, errors name the line); the gate
  fails closed — an unparseable `files:` block is a refusal with the parse
  error, `--force` is the escape hatch.
