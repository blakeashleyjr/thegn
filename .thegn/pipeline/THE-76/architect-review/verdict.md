# THE-76 architect review — verdict

- **Branch:** `tg/the-76-pipeline-v2` (reviewed at `908cafb7`)
- **Base:** `main` (`ad04f2ba`), after the binding merge (`1fee2687`)
- **Reviewer:** architect stage (THE-76 lane)
- **Design:** `.thegn/pipeline/THE-76/architect/design.md` (chunks 1–3 +
  done reports read in full; every Unverified item dispositioned below)

## Verdict: **APPROVED**

No revision chunks. All gaps found were review-fixable and were fixed here
(two commits: the merge resolution `1fee2687`, the completion-catalog
classification `908cafb7`). The branch is green on the full workspace test
suite, smoke, clippy and openspec after the merge. Two pre-PR requirements
remain (see "Gates still owed", both standard pre-PR gates, not design gaps).

## 1. The merge (LEAD addendum, first)

`git merge main` brought in 81 commits (THE-70 sidebar/doctor, THE-83
agents/model/env, THE-72 linear backend, bundled skills). Four conflicts;
the two that mattered were **the same feature built twice**:

| Collision | Branch (THE-76) | main (THE-83) | Resolution |
| --- | --- | --- | --- |
| Stage permissions seeding | CLI-side `seed_permissions` in `open_stage`, union-merge via new `pipeline_run::merge_claude_allow`, stage-list only, Claude-file hard-coded | Daemon-side `agent_permissions::seed` inside `launch_spec_full` — harness-aware, *effective* list (stage overrides entry), best-effort at launch, wired into **every** launch path | **main's is the keeper.** `open_stage` now carries the stage name through `AgentLaunch.stage`; the daemon layers `model`/`env`/`permissions` and seeds. The CLI-side seeder, `merge_claude_allow`, `PermsError` and their tests are deleted — one file, one writer. Two seeders with different merge semantics (union vs replace) writing the same file on one dispatch would have flapped. |
| Config freshness (design item 7 / D4) | `agent_open::resolve` re-loads with `pipeline_run::with_fresh_registry`, registries only, **empty** `--set`/`--config` source | `config_source::install/fresh()` — records the daemon's *real* `--set`/`--config` source, whole-config per-request load, snapshot fallback | **main's is the keeper** (and strictly better: the branch's empty-source re-load would have silently dropped boot-time `--set` overrides on registry keys). A naive merge left *both* running per open — two TOML reads; the branch's block, `with_fresh_registry` and its tests are deleted. |
| `--stage` on `session open` | Full atomic dispatch (`--issue` required, `--prompt` forbidden by clap) | Lightweight overlay: layer the stage's `model`/`env`/`permissions` over the agent; documented + used with `--prompt` in `docs/help/cli.md`, `configuration.md` and the bundled `/pipeline` skill | **Union, both live:** `--stage <name> --issue <id>` = the THE-76 dispatch; `--stage` alone = THE-83's overlay open (agent defaults to the stage's, `--prompt` honoured). The clap `conflicts_with` moved into `open_preflight` (message: "the stage's prompt template owns the task"), smoke checks updated to match. |
| `PipelineStage` fields | Added `permissions` (seeding doc) | Added `harness`/`model`/`env`/`permissions` (override doc) | main's fields + a combined `permissions` doc (override semantics *and* harness-vocabulary wording). The branch's `validate_pipeline` permission checks (blank/control-char/duplicate) kept; main's `validate_agent_models` kept. |

Smaller resolutions: `agent_task.rs` tests are additive on both sides (both
blocks kept); `config.toml.example` takes main's per-stage override block (it
registers the same keys the drift test needs); the two stale `NamedCommand`
literals in the branch's tests gained main's new fields.

## 2. Corrections applied by the reviewer

1. **Merge resolution** — `1fee2687` (details above). Also:
   - `open_stage` passes `stage: Some(stage.name)` through `AgentLaunch`, so a
     dispatch gets THE-83's tiering (model/env) and seeding; the CLI-side
     seeder is gone (doc comment on `open_stage` states why: one seeder, every
     launch path).
   - `open_preflight` gained the `issue` param; the `--prompt`-with-dispatch
     refusal is a documented offline caller-mistake refusal; unit tests
     updated (`a_dispatch_does_not_fire_the_headless_check`,
     `a_dispatch_refuses_an_explicit_prompt`).
   - **Test-infra fix:** the dispatch tests' git harness hung 60 s and failed —
     the ambient global config sets `commit.gpgsign=true`, so `git commit` in
     the temp repo blocked on a passphrase prompt. Pinned
     `commit.gpgsign=false` in the harness (the same isolation every other
     git harness in the repo already applies).
   - Smoke: the `--stage X --prompt Y` clap-conflict check replaced by the two
     union-shape checks (overlay path hits the offline stage-miss; dispatch +
     `--prompt` hits the preflight refusal), backed by a `[[pipeline.stages]]
     smoke` stage appended to the smoke config.
   - Help pages: `docs/help/daemon-and-sessions.md` gained "Closing a session,
     and the dispatch door" (close, liveness token, `--live`, the dispatch
     form); `docs/help/cli.md` gained the verify/wait/gated-done paragraph.
     Neither the TUI action ratchet nor its context ratchets are affected
     (CLI-only verbs), but the prose now exists.
   - CHANGELOG: a pipeline-mechanism entry under `[Unreleased]` (the branch
     had none; the repo documents every feature).
   - openspec deltas amended to the merged contracts: the "Stage permissions
     are seeded, not interpreted" requirement (union + CLI-side merge) is now
     "Stage permissions ride the launch, never a second seeder" (effective
     list, one daemon-side seeder, replace-what-the-stage-overrides); the CLI
     spec pins both `--stage` forms. `openspec validate --all --strict`: 170/170.
2. **Completion catalog** — `908cafb7`. The completion-slot ratchet refused
   the seven new value-taking args. All classified: row ids → new
   `Reserved::DispatchRow` (roster is local SQLite; a real source is
   implementable later), `--issue` → `Reserved::Issue`, `--timeout` → new
   `Reserved::Freeform`, `--parent-artifact` → `Structural` (path; engine's
   filesystem completion is intended, same shape as `--config`),
   `session close <session>` → `Session`. The allowlist was not grown.

## 3. Design conformance (the `[spec]` items 1–7)

| Design item | Status |
| --- | --- |
| 1. Stage dispatch performed atomically | ✅ `session open --stage --issue` → `open_stage`: row before open (D5), any post-insert failure leaves the row `failed` with `// best-effort` on the error stamp, rendered-prompt refusal, literal-brace property pinned end-to-end. |
| 2. Artifact paths (pure, sanitized) | ✅ `pipeline_run::artifact_path` — traversal boundary property tested per-component; per-row paths keep parallel coders collide-free. |
| 3. Stage prompt rendering (CLI-side) | ✅ `template_vars` over the same `parse` as validation; the literal-brace defect is pinned at both the engine and dispatch layers. Tracker consulted only when the template needs it. |
| 4. Stage permissions | ✅ *as superseded by the merge* — the field, validation and config docs are the branch's; the writing is main's daemon-side seeder over the effective list. Recorded in the delta spec. |
| 5. `dispatch verify` | ✅ shared `verify_facts`/`verify_report`; no-artifact rows never gated and never spend a git subprocess; exit 2 = retryable, both modes. |
| 6. `dispatch wait` | ✅ selection before `connect` (offline errors), `--any` first-wake-wins with cancel-on-drop, `matched:false` keeps listening, daemon-error ⇒ `gone:true`, tombstone answers instantly. |
| 7. Daemon config freshness | ✅ *as superseded by the merge* — main's `config_source::fresh()` satisfies D4's intent (freshness without restarting; snapshot fallback) with a truer source. |
| Gated `done` | ✅ CLI-only gate; the control API's `dispatch_set_status` is `Unimplemented`, so there is no bypass to flag; `--force` is visible in both output modes. |
| `session close` + truthful liveness | ✅ close is the dedicated verb over routed `sessions.kill`; the liveness token is the fixed second column; `--live` filters before serialization. |

Doctrine holds unchanged: nothing advances `next`, nothing enforces
`concurrency`, nothing fires `timeout_secs` on its own (`dispatch wait`
blocks only when the *caller* passes a timeout). `thegn-core` gained only
pure, substrate-free code (post-trim: selection, paths, verification). No new
color/glyph literals, no `platform` `#[cfg]`, no `async fn` in traits; the
ignored-`Result` ratchet is green (the one `let _ =` on the failed-stamp path
carries its `// best-effort:` comment).

## 4. Unverified items — disposition

**Chunk 1**
- Heavy gates not run → **now run** (`just test`: 6755 passed incl. all
  ratchets; clippy clean on both crates). `just coverage` (95% core gate)
  → **still owed at pre-PR** (see Gates still owed); risk low — the review
  only *removed* tested core code.
- `just smoke` → **now run, all checks pass** (incl. the new dispatch/smoke
  stage sections).
- Config-reference generation → `config_example` drift tests green; the
  example was amended and re-tested.

**Chunk 2**
- `just smoke` / e2e not run → smoke **now green**; e2e unaffected (no TUI
  frame changes — `git diff main...HEAD` touches no chrome/`src/` frame path
  and no snapshot).
- Tombstone-TTL expiry as the gone-wake trigger → accepted: the unknown-session
  daemon-error branch is the same code path, and waiting 10 min in CI is not
  proportionate. Residual risk noted.
- Help pages → **now added** (prose, not ratchet-driven).

**Chunk 3**
- Heavy gates / smoke → **now green** (the daemon-backed close/liveness
  section passes end-to-end).
- Boot-time `--set` override survival → the branch's unit test was deleted
  *with* `with_fresh_registry`; the property is now carried by main's
  `config_source::install/fresh`, which records the real source (strictly the
  behavior D4 wanted). Flagged, not lost.
- A genuinely live stage dispatch with a real agent binary → **still
  unexercised** (no agent CLI in the replay env; smoke's daemon block opens
  raw sessions). The spawn machinery is `launch_spec_full`, unchanged, and
  the daemon-side stage overlay it now receives is main's shipped path. This
  is the branch's one residual behavioral risk — a first real `/pipeline`
  run should watch the first dispatch's argv/env (`THEGN_LOG=thegn::agent=debug`).
- Signing: the merge and review commits are unsigned (`--no-gpg-sign`), same
  as the chunk-3 commit — gpg-agent pinentry is unattended in this session.

## 5. Gates still owed before the PR

1. `just coverage` — the 95% `thegn-core` line gate (CI-only; not run here per
   the machine-cost policy — llvm-cov is a third full instrumented compile).
   Expect green: the post-merge core delta is net-negative code, all remaining
   items tested.
2. `git push` with the pre-push hook intact (clippy + `just test` + smoke) —
   it will re-run what this review ran; nothing is expected to move.

## 6. Files touched by this review

- `1fee2687` — merge + resolutions: `config/config.toml.example`,
  `crates/thegn-core/src/{agent_task,config_pipeline,pipeline_run,completion/catalog}.rs`,
  `crates/thegn-host/src/cmd/{session,dispatch}.rs`,
  `crates/thegn-host/src/daemon/agent_open.rs`, `test/smoke.sh`,
  `docs/help/{cli,daemon-and-sessions}.md`, `CHANGELOG.md`,
  `openspec/changes/improve-agent-pipeline-v2/specs/{agent,cli}/spec.md`
- `908cafb7` — completion-catalog classification (see §2.2)
- This verdict.
