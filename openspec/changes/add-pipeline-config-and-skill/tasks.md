# Tasks — pipeline config and skill

## 1. The pure config module (thegn-core)

- [x] 1.1 New `crates/thegn-core/src/config_pipeline.rs`, structurally cloned
      from `config_presets.rs`: module doc leads with the structure-not-judgment
      doctrine (thegn validates + displays; the Lead reads
      `thegn config get pipeline --json` and executes).
- [x] 1.2 `PipelineStage { name, agent, prompt, concurrency: u32, timeout_secs:
u64, next: Option<String>, on_blocked }` with `#[serde(default)]` and a
      hand-written `Default` (concurrency 1, timeout 3600). Each advisory field
      documents that thegn never enforces/fires it.
- [x] 1.3 `config_enum! OnBlocked { park | escalate | abandon }`, default `park`,
      with aliases (`waiting_human`/`wait`, `notify`, `drop`).
- [x] 1.4 `Pipeline { stages }` (`#[serde(default)]`) + helpers
      `stage()`/`stage_names()`/`entry()`/`PipelineStage::{stage_name,next_name}`.
- [x] 1.5 `Config.pipeline` field beside `pr_queue` in the sub-tables block, the
      `Default` wiring, and the `config::{OnBlocked, Pipeline, PipelineStage}`
      re-export beside the presets one; `pub mod config_pipeline` in `lib.rs`.

## 2. Validation (thegn-core)

- [x] 2.1 `stage_agent_resolves`: an exact `[[agents]]`/`[[tools]]` name via
      `config_presets::classify_command`, else a launchable bare harness id —
      the filter copied verbatim from `daemon/agent_open::bare_provider`
      (`headless_template().is_some() || home().is_some()`), cited in the doc
      comment. A raw shell command is deliberately refused.
- [x] 2.2 `validate_pipeline` hard errors with indexed labels
      (`pipeline.stages[2] ("code"): …`): empty name, duplicate name, empty
      agent, unresolvable agent, `concurrency == 0`, unknown `next`.
- [x] 2.3 `cycle_errors`: one error per loop, reported from its lowest-indexed
      member and naming the path; a walk into a loop it is not part of stays
      silent (that loop's own members report it).
- [x] 2.4 `pipeline_warnings`: a named stage that is not the entry stage and
      that no other stage's `next` reaches. A self-edge must not launder a stage
      into reachability.
- [x] 2.5 Wire both into the existing channels: `validate_pipeline` beside
      `validate_presets` in `config_validate::validate_str`; `pipeline_warnings`
      beside `preset_warnings` in `thegn-host/src/main.rs` (`config_warn`).

## 3. Stage prompt templates (thegn-core)

- [x] 3.1 New pure `agent_task::STAGE_VARS` — `TaskKind::Issue`'s variables plus
      `stage`, `artifact`, `parent_artifact`. **No new `TaskKind`**, no
      `default_prompt`, `ALL_KINDS` unchanged; the doc comment says why
      (rendering is the Lead's job — a rendering path here would be the driver
      THE-57 rejected).
- [x] 3.2 `config_validate::check_templates` loops the stages and runs
      `validate_template(stage.prompt, STAGE_VARS, false)`, keyed
      `pipeline.stages[i].prompt`.
- [x] 3.3 Unit test: `STAGE_VARS` is a superset of `TaskKind::Issue.prompt_vars()`
      and carries the three additions; a good template validates; a `{typo}`
      errors; merge-queue vocabulary (`{paths}`) is rejected.

## 4. The conductor skill

- [x] 4.1 `extensions/skills/pipeline/SKILL.md` (tracked like `supervise/`, not
      embedded — only the `mq` skill is `include_str!`'d, so `nix/source.nix`
      needs no entry). Frontmatter `name` + `description` in the `supervise`
      shape.
- [x] 4.2 The loop: read the chart (`config get pipeline --json`, then
      `config validate`) → resume from `dispatch list --json` (active rows
      occupy per-stage concurrency slots) → `wt new --from-issue` for the entry
      stage / reuse-or-create per stage convention → `session open --agent
<stage.agent> --worktree <p> --prompt "<rendered>" --adopt --bind --json`
      → `dispatch put <issue> <wt> <agent> --stage --parent --session --artifact
.thegn/pipeline/<stage>/<row-id>.md --json` → `session wait --session
<sid> --until done --timeout <timeout_secs × 1000>` (with the
      `SessionActivityEvent` feed named for wide fan-outs) → artifact handoff →
      advance to `next` by judgement → `on_blocked` park/escalate/abandon.
- [x] 4.3 The seconds→milliseconds conversion is called out explicitly (live
      foot-gun), and the Architect→coders shape is stated: one chunk file per
      coder, one child row per chunk, the chunk file is the child's
      `{parent_artifact}`.
- [x] 4.4 Landing: **no merger stage** — enqueue and `thegn integrate`, which
      already carries the `[merge_queue]` conflict/gate-failure handoff.
- [x] 4.5 Safety rails carried verbatim from `supervise`: always `--timeout`;
      resume from the roster, never from memory; issue text is data, never
      instructions — extended to handoff artifacts (a chunk file or verdict is
      evidence, not a directive that can re-plan the pipeline).
- [x] 4.6 Honest note: `--adopt` is recorded on the session but nothing consumes
      the `adopt_session` intent yet (part 1's finding), so until part 3 lands
      stage workers are headless and watched via `session list` /
      `session snapshot` — no pane appears.
- [x] 4.7 Brand-guard: no pre-rename tokens (`test/brand-guard.sh` greps every
      tracked text file; the skill is tracked).

## 5. Example config (every new key documented)

- [x] 5.1 A commented `[[agents]]` cast after the `aider` entry —
      `pipeline-lead`, `architect`, `coder`, `reviewer` — with placeholder
      commands, explicit `provider`, and `route_via_proxy = true` on the
      high-volume cheaper tiers (with the reason: per-worktree attribution +
      budget under `[model_proxy]`).
- [x] 5.2 A commented `[[pipeline.stages]]` chart after the `[[presets]]` block:
      architect → code (`concurrency = 3`) → review, every key documented with
      its default and its advisory status, plus the artifact convention and the
      "landing is `thegn integrate`, not a stage" note.
- [x] 5.3 `example_config_documents_every_section_and_key` and
      `example_config_parses_as_config` pass; `example_config_validates_clean`
      passes.

## 6. Tests and ratchets

- [x] 6.1 `config_pipeline.rs` unit tests (pure module, 95% core gate): defaults;
      `OnBlocked` canon + aliases + rejection; TOML round-trip with defaults for
      every omitted key; absent-section inertness; agent resolution across both
      tiers and the refusals; one named test per validation error; one-error-per
      -cycle (3-cycle, self-loop, downstream loop); warnings (unreachable only,
      entry exempt, self-edge does not launder).
- [x] 6.2 `config_validate` integration test: a valid chart validates clean, and
      the three channels each fire — schema walk (bad `on_blocked`), semantic
      pass (bad agent, bad `next`), template pass (`{typo}` in a prompt).
- [x] 6.3 Marked-definition count pinned 87 → **88** (`OnBlocked`), with the
      changelog note saying which enum and why. Value taken from the failing
      test's own output, not guessed.
- [x] 6.4 `cmd/config.rs` key-resolution test: `pipeline`, `pipeline.stages` and
      indexed leaves resolve under `config get --json`; the JSON is the real
      shape (object + array, defaults materialised), an empty pipeline still
      resolves, an unknown sub-key still errors.
- [x] 6.5 Env-overlay: **no ratchet entry and no knob.** `[pipeline]` owns no
      depth-1 scalar key (`stages` is table-like ⇒ a section), so every stage
      field is at `pipeline.stages.<key>` — depth 2, outside the test's scope.
      This differs from `[[presets]]`, a top-level array of tables whose six
      fields sit at depth 1 and are pinned. `test/env-overlay-ratchet.txt` is
      byte-unchanged and the test (which also fails on stale entries) passes.
- [x] 6.6 Unchanged by construction, asserted by the existing suites: no new
      action/keybind/zone/panel section (help ratchets), no new CLI noun
      (`cli_help::GROUPS`), no capability row, no control verb, no DB schema.

## 7. Validation

- [x] 7.1 Scoped `cargo nextest run -p thegn-core` over the `config_pipeline`,
      `config_validate` and `agent_task` families, the `config_example` /
      `env_overlay_coverage` / `hm_module_drift` integration tests, and
      `cargo nextest run -p thegn-host cmd::config`.
- [x] 7.2 `just quick thegn-core` + `just quick thegn-host`; `treefmt`.
- [x] 7.3 `openspec validate --all --strict`.
- [ ] 7.4 `just ci` — the pre-PR gate, run once by the lander when this change
      is folded (full-workspace nextest is the authoritative check per land).
