# THE-76 chunk 1 — core policy, `stage.permissions`, the roster field stamp

**Crate:** `thegn-core` only (plus `config/` and `openspec/`).
**Runs:** FIRST. Chunks 2 and 3 both depend on the API you add here.
**Overlap:** none — no other chunk touches any file in this list.
**Read first:** `.thegn/pipeline/THE-76/architect/design.md` (§2 D2/D6, §3, §4).

Everything here is **pure**: no I/O, no subprocess, no filesystem, no tokio.
Facts about git and the filesystem are gathered by the host (chunks 2/3) and
passed in. That is what keeps `thegn-core`'s substrate-free rule and its 95%
line-coverage gate satisfiable.

## Files touched (exact)

1. `crates/thegn-core/src/pipeline_run.rs` — **NEW**
2. `crates/thegn-core/src/lib.rs` — add `pub mod pipeline_run;` (keep the list alphabetical)
3. `crates/thegn-core/src/config_pipeline.rs` — `permissions` field + validation
4. `crates/thegn-core/src/agent_task.rs` — `template_vars()` + literal-brace regression tests
5. `crates/thegn-core/src/store/notification.rs` — `stamp_dispatch_run` trait method
6. `crates/thegn-core/src/db_notification.rs` — its `impl`
7. `crates/thegn-core/src/db_tests.rs` — DB test for the stamp
8. `config/config.toml.example` — document `permissions` in the `[[pipeline.stages]]` block
9. `openspec/changes/improve-agent-pipeline-v2/**` — **NEW** (proposal, design, tasks, delta specs)

## 1. `crates/thegn-core/src/pipeline_run.rs` (new)

Module doc: this is the **mechanism** half of the pipeline — the pure policy a
supervisor's dispatch, verification and wake steps are built from. It renders no
judgment: it decides _whether a claim is true_, never _what to do about it_.
Cross-reference `config_pipeline.rs`'s "structure, not judgment" doctrine and say
explicitly that nothing here advances a stage.

### 1.1 Artifact paths

```rust
/// `.thegn/pipeline/<ISSUE>/<stage>/<row>.md` — the per-issue handoff path.
pub fn artifact_path(issue_id: &str, stage: &str, row_id: i64) -> String;

/// The tracker key with its `<provider>:` prefix stripped (`linear:THE-76` → `THE-76`).
pub fn issue_key(issue_id: &str) -> String;
```

- `issue_key`: take everything after the **first** `:`; if that is empty, use the
  whole string.
- Both components are whitelist-sanitized: keep `[A-Za-z0-9._-]`, map every other
  character (including `/`, `\`, whitespace, control chars, non-ASCII) to `-`,
  collapse runs of `-`, trim leading/trailing `-` and `.`. An empty result
  becomes `issue` (for the key) / `stage` (for the stage).
- **This is a security boundary**, not cosmetics: the result is joined under a
  worktree path and written to. `..` must not survive (it is stripped by the
  leading/trailing-`.` trim; assert it).

Tests (name them for the property, not the input):
`a_provider_prefixed_id_becomes_a_bare_key`,
`a_traversal_attempt_cannot_escape_the_worktree` (`linear:../../etc`,
`..`, `a/../../b`, `linear:`, `""`), `the_row_id_disambiguates_two_coders_of_one_stage`,
`sanitization_is_idempotent`.

### 1.2 Run-completion verdict

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyFacts { pub artifact: Option<String>, pub exists: bool, pub tracked: bool, pub dirty: bool }

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct VerifyReport {
    pub ok: bool,
    pub artifact: Option<String>,
    pub exists: bool,
    pub tracked: bool,
    pub dirty: bool,
    pub reasons: Vec<String>,
}

pub fn verify_report(f: &VerifyFacts) -> VerifyReport;
```

Rule — and document _why_ in the code:

- `artifact == None` ⇒ `ok = true`, no reasons. A row with no artifact is a plain
  (non-pipeline) dispatch; the column has been optional since v56
  (`issue.rs:252-257`) and gating those would break `set-status done` for every
  non-pipeline user while catching nothing.
- otherwise `ok = exists && tracked`, with a reason per miss:
  - not exists → `format!("artifact {a:?} does not exist under the worktree")`
  - exists, not tracked → `format!("artifact {a:?} exists but git does not track it — commit it")`
- `dirty` is **reported, never blocking**. Add a non-blocking note to `reasons`?
  No — keep `reasons` strictly the things that make `ok` false, so a caller can
  print `reasons` verbatim on refusal. `dirty` is its own field.

Tests: `a_row_without_an_artifact_is_never_gated`,
`a_missing_artifact_is_refused_with_a_reason`,
`an_untracked_artifact_is_refused_and_the_reason_says_commit_it`,
`a_dirty_worktree_is_reported_but_never_blocks`,
`a_present_tracked_artifact_passes`.

### 1.3 Wait-target selection

```rust
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct WaitTarget { pub id: i64, pub session_id: String, pub stage: Option<String>, pub issue_id: String }

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitSelectError { NoSuchRow(i64), NotActive(i64, &'static str), NoSession(i64), NoneActive }
// impl std::fmt::Display — operator-language messages; the host bails with them verbatim.

pub fn wait_candidates(rows: &[crate::issue::AgentDispatch], row: Option<i64>)
    -> Result<Vec<WaitTarget>, WaitSelectError>;
```

- `Some(id)`: the row must exist, be waitable, and carry a non-empty
  `session_id` — each miss its own error variant so the message can be specific.
- `None` (the `--any` case): every waitable row with a non-empty `session_id`,
  in roster order. Empty ⇒ `NoneActive`.
- **Waitable = `Spawning | Running`.** Not `Queued` (no session yet), and not
  `WaitingHuman`/`PrOpen` — those are rows whose worker already finished
  (`issue.rs:376-382` groups them under `is_active`, which is a _different_
  question: "don't re-dispatch this"). Including them would make `--any` return
  instantly and forever, starving the real wait. Write that reasoning in the
  doc comment; it is the one thing a future reader will get wrong.

Tests: `only_spawning_and_running_rows_are_waited_on`,
`a_row_without_a_session_is_named_not_silently_skipped`,
`an_empty_roster_reports_nothing_active`,
`selecting_one_row_reports_why_it_is_unwaitable` (all three variants),
`candidates_carry_the_stage_and_issue_for_the_wake_message`.

### 1.4 Claude permission seeding (pure merge)

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermsError { Malformed(String), NotAnObject(&'static str) }
// Display: name the file shape problem; the host prefixes the path.

/// Merge `allow` into a `.claude/settings.local.json` document, preserving every
/// other key. Returns the file's new full text (pretty JSON, trailing newline).
pub fn merge_claude_allow(existing: Option<&str>, allow: &[String]) -> Result<String, PermsError>;
```

- `None` / blank / whitespace-only existing ⇒ start from `{}`.
- Existing text that does not parse ⇒ `Malformed`.
- Root not a JSON object, or `permissions` present and not an object, or
  `permissions.allow` present and not an array of strings ⇒ `NotAnObject(which)`.
  **Never clobber a file we do not understand** — the user's own permissions live
  here.
- Union: existing entries keep their order; new ones append; duplicates dropped.
- Everything else in the document survives byte-for-byte in value (key order need
  not be preserved — `serde_json::Value` is a `Map`; use the crate's default
  behaviour and say so).
- Output: `serde_json::to_string_pretty` + `\n`.

Tests: `an_absent_file_becomes_a_minimal_allow_list`,
`unrelated_keys_survive_the_merge`, `an_existing_allow_list_is_unioned_not_replaced`,
`the_merge_is_idempotent` (run it twice, compare bytes),
`a_malformed_or_wrongly_shaped_file_is_refused_not_overwritten` (each error variant),
`an_empty_allow_list_still_produces_a_valid_document`.

### 1.5 Registry refresh

```rust
/// `base` with only its agent/tool/pipeline registries replaced from `fresh`.
pub fn with_fresh_registry(base: &crate::config::Config, fresh: &crate::config::Config) -> crate::config::Config;
```

Clone `base`; overwrite `agents`, `tools`, `pipeline`. Document why it is
_narrow_: the daemon may have booted with `--set`/`--config` overrides
(`main.rs:1016-1021` layers them) and a wholesale re-load would silently discard
them; the only thing that goes stale in practice is a stage's `agent` name.

Test: `only_the_registries_are_taken_from_the_fresh_config` — set a non-registry
field (e.g. `base_branch`) differently on both and assert `base`'s survives while
the agent list is `fresh`'s.

## 2. `config_pipeline.rs` — `stage.permissions`

Add to `PipelineStage` (after `on_blocked`, keep `#[serde(default)]` on the
struct as-is) and to `Default`:

```rust
/// Tool-permission patterns seeded into `<worktree>/.claude/settings.local.json`
/// when this stage is dispatched (`thegn session open --stage`). thegn does NOT
/// interpret them — they are the harness's own vocabulary
/// (`Bash(git status:*)`, `Read`, `mcp__srv__tool`). Empty = seed nothing.
pub permissions: Vec<String>,
```

Validation in `validate_pipeline` (the existing per-stage loop,
`config_pipeline.rs:208-247`), using the same `label(i, s)` prefix:

- entry empty after trim → `"{label}.permissions[{j}]: empty (a permission pattern must name something)"`
- entry contains a control character → `"{label}.permissions[{j}]: contains a control character"`
- duplicate entry → `"{label}.permissions[{j}]: duplicate of permissions[{k}]"`

Tests beside the existing ones:
`validate_rejects_an_empty_or_control_char_permission`,
`validate_rejects_a_duplicate_permission`,
`a_stage_with_no_permissions_is_valid_and_seeds_nothing`, and extend
`toml_round_trips_with_defaults_for_every_omitted_key` to cover a
`permissions = [...]` stage.

## 3. `agent_task.rs` — `template_vars` + the literal-brace pin

```rust
/// The placeholder names a template references, in order, deduped. Same parser
/// as rendering and validation, so the three can never disagree.
pub fn template_vars(template: &str) -> Result<Vec<String>, TemplateError>;
```

Implemented over the existing `parse` (`agent_task.rs:238-281`). Chunk 3 uses it
to decide whether a stage prompt actually needs the tracker.

Regression tests (this is the item-3 "literal braces" defect — the bug was in the
Lead's hand-rolled substitution, never in this engine; pin the property so it
stays true):

- `a_value_full_of_braces_is_never_reparsed`: render the `TaskKind::Issue`
  default with `issue_body = "query { nodes { name } } and {unclosed"` and assert
  the output contains that string **verbatim** and that rendering is `Ok`.
- `braces_in_a_value_cannot_inject_a_placeholder`: bind `issue_title =
"{issue_body}"` and assert the rendered output contains the literal
  `{issue_body}` and not the body's text.
- `template_vars_lists_what_a_stage_prompt_needs`: including the empty template
  (`[]`), duplicates deduped, and an unterminated template erroring.

## 4. The roster field stamp

`crates/thegn-core/src/store/notification.rs` — declare beside
`update_dispatch_status` (`:154-161`):

```rust
/// Stamp a dispatched row with the session running it and the artifact it will
/// produce. The roster's only field update: `session_id` is the row's identity
/// for pane-exit attribution (`dispatch_for_exit`) and `artifact_path` is the
/// pointer the completion gate checks, and neither is knowable until the row id
/// exists and the session has opened.
fn stamp_dispatch_run(&self, id: i64, session_id: &str, artifact_path: &str) -> Result<()>;
```

`crates/thegn-core/src/db_notification.rs` — impl beside `update_dispatch_status`
(`:326-337`):

```sql
UPDATE agent_dispatches SET session_id=?1, artifact_path=?2 WHERE id=?3
```

No schema change — the columns are v56 (`db_notification.rs:299-321`). Do **not**
bump `SCHEMA_VERSION`.

`db_tests.rs` — `stamp_dispatch_run_records_the_session_and_artifact`: put a row,
stamp it, read it back, assert both fields and that `status` / `stage` /
`parent_id` are untouched; stamping a non-existent id is a no-op `Ok(())` (SQL
`UPDATE` matching nothing) — assert that too and say why it is not an error (the
caller has already checked the id).

## 5. `config/config.toml.example`

In the `[[pipeline.stages]]` key table (`:1505-1521`), add:

```
#   permissions  tool patterns seeded into <worktree>/.claude/settings.local.json
#                when the Lead dispatches this stage with
#                `thegn session open --stage`. thegn does not interpret them —
#                they are the harness's own vocabulary. Empty = seed nothing.
```

and add a `permissions = [...]` line to one of the commented example stages (the
`code` one is the natural home), e.g.:

```
# permissions = ["Read", "Edit", "Bash(git:*)", "Bash(just quick:*)"]
```

`tests/config_example.rs` requires every key to be documented **and** the example
to parse and validate clean — run it.

## 6. openspec change folder

`openspec/changes/improve-agent-pipeline-v2/`:

- `proposal.md` — why (the pilot's hand-rolled Lead loop), what changes, Impact
  citing `tasks.md` group + THE-76.
- `design.md` — port §2 (Decisions D1–D6) of the architect design; that is where
  the "why not a reload verb / why not in the daemon" reasoning belongs.
- `tasks.md` — the three chunks as task groups.
- delta specs, `## ADDED Requirements` with `### Requirement:` + `#### Scenario:`
  (WHEN/THEN), under:
  - `specs/agent/spec.md` — stage dispatch, permission seeding, the
    run-completion contract, the wake primitive, daemon registry freshness.
  - `specs/cli/spec.md` — `session close`, `session list --live`,
    `dispatch verify`, `dispatch wait`, the gated `set-status done`.

Cover at minimum these scenarios (they are the pilot's actual failures):

- WHEN a stage's rendered prompt is empty THEN the dispatch is refused and no
  session is opened.
- WHEN a roster row's artifact is written but not committed THEN
  `set-status done` is refused and names the artifact.
- WHEN a row carries no artifact THEN `set-status done` is not gated.
- WHEN an issue body contains literal braces THEN the rendered stage prompt
  contains them verbatim.
- WHEN an existing `.claude/settings.local.json` holds unrelated keys THEN they
  survive the seed.
- WHEN the session open fails THEN the roster row is left `failed`, not `queued`.

Validate with `just openspec-validate` (fast; it is the node CLI, not a compile).

## Tests to run (scoped — nothing full-workspace)

```sh
just quick thegn-core
cargo nextest run -p thegn-core pipeline_run
cargo nextest run -p thegn-core config_pipeline
cargo nextest run -p thegn-core agent_task
cargo nextest run -p thegn-core dispatch      # db_tests roster cases
cargo nextest run -p thegn-core config_example
just openspec-validate
```

Do **not** run `just test`, `just ci`, `just coverage`, `just smoke`, or e2e, and
do not start any full-workspace compile.

## Done criteria

- [ ] `pipeline_run` exists, is pure (no `std::fs`, no `std::process`, no tokio,
      no `Db`) and every public item has a unit test; the traversal and
      malformed-file cases are covered.
- [ ] `PipelineStage.permissions` parses, round-trips, validates, and is
      documented in `config/config.toml.example`.
- [ ] `agent_task::template_vars` exists; the literal-brace property is pinned by
      a test.
- [ ] `stamp_dispatch_run` exists on the store trait and the `Db` impl, with a
      DB test; `SCHEMA_VERSION` unchanged.
- [ ] The openspec change folder validates strict.
- [ ] No new `let _ =` / `.ok()` without a `// best-effort:` reason
      (`test/ignored-result-ratchet.txt`).
- [ ] Scoped tests above are green.

**Commit subject (exact):**

```
feat(pipeline): stage permissions, artifact paths and run-completion policy (THE-76)
```

Also write your summary to the artifact path your roster row carries and commit
it in the same commit.
