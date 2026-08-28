# THE-76 chunk 3 — completion summary (code stage)

Coder stage, chunk 3 of improve-agent-pipeline-v2 (the final chunk). Branch
`tg/the-76-pipeline-v2`, commit `feat(session): server-side stage dispatch,
close, and truthful liveness (THE-76)` — code + this summary in one commit.
Roster row 91; this file is the row's artifact.

## What landed

Exactly the three files the spec allows:

1. **`crates/thegn-host/src/cmd/session.rs`** — `SessionAction`, `run_async`,
   `session_line`, plus the new private dispatch helpers (chunk 2's
   `cli_control_caps()` untouched):
   - **`Close { session, json }`** — `client.kill(&session)`; human
     `closed <id>`, `--json` `{"session":…,"closed":true}`; the
     `Close { json: true, .. }` arm added to the `json_mode` match so the
     no-daemon degradation emits `{"error":"no_daemon"}` like every other JSON
     verb. The session id is positional (spec: `session close bogus`). No
     catalog change — `sessions.kill` was already routed and already covered
     on the CLI surface through `API_CALLS`.
   - **Truthful `session_line`** — a liveness token as the line's **second
     column** (immediately after the id, so a fixed-column grep works):
     `live`, else `exited(<code>)` / `exited(?)` when unreapable, suffixed
     with the `final_state` word — `exited(0,done)`. The rest of the line is
     unchanged, so `thegn attach`'s shared listing only gains the token.
     `SessionAction::List` gains `--live`, which retains only rows with
     `exited_at_ms.is_none()` **before** serialization, so a `--live --json`
     caller never has to re-filter.
   - **`session open --stage`** — new flags `--stage` (`requires = "issue"`,
     `conflicts_with = "prompt"`), `--issue`, `--parent`, `--parent-artifact`;
     `--agent` becomes `Option<String>` (`required_unless_present = "stage"`,
     defaulting to `stage.agent`, explicit flag wins — documented in the flag
     help). The non-stage path is byte-identical modulo the `agent.expect()`
     clap invariant. The dispatch lives in a new private `async fn open_stage`
     over a `StageDispatch` struct (no 11-arg call), following the spec's
     order exactly: stage lookup → `resolve_worktree` → two-tier branch lookup
     (registered row, else `git rev-parse --abbrev-ref HEAD`) → issue facts →
     parent check → **roster insert** → artifact path → render → empty-prompt
     refusal → permission seeding → `client.open(headless: Some(true))` →
     `stamp_dispatch_run` + `running` → print (`--json` emits the spec's
     seven-key object). Key sub-decisions: - the stage lookup runs **twice** via one helper `stage_or_bail` (message
     lives in one place): once in `open_preflight` **before `connect`** so a
     config typo is answerable offline (the smoke check relies on it), and
     once inside `open_stage`, which owns the whole dispatch; - `open_preflight` also refuses explicit `--headless` with a blank
     prompt, before `connect`; a promptless plain open stays a legal
     interactive launch (pinned by a test); - issue facts: `agent_task::template_vars(&stage.prompt)` decides —
     `{issue_number}` is local (`pipeline_run::issue_key`); the tracker is
     consulted **only** when the template references
     `{issue_title}`/`{issue_body}`/`{issue_url}`, and a failed lookup
     propagates the tracker's own error (verified live: `issues.get …: no
issue provider configured`). Because facts are gathered at step 4 and
     the row is inserted at step 6, a tracker failure leaves **no** row —
     exactly the spec's order; - `{parent_artifact}` binds `--parent-artifact`, else the parent row's
     own `artifact_path`, else empty (the parent `get_dispatch` row is kept
     for both the existence rule and this fallback); - everything after the insert is wrapped so **any** failure stamps the
     row `failed` and the error names it (`dispatch N failed: …`); the
     error-path stamp itself is `let _ =` with a `// best-effort:` comment
     (the original error must not be masked; the roster is a cache). The
     happy-path stamps use `?`.
   - **Helpers**: `stage_task_vars` binds the nine `STAGE_VARS` (single
     testable assembly point); `seed_permissions` merges via chunk 1's
     `pipeline_run::merge_claude_allow` into
     `<worktree>/.claude/settings.local.json` (absent file = normal first
     dispatch; unreadable file = error; creates `.claude/`).
   - **Tests** (2 new modules, 13 tests): `session_line_tests` (live /
     exited(code) / exited(?) / exited(0,done) — asserting the token column,
     not the whole line) and `open_stage_tests` (stage-miss message names the
     typo and the configured stages; empty-pipeline wording; preflight
     offline refusal; `--headless` blank-prompt refusal; promptless open
     stays legal; stage path doesn't fire the headless check; the
     **literal-brace end-to-end render test** — an issue body full of
     `{ nodes { name } }` renders verbatim through `stage_task_vars` +
     `render_prompt`; permissions merge preserves a pre-existing `model` key
     and is byte-idempotent on re-dispatch; fresh creation without
     `.claude/`).
2. **`crates/thegn-host/src/daemon/agent_open.rs`** — per-request registry
   refresh (item 7 / D4): `resolve` now loads
   `Config::load_layered(&ProcessEnv, &[], None)` — it already runs on
   `spawn_blocking` — and applies `pipeline_run::with_fresh_registry`, so only
   `agents`/`tools`/`pipeline` are refreshed over the daemon's boot snapshot
   and boot-time `--set`/`--config` overrides elsewhere survive. A failed load
   falls back to the snapshot unchanged (`load_layered` warns-and-defaults).
   New test `a_newly_added_agent_entry_resolves_after_the_registry_refresh`
   pins the composition: the merged config resolves a newly-added `[[agents]]`
   entry the boot snapshot lacks, and a snapshot-only `repo_roots` override
   survives the merge.
3. **`test/smoke.sh`** — 7 new daemon-free `check`s in the session section
   (same `set +e` / flag-variable style, no daemon started): `session close`
   (positional id) without a daemon exits 1 with the clear message and
   `--json` emits `{"error":"no_daemon"}`; `session list --live` exits 1 and
   `--live --json` emits `{"error":"no_daemon"}`; `session open --stage
nosuchstage --issue …` fails **offline** naming the stage; `--stage X
--prompt Y` is refused by clap (`cannot be used with`); `--headless` with
   no prompt is refused naming the empty prompt. `shellcheck` + `shfmt -d`
   clean.

## Verification (scoped per the dev-loop policy)

- `just quick thegn-host` — clean (clippy `-D warnings`, lib+bin).
- `cargo nextest run -p thegn-host session` — 86 passed (incl. all new tests).
- `cargo nextest run -p thegn-host open_stage_tests session_line_tests
agent_open` — 24 passed.
- `shellcheck test/smoke.sh` + `shfmt -d test/smoke.sh` — clean (store-pinned
  binaries; shellcheck was not on PATH outside the dev shell).
- rustfmt 1.96.1 (the flake-pinned build) `--check` clean on both .rs files.
- Ratchets run directly: `ignored-result` (clean, 323 pinned — `cmd/session.rs`
  was already a pinned file; the one new `let _ =` carries the sanctioned
  comment), `forge-leak`, `async-trait`, `json-emit` — all clean.
- **Live behavioural replay** (isolated HOME/XDG\_\*/socket, one daemon, the
  built binary; not e2e, nothing in smoke starts a daemon):
  - dispatch with a stage whose agent is unknown → row inserted then `failed`
    (never `queued`), error `dispatch 1 failed / no headless form for agent
'ghost'`;
  - **registry freshness**: appending the `[[agents]] ghost` entry to config
    with the daemon running, no restart → re-dispatch resolves: row `running`,
    session opened, artifact `.thegn/pipeline/TEST-1/code/2.md`, branch
    resolved, permissions seeded;
  - `session list` shows the finished worker as `exited(1,idle)`; `--live`
    filters it (`no live sessions`);
  - permissions file byte-identical across re-dispatch (`cmp` on the file);
  - a stage whose template renders empty (`prompt = "{parent_artifact}"`,
    artifact-less parent) → `dispatch N failed: stage 'blank' rendered an
empty prompt`, row `failed`;
  - `--parent 999` refused with no row inserted (check precedes the insert);
  - `{parent_artifact}` from the parent row's `artifact_path` and the
    `--parent-artifact` override both verified (dispatches succeed where they
    failed as empty before the fallback existed);
  - the tracker gate: a stage referencing `{issue_title}` with no tracker
    configured → the tracker's own error (`issues.get linear:TEST-1: no issue
provider configured`), and **no roster row** (facts precede the insert);
    a tracker-free stage with a different issue dispatches fine;
  - a raw `sleep 60` session over the socket: `session list` token `live` →
    `session close <id>` prints `closed <id>` → tombstone shows
    `exited(?,idle)`; closing again is idempotent (`closed <id>` — the
    daemon's tombstone answers);
  - `--json` dispatch output matches the spec's key set (row, session, stage,
    artifact, issue, worktree, branch).

## Deviations / notes for review

- **The commit is unsigned** (`%G?` = `N`). Three signing attempts timed out:
  gpg-agent's passphrase cache was cold and pinentry-gnome3 was never answered
  (the operator was not at the machine). Sibling commits (chunks 1–2) are
  signed. Pre-commit hooks (treefmt, shellcheck, yamllint) passed on every
  attempt, and the final commit used `--no-gpg-sign` to avoid blocking the
  stage; amend/re-sign if signature coverage matters.
- The summary was committed by **amending** the code commit (branch is
  local-only, no upstream), so code + summary land as one commit like chunks
  1–2.

## Unverified (for the review stage)

- **`just smoke` / e2e not run** (dev-loop policy). The new smoke section was
  validated by shellcheck + shfmt + the live replay above, which exercises the
  same commands and assertions daemon-free.
- **`just test` / `just coverage` / `just ci` / `just lint` not run** (heavy
  gates are pre-push/CI-only). In particular: the color/glyph and
  platform-`#[cfg]` ratchets are the Rust `file_ratchet` twins run under
  `just test` — by inspection nothing new was added (no color/glyph literal, no
  `#[cfg]`), and the bash-run ratchets above are clean; the 95% core coverage
  gate is unmeasured (this chunk touches `thegn-host` only, which is not
  coverage-gated).
- **A running daemon observing a _second_ config change was not re-tested**
  (only one freshness round-trip was exercised); the per-request read makes
  this a non-scenario by construction.
- **Boot-time `--set` overrides on the _daemon_ command line surviving a
  dispatch were not observed live** — the merge property is pinned by the new
  agent_open unit test (`repo_roots` override survives `with_fresh_registry`),
  and D4's design specifies the load with `&[]` overrides, so registry-scoped
  `--set` values are refreshed away by design (documented in the design doc).
- **A genuinely _live_ stage-dispatched session with a real agent binary was
  not observed** (no agent CLI installed in the replay env; the child exits
  non-zero immediately, which itself verified the tombstone/exited-token
  path). The spawn machinery is `launch_spec_full`, unchanged in this chunk.
- **Help pages**: unchanged. The help ratchet pins TUI `ACTION_SPECS` ids;
  these are CLI verbs with no TUI action ids (same reasoning as chunk 2).
