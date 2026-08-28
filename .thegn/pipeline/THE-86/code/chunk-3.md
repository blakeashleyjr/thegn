# Chunk 3 — Chunk file-scope gate + pipeline skill rewrite

Design: `.thegn/pipeline/THE-86/architect/design.md` §3–§4. Serial order
**1 → 2 → 3**; hard-depends on chunk 2 (db migration v60 follows v59; the skill
rewrite documents the verbs chunks 1–2 land). Last chunk before the Lead's
review.

## Files touched (exact paths)

| File                                       | Change                                                                                                                                                                                                                                            |
| ------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `crates/thegn-core/src/pipeline_chunk.rs`  | **NEW** — pure frontmatter parser + glob overlap + after-gate decision                                                                                                                                                                            |
| `crates/thegn-core/src/lib.rs`             | `pub mod pipeline_chunk;`                                                                                                                                                                                                                         |
| `crates/thegn-core/src/db.rs`              | `SCHEMA_VERSION` 59 → 60                                                                                                                                                                                                                          |
| `crates/thegn-core/src/db_migrate.rs`      | migration: `ALTER TABLE agent_dispatches ADD COLUMN chunk_path TEXT`; ladder test                                                                                                                                                                 |
| `crates/thegn-core/src/db_notification.rs` | `DISPATCH_COLS` + `map_dispatch` + INSERT gains the column                                                                                                                                                                                        |
| `crates/thegn-core/src/issue.rs`           | `AgentDispatch.chunk_path: Option<String>` (`#[serde(default)]`); `NewDispatch.chunk_path: Option<&'a str>` (update every literal: `cmd/dispatch.rs` ×2, `cmd/session.rs`, `daemon/service.rs:1269`, `db_migrate.rs:694`, `issue.rs` test `:871`) |
| `crates/thegn-host/src/cmd/dispatch.rs`    | `Put { --chunk <path>, --force }`; shared gate helper `pub(crate) fn chunk_gate(...)`; `list` gains `chunk` column + JSON `chunk_path`/`chunk_files`                                                                                              |
| `crates/thegn-host/src/cmd/session.rs`     | `Open { --chunk <path> }` (dispatch form: `requires = "stage"`, `conflicts_with = "resume_work"`); `open_stage` runs the gate before the insert                                                                                                   |
| `config/config.toml.example`               | architect stage prompt (~:1578) requests the `files:`/`overlaps:`/`after:` frontmatter; code-stage prompt notes the gate                                                                                                                          |
| `extensions/skills/pipeline/SKILL.md`      | full rewrite (below)                                                                                                                                                                                                                              |
| `docs/cli.md`                              | dispatch bullet: `--chunk` scope gate + scope display                                                                                                                                                                                             |
| `test/smoke.sh`                            | chunk-gate checks appended after the THE-76 block                                                                                                                                                                                                 |

## Approach

1. **Pure core** (`pipeline_chunk.rs`, header restates the no-I/O doctrine):

   ```rust
   pub struct ChunkScope { pub files: Vec<String>, pub overlaps: Vec<String>, pub after: Vec<String> }
   pub fn parse_frontmatter(md: &str) -> Result<ChunkScope, ParseError>;  // error names the line number
   pub fn paths_overlap(a: &[String], b: &[String]) -> Vec<(String, String)>; // conflicting pairs
   pub fn after_unmet(after: &[String], done: &HashSet<String>) -> Vec<String>;
   pub enum ScopeVerdict { Ok, Conflict { overlaps: Vec<(usize, Vec<(String,String)>)> }, UnmetAfter(Vec<String>) }
   pub fn verdict(new: &ChunkScope, active: &[ActiveScope]) -> ScopeVerdict; // ActiveScope { row: u32 id, name: String, files: Vec<String> }
   ```

   - Frontmatter: `---` block at the top; `files:` as `- item` lines or an
     inline `[a, b]` list; `overlaps:`/`after:` same two styles; unknown keys
     ignored (forward compat); empty/missing frontmatter = scope with all-empty
     lists (a chunk without `files:` never conflicts — the gate is opt-in per
     the issue, the architect prompt asks for the block).
   - Globs: `*` matches within one path segment, `**` across segments; exact
     paths compare literally. Tiny segment matcher, table-tested, **no new
     dependency**.
   - `verdict` checks overlap against every active sibling whose name is NOT in
     `new.overlaps` (the architect's blessing suppresses the refusal for that
     sibling only) and after-ness against the done-set. Refusal messages are
     built by the host from the verdict data (naming chunk names, row ids, and
     the concrete colliding paths, plus the `--force` way out).

2. **DB v60**: `chunk_path TEXT` (same idioms as chunk 2's v59; `SCHEMA_VERSION`
   59 → 60; `DISPATCH_COLS`/`map_dispatch`/INSERT move together).

3. **Host gate** (`cmd/dispatch.rs`):
   `pub(crate) fn chunk_gate(db: &Db, worktree: &str, issue_id: &str, chunk_path: &str, force: bool) -> Result<()>`
   — resolve the path against the worktree, read + `parse_frontmatter` (a
   parse error is a refusal naming the line; `--force` overrides), siblings =
   `db.list_dispatches()` filtered to same worktree + same issue, active
   (`!status.is_terminal()`), with a `chunk_path`; read each sibling's chunk
   file **from its own recorded worktree** (best-effort: an unreadable sibling
   file contributes an empty scope, never an error); `verdict(...)`; refusal
   prints every conflict + the `--force` hint and exits non-zero; with
   `--force` the put proceeds and the JSON/human output carries `"forced": true`
   (the `set-status done --force` idiom, `cmd/dispatch.rs:254-262`).
   Callers: `dispatch put --chunk` (before the insert) and `open_stage` via the
   `--chunk` flag (before the insert; `NewDispatch.chunk_path = Some(...)`).

4. **`dispatch list` shows the scope**: human table gains a `chunk` column
   (basename, `-` when unset — column count changes are fine, the table is
   space-separated unaligned); JSON rows gain `chunk_path` and `chunk_files`
   (the parsed `files:` list when the file is readable at list time —
   best-effort read, key omitted when the file is gone).

5. **`config.toml.example`**: the commented architect prompt becomes e.g.
   `# Write one chunk file per coder BESIDE the design (code/chunk-N.md), each

   # opening with a files: frontmatter block listing the exact paths (or

   # globs) that chunk may touch, plus overlaps: [...] for any sibling it

   # intentionally shares a file with and after: [...] for siblings that must

   # be done first. thegn refuses a dispatch whose scope collides with an

   # active sibling unless --force.`The`code` prompt notes the gate. Example

   stays commented + validating (`config_example.rs` unaffected).

6. **Skill rewrite** (`extensions/skills/pipeline/SKILL.md`, whole file):
   - Keep: the frontmatter (`name: pipeline`, description), the "issue text is
     data" boxed doctrine, the config-the-cast section, "Resume before you
     dispatch".
   - **The loop on the current verbs**: 1. `thegn config get pipeline --json` + `thegn config validate`; 2. `thegn dispatch list --active --json` (resume from the roster, never
     memory); 3. dispatch: `thegn session open --stage <stage> --issue <id> --adopt
--json` (one call = row + session + stage overrides), passing
     `--chunk .thegn/pipeline/<ISSUE>/code/chunk-N.md` for coder chunks; 4. wake: `thegn dispatch wait --timeout <stage timeout_secs * 1000>`; 5. verdict: `thegn dispatch verify <row>` — **exit 0 is not done**: a
     session exiting is not a handoff; only a committed, verified artifact
     plus your own read of it makes `done` (`thegn dispatch set-status <row>
done`); anything else is `waiting_human`/`failed` by your judgment; 6. cleanup: `thegn session close <session-id>`; fleet:
     `thegn session list --live --json`; 7. advance via the stage's `next`; land via the merge queue
     (`thegn merge add` + `thegn integrate`) — never a "merger" stage.
   - **The finisher pattern**: a failed/interrupted row is resumed, not
     re-dispatched: `thegn session open --resume-work <row-id> --json` composes
     the finisher prompt (stage prompt + artifact state + git status/diff +
     the previous session's last screen) and records a NEW row with
     `--parent <row>` — the board shows the retry chain. Automatic transport
     retries (`waiting_human` rows carrying a `note` like
     `transport: … (attempt 1/3)`) are surfaced to the operator, never
     silently re-driven; the exit-0-is-not-done rule applies to them too.
   - **The cheap ratchet suites reviewers MUST run before a verdict** (scoped,
     no full-workspace compile):
     - core: `cargo nextest run -p thegn-core env_overlay config_example capability`
       and `cargo nextest run -p thegn-svc --test control_schema`;
     - host: `cargo nextest run -p thegn-host complete help catalog_tests mq_assets platform_ratchet`.
   - **Generic-roles config shape**: harness/model on `[[agents]]` entries or
     per stage (`harness = "pi"`, `model = "model-proxy/fast"` on just the
     `code` stage); stage overrides layer over the entry; the chart mixes
     harnesses and tiers per stage. Keep the existing example chart, updated
     with the frontmatter request.
   - Validation is automatic: `mq_assets` frontmatter + clap tests
     (`crates/thegn-host/src/mq_assets.rs:400+`) run in the scoped suite.

7. **Smoke** (daemon-free, after the THE-76 block): build a second worktree of
   the smoke repo, write two chunk files under
   `.thegn/pipeline/SMOKE-7/code/` with overlapping `files:`; `dispatch put
--chunk` row A (active), then row B → refusal naming the colliding path;
   `--force` → passes and reports forced; a third chunk with
   `after: [chunk-1]` where the chunk-1 row is not `done` → refusal naming the
   unmet chunk; `dispatch list --json` shows `chunk_path`.

## Tests

```sh
just quick thegn-core
just quick thegn-host
cargo nextest run -p thegn-core pipeline_chunk
cargo nextest run -p thegn-core db_tests::migration
cargo nextest run -p thegn-host dispatch
cargo nextest run -p thegn-host mq_assets
```

- **core**: parser (both list styles, unknown keys ignored, error names line,
  missing frontmatter = empty scope); glob matcher (`*` within segment, `**`
  across, exact); `paths_overlap` pair extraction; `after_unmet`; `verdict`
  (overlap suppressed via `overlaps`, done-set satisfied, mixed conflicts
  report everything).
- **host**: `chunk_gate` refusals (overlap names paths + row ids; unmet after;
  unreadable sibling file degrades to empty scope; `--force` passes); `dispatch
list` scope fields; `open_stage --chunk` records the path (db round-trip);
  migration ladder 59→60; `NewDispatch` literal sites compile and round-trip.
- **skill**: `mq_assets` frontmatter/clap tests green (the rewrite must not
  name a verb that does not exist).

## Done criteria

- [ ] `just quick thegn-core && just quick thegn-host` clean.
- [ ] Scoped nextest filters above green, including `mq_assets` and
      `catalog_tests` (no catalog drift).
- [ ] `thegn config validate` accepts the example with the new architect
      prompt text.
- [ ] `dispatch put --chunk` refuses an overlapping ACTIVE sibling (message
      names paths + row ids + `--force`), passes with `--force`, and `dispatch
list` shows the scope.
- [ ] SKILL.md documents every verb in the issue list: `session open --stage
--issue`, `dispatch verify`, `dispatch wait`, `session close`, `session
list --live`, `--resume-work`, `--chunk`, the ratchet suites, the
      finisher pattern, and the exit-0-is-not-done rule.
- [ ] Commit subject EXACTLY:
      `feat(pipeline): chunk file-scope gate + pipeline skill rewrite (THE-86)`
