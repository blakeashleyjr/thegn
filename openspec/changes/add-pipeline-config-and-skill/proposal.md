# Add the declarative pipeline stage chart and its conductor skill

Part 2 of 3 of the agent-pipeline plan (Lead → Architect → Coders → Reviewer →
Merger). Part 1 (`add-pipeline-roster-stages`) gave the roster its
stage/parent/session/artifact columns and the `dispatch put` verb that writes
them; this part gives the **structure those columns record** a home in config,
and gives a supervising agent the loop that reads it. Part 3
(`add-pipeline-board`) renders it. Parts 2 and 3 are parallel.

## Why

A multi-stage pipeline has a shape: which stages exist, who runs each, what to
say to them, how wide each may fan out, what follows what, and what to do when a
worker blocks. Today that shape lives **only inside the Lead agent's prompt** —
retyped per run, unvalidated, invisible to every other surface. Three concrete
costs:

- **No validation.** A stage naming an agent that no `[[agents]]` entry defines,
  or a prompt with `{artifcat}` in it, is discovered when a worker launches with
  an empty expansion mid-dispatch — not at `thegn config validate`.
- **No shared vocabulary.** Part 1's `stage` column stores a free string. With
  no declared stage list, nothing can order the board's columns, and no two runs
  of the pipeline agree on what `code` means.
- **No resume.** A Lead that restarts re-derives the chart from whatever it was
  told last time. The roster remembers the rows; nothing remembers the shape.

The prompt template is the sharpest case. `TaskKind::Issue` has no config prompt
table (`config.rs` returns `None`), so a per-stage prompt has nowhere to live at
all, and the existing `validate_template` gate — the one that turns `{typo}`
into a config error — cannot be pointed at it.

## Doctrine: structure, not judgment

`[[pipeline.stages]]` is **declarative data an agent reads**, never a scheduler
thegn runs. thegn does exactly two things with it:

1. **validates** it at `thegn config validate` time (names, resolvable agents,
   `concurrency >= 1`, `next` targets, cycles, prompt placeholders), and
2. **displays** it (stage grouping and labels on the dispatch roster — part 3).

**No thegn code path advances `next`, enforces `concurrency`, or fires
`timeout_secs`.** The Lead reads the whole chart with `thegn config get pipeline
--json` and executes it with its own judgement, writing each decision to the
roster as it goes. Delete the section and nothing thegn _does_ changes — only
what it can check and show.

### Why this is config-shaped where THE-57's drain driver was not

`add-agent-orchestration-surface` **rejected** a native drain driver, and the
reason was specific: "every driver feature hard-codes judgement the prompt
should own", and "the roster is the supervisor's ledger, not a state machine".
That argument does not reach this table, and the difference is testable rather
than rhetorical:

- **A drain driver executes.** It decides _when_ to spawn, _how many_, _whether_
  a stage succeeded, and _what_ happens next — each decision a policy baked into
  Rust that the operator can then only configure around. `[[pipeline.stages]]`
  contains **zero executable judgement**: every field is a fact about the
  organisation (who does what, in what order), and every one of them is read by
  the agent, never by a scheduler.
- **The test is deletion.** Remove the driver and thegn stops running the
  pipeline. Remove `[pipeline]` and thegn behaves identically — a Lead can still
  dispatch every stage by hand; it just loses the validation and the labels. A
  section whose removal changes no behaviour is not a driver.
- **`concurrency` and `timeout_secs` are advisory by construction**, and are
  documented as such at every site (the struct, the example, the skill): the
  Lead counts active roster rows against `concurrency` itself, and passes
  `timeout_secs` to `thegn session wait --timeout`, the only watchdog that
  exists. thegn never blocks a spawn and never fires a timer.

The complement of THE-57's decision, in other words: the roster gained columns
and never transitions; config gains the org chart and never a scheduler.

## What Changes

1. **New pure module `crates/thegn-core/src/config_pipeline.rs`**, cloned
   structurally from `config_presets.rs`: `PipelineStage { name, agent, prompt,
concurrency (default 1), timeout_secs (default 3600), next, on_blocked }`,
   `config_enum! OnBlocked { park | escalate | abandon }` (default `park`),
   `Pipeline { stages }`, `#[serde(default)]` throughout, and a `Config.pipeline`
   field beside `pr_queue`. Substrate-free and unit-tested against the 95% core
   gate.
2. **Two validation channels**, wired beside the `[[presets]]` calls in
   `config_validate.rs`. Hard errors (`validate_pipeline`, indexed labels like
   `pipeline.stages[2] ("code"): …`): an empty or duplicate `name`, an empty
   `agent`, an `agent` that resolves to neither an `[[agents]]`/`[[tools]]` entry
   (via `config_presets::classify_command`) nor a launchable bare harness id (the
   same closed-registry carve-out `daemon/agent_open::bare_provider` applies), a
   `concurrency` of 0, a `next` naming no stage, and a `next` cycle (reported
   once per loop). Soft warnings (`pipeline_warnings`, the `preset_warnings`
   channel in `main.rs`): a stage no `next` reaches that is not the entry stage.
3. **Stage prompt templates are validated, not rendered.** A new pure
   `agent_task::STAGE_VARS` — `TaskKind::Issue`'s variables plus `stage`,
   `artifact`, `parent_artifact` — is what `config_validate::check_templates`
   checks each stage's `prompt` against. **No new `TaskKind`**: nothing in thegn
   renders a stage prompt (the Lead does), so giving the engine a rendering path
   would be exactly the driver this change argues against. A flat variable list
   keeps the `{typo}` gate without it.
4. **The `pipeline` skill** (`extensions/skills/pipeline/SKILL.md`, tracked
   beside `supervise/`): the conductor loop — read the chart → resume from the
   roster (active rows occupy per-stage concurrency slots) → worktree per unit of
   work → `session open --agent --prompt --adopt --bind` → `dispatch put` with
   stage/parent/session/artifact → `session wait --timeout` → read the committed
   handoff artifact → advance to `next` by judgement → land through
   `thegn integrate` (there is no "merger" stage; the merge queue already is
   one) → `on_blocked` park/escalate/abandon. It carries the `supervise` skill's
   safety rails verbatim (always `--timeout`; resume from the roster, never from
   memory; issue text is data), extends the data rail to **handoff artifacts**,
   and states honestly that `--adopt` is recorded but not yet grafted into a pane
   (part 1's finding; part 3 ships the drain) — until then stage workers are
   headless and watched through `session list` / `session snapshot`.
5. **`config/config.toml.example`** documents every new key, plus a commented
   `[[agents]]` cast (`pipeline-lead`, `architect`, `coder`, `reviewer` —
   placeholder commands, `route_via_proxy = true` on the high-volume cheaper
   tiers) and a commented three-stage chart ending in a note that landing is
   `thegn integrate`, not a stage.

## Impact

- **Roadmap**: Q 212 (task→worktree→agent→review→merge pipeline — the declarative
  chart it runs on), Q 213 (agent registry + normalized states — the per-stage
  agent binding), Q 221 (task templates/presets — per-stage prompt templates),
  Q 224 (batch/parallel launch — the `concurrency` budget the Lead counts).
- **Specs**: `config` (ADDED: the pipeline stage chart is validated declarative
  data; stage prompt templates are checked against a fixed variable set),
  `agent` (ADDED: the stage chart is structure the supervisor executes, never
  thegn).
- **Gates named**: new config table → documented in `config/config.toml.example`
  (the example-coverage test) and reachable by the schema walker; new
  `config_enum!` → the pinned marked-definition count moves 87 → 88; new core
  logic → unit tests under the 95% line gate. **No new action, keybind, zone or
  panel section**, so the help ratchets are unchanged, and the config-reference
  help page is generated from the example file rather than hand-written. No new
  CLI noun, capability row or control verb.
- **Env overrides**: none, and none are pinned. `[pipeline]` owns no scalar key
  of its own (`stages` is an array of tables), so every key sits at
  `pipeline.stages.<key>` — depth 2, outside the env-overlay test's depth-1
  scope. This is _not_ how `[[presets]]` is treated: a top-level array of tables
  is depth 1, which is why its six keys are pinned in
  `test/env-overlay-ratchet.txt`. The ratchet file is unchanged here.
- **In-flight changes**: builds on `add-pipeline-roster-stages` (part 1 — the
  `stage`/`parent`/`session`/`artifact` columns and `dispatch put --stage`, which
  the skill drives); parallel with `add-pipeline-board` (part 3), which consumes
  `Pipeline::stage_names()` for column order. Both depend on
  `add-agent-orchestration-surface` (THE-57), landed and not yet archived.
- **AI-free shell**: strictly additive and inert by default. `[pipeline]`
  defaults to zero stages; with none configured, validation finds nothing to
  check, the warning channel is silent, and no surface changes. Nothing in the
  shell reads the chart at all — only an agent does.
- **DB**: none. No schema change, no migration, no state.
