# Design — pipeline config and skill

## Shape

```text
  config.toml                        thegn                       the Lead agent
  ───────────                        ─────                       ──────────────
  [[pipeline.stages]]  ──parse──►  Config.pipeline
    name / agent                        │
    prompt                              ├─ validate_pipeline ──► `config validate`
    concurrency / timeout_secs          │    (names, agents, concurrency,
    next / on_blocked                   │     next targets, cycles)
                                        ├─ check_templates(STAGE_VARS)
                                        │    (a `{typo}` is an error)
                                        ├─ pipeline_warnings ──► config_warn
                                        │    (unreachable stage)
                                        │
                                        └─ value_at("pipeline") ──►
                                             `thegn config get pipeline --json`
                                                                        │
                                                                        ▼
                                                          reads the chart, renders
                                                          prompts, counts slots,
                                                          dispatches, waits, advances
                                                                        │
                        agent_dispatches (part 1) ◄──── dispatch put ────┘
```

The arrow from the chart to anything thegn _executes_ is missing on purpose.
`next`, `concurrency` and `timeout_secs` have exactly one consumer — the agent
on the right.

## Why a table and not a `TaskKind`

The obvious place for a per-stage prompt is the `agent_task` engine: add
`TaskKind::Stage`, give it `prompt_vars()`, let `resolve_agent` +
`substitute_command` render it, and reuse the whole merge-queue/pr-queue prompt
machinery.

That is the driver, wearing a different hat. The moment thegn can _render_ a
stage prompt, it needs to know which stage is next, which worktree, which
artifact — and it has re-derived a scheduler from the inside out. So the change
takes only the half it needs:

- **`STAGE_VARS` is a `pub const`, not a `TaskKind`.** It is a list of legal
  variable names, consumed by `validate_template` at config-validate time and by
  nothing else. `ALL_KINDS` is unchanged, `TaskKind::prompt_vars` is unchanged,
  and there is no `default_prompt` for a stage — a stage prompt has no default,
  because there is no code path that would need one.
- **Rendering stays with the Lead.** It substitutes the values it alone knows
  (which chunk, which artifact, which parent). A server-side `--stage` renderer
  is explicitly deferred; if it ever lands it must render _on request_, not on a
  schedule.

`STAGE_VARS` is a superset of `TaskKind::Issue.prompt_vars()` (a pinned test),
so anything sayable to an issue worker is sayable to a stage worker; the three
additions (`stage`, `artifact`, `parent_artifact`) are exactly the pipeline's
own vocabulary.

## Why `config_presets` is the pattern to clone

`[[presets]]` and `[[pipeline.stages]]` have the same job description: a named
entry that _layers on_ the `[[agents]]`/`[[tools]]` registry rather than
duplicating it, with two validation channels and no host types. So the module
copies its structure deliberately —

- a pure module (no I/O), which is what makes the 95%-line core gate reachable;
- `config_enum!` for the one enumerated field, so `config validate --strict`
  covers it **by construction** (the schema walker finds the marker; the pinned
  marked-definition count moves 87 → 88 and no key list is edited);
- `#[serde(default)]` on the struct, so a half-written stage still parses and a
  missing table is an empty inert one;
- **hard errors with indexed labels** (`pipeline.stages[2] ("code"): …`) so a
  message points at a line, and **soft warnings** on the `config_warn` channel
  for things that are suspicious rather than wrong.

And it reuses `classify_command` rather than writing a second resolver: a stage
agent is a registry name for the same reason a preset command is.

## Agent resolution: the bare-harness carve-out

`stage_agent_resolves` accepts two tiers, and the second is not obvious:

1. an exact `[[agents]]`/`[[tools]]` name (a `PresetCommand::Named`);
2. a **bare harness id** — `claude`, `codex`, `aider`, `antigravity` — filtered
   exactly as `daemon/agent_open::bare_provider` filters it:
   `harness(id).filter(|h| h.headless_template().is_some() || h.home().is_some())`.

Tier 2 exists because the launch path already accepts it: a supervisor that says
`claude` on a machine whose entry is called `main-agent` still launches. If
config validation refused what `session open --agent` accepts, `config validate`
would fail a chart that runs fine — the worse of the two errors.

A raw shell command is deliberately **not** accepted, which is where this
diverges from presets (whose third tier is "run it through the login shell"). A
stage worker is opened with `session open --agent`, which takes a registry name;
accepting `just dev` here would produce a validation pass and a dispatch-time
failure.

## Cycle detection

`next` makes the chart a graph, and a cycle is a Lead that never terminates. The
check walks forward from every stage following `next`; a walk that returns to
its own start is a cycle. To keep one loop from producing N identical errors,
only the **lowest-indexed member** reports — so `a → b → c → a` yields one error,
on `pipeline.stages[0]`, naming the path. A walk that runs into a loop it is not
part of breaks without reporting; that loop's own members report it.

Duplicate names are a hard error, so the "first match wins" lookup the walk uses
is unambiguous in every config that validates.

## Render / event loop

**No render-damage channel, no wake path, no new thread.** Everything here is
parsing and pure validation, plus one skill file (developer tooling, not shipped
binary code). The two host-side touches are a `config_warn` loop beside the
existing preset one in `main.rs` (CLI startup, already off the render path) and a
unit test in `cmd/config.rs`. Part 3 owns the board's rendering and its
`Incremental`/`Panes` invariant.

**No SQLite schema change**; no `user_version` bump. Part 1 owns the v56 columns
this chart labels.

## Config-surface gates this change satisfies

| Gate                       | How                                                                                                       |
| -------------------------- | --------------------------------------------------------------------------------------------------------- |
| example-config coverage    | every `pipeline.stages` key documented in `config/config.toml.example`                                    |
| schema-walker reachability | `Pipeline` is a plain struct field; `OnBlocked` is reached at `pipeline.stages.on_blocked`                |
| marked-definition pin      | 87 → 88, with the note saying which enum and why                                                          |
| env-overlay ratchet        | **unchanged** — see below                                                                                 |
| core coverage (95% lines)  | `config_pipeline.rs` is pure, with per-error-case unit tests                                              |
| help pages                 | unchanged — no action/keybind/zone/panel section; the config-reference page is generated from the example |

### Env-overlay treatment (and how it differs from `[[presets]]`)

The env-overlay test walks the schema and puts a key **in scope** only at depth
≤ 1 (`key` or `section.key`); anything whose section path contains a `.` is
structured config, not a knob.

- `[[presets]]` is a **top-level** `Vec<Preset>`, so its fields land at section
  `presets` — depth 1, in scope, and with no `THEGN_PRESETS_*` knobs they are
  pinned: six lines (`presets.commands`, `.cwd`, `.description`, `.layout`,
  `.mode`, `.name`) in `test/env-overlay-ratchet.txt`.
- `[pipeline]` is a **struct field holding an array of tables**. The walker
  treats `stages` as table-like, so it becomes a _section_ (`pipeline.stages`),
  not a key — `[pipeline]` contributes **no depth-1 scalar key at all**, and each
  stage field sits at `pipeline.stages.<key>`, filtered out by the
  `section.contains('.')` guard.

So no knob and no pin: `test/env-overlay-ratchet.txt` is byte-unchanged, and
`every_shallow_key_has_an_env_knob_or_is_pinned` (which also fails on _stale_
entries) passes. A per-stage env override would be meaningless anyway — the
chart is a structure, not a setting.

## Decisions taken

- **`concurrency = 0` is an error, not "disabled".** A stage that can never run
  is a typo; deleting the stage is how you remove one. Silently accepting 0
  would give the Lead a stage it must skip for a reason config never states.
- **`timeout_secs` defaults to 3600.** Long enough for a real implementation
  turn, short enough that a wedged worker surfaces the same day. Advisory at
  every site; the only real watchdog is `session wait --timeout` (milliseconds —
  the skill says "multiply", because that unit mismatch is a live foot-gun).
- **`on_blocked` defaults to `park`.** The conservative exit: the row goes
  `waiting_human` and the slot frees, rather than the Lead retrying into a wall
  or dropping work an operator would have wanted.
- **The first stage is the entry point**, by position — no `entry = true` flag.
  One less field to keep consistent, and it matches how the example reads
  top-to-bottom.
- **No `merger` stage.** Landing is `thegn integrate`: serial fold + gate + CAS
  advance, with the conflict/gate-failure agent handoff already configured under
  `[merge_queue]`. A merger stage would reimplement a queue that exists and lose
  its serialization. The example and the skill both say so where a reader would
  otherwise reach for one.

## Deferred

- **Server-side stage prompt rendering** (`session open --stage <name>` filling
  `STAGE_VARS` from the roster row) — a convenience, but it moves rendering into
  thegn and is the first step toward the driver. Revisit only with the doctrine
  restated.
- **Per-stage sandbox/limits overrides.** `[sandbox.limits]` is a single shared
  ceiling on purpose; a per-stage carve-out is a resource-policy change, not a
  pipeline one.
- **A `pipeline` CLI noun.** The chart is read with `thegn config get pipeline
--json` and driven with the verbs part 1 landed; a noun would need a
  `cli_help::GROUPS` heading and would imply thegn runs something.
