# Chunk 3 — architecture, help, and OpenSpec synchronization

## Scope

Make the repository’s normative architecture and user-facing configuration
help agree with the audited implementation and the two code chunks. Synchronize
the existing THE-38 OpenSpec draft by pruning claims already satisfied on this
branch and describing only the remaining improvements. Do not add code or a
new help page/action.

## Files touched

- `docs/ARCHITECTURE.md` — correct §7’s stale layer and `--strict` statements;
  document TOML-only trusted layers, repo tri-format precedence, actual repo
  tables, tolerant load/explicit validation, and existing gates/ratchets.
- `docs/help/configuration.md` — reconcile the contradictory repo-overlay
  paragraphs, remove every nonexistent `--strict` claim, document profile
  order, repo candidate precedence/shadow warning, and say the generated
  reference contains example values rather than asserting code-default parity.
- `openspec/changes/align-config-formats-and-validation/proposal.md` — revise
  the verified findings, non-goals, impacts, and implementation scope.
- `openspec/changes/align-config-formats-and-validation/design.md` — align the
  draft design with the actual core/host module seams and diagnostic shape.
- `openspec/changes/align-config-formats-and-validation/tasks.md` — replace
  stale broad tasks and prohibited full-CI instruction with the three scoped
  implementation chunks and ratchet checks.
- `openspec/changes/align-config-formats-and-validation/specs/config/spec.md`
  — update scenarios/requirements for TOML-only trusted layers, supported
  repo formats, all-layer validation, path-prefixed diagnostics, and doctor
  health.

## Approach

1. Keep the architecture source of truth honest: trusted `Config` and profile
   documents are TOML; repo-local files are the only tri-format exception;
   `--set` and `THEGN_*` are value overlays, not document readers.
2. State the real repo tables (`sandbox`, `keybinds`, `notifications`, `issues`,
   `env`, and metrics detection/refusal) and distinguish trust clamping from
   validation. Do not claim repo overlays are `[sandbox]` only.
3. Remove all `config validate --strict` prose because validation is already
   the strict command and `Action::Validate` has no such option. Preserve the
   separate reserved-provider semantics if the page documents them.
4. Describe the hand-authored example and generated reference accurately:
   schema/example coverage is enforced, the runtime page is generated from
   the example, and its values are illustrative example values unless a
   future defaults gate is added. Keep that defaults comparison explicitly
   deferred rather than inventing a noisy ratchet.
5. Synchronize the in-flight OpenSpec change only. Do not edit canonical
   `openspec/specs/config/spec.md`; it is updated when the implemented change
   is ready to archive. Mark tri-format repo parsing, profile loading, example
   coverage, generated page registration, home-manager drift, and existing
   trust clamping as already present and scope only the gaps to code chunks.
6. Record ratchet impact accurately: no new config key means no env-overlay,
   home-manager, example, or enum-count entry; no new help/action means help
   ratchets are unchanged; `--repo` is cataloged structurally by chunk 2; no
   control-schema snapshot changes.

## Overlap and dependency

No file overlap with chunks 1 or 2. This chunk is implementation-independent
and may run in parallel. Its wording should be reviewed against the landed
core API before commit, but it must not modify source files to resolve a prose
disagreement.

## Tests to run

Run scoped documentation/registration checks:

- `just quick thegn-core`
- `cargo nextest run -p thegn-core config_reference`
- `just quick thegn-host`
- `cargo nextest run -p thegn-host help`
- `just openspec-validate`

Do not run `just test`, `just ci`, a full-workspace compile, or e2e. If the
repository has a docs-only validation target, it may be used in addition to
the commands above, but do not update snapshots without a real behavioral
change.

## Done criteria

- `docs/ARCHITECTURE.md` and `docs/help/configuration.md` contain no stale
  `[sandbox] only` or `--strict` claims and accurately explain the trust-tier
  format contract and layer order.
- The four OpenSpec change files validate strictly and describe the actual
  code-backed behavior, including shadow warnings, all-layer validation,
  path/key diagnostics, and doctor health.
- The docs preserve the architecture invariants: 0% idle, substrate-free
  tested core, edge degradation, one capability catalog, and no god-file
  growth. They do not request JSON/YAML trusted readers or autogeneration of
  the curated example.
- No canonical spec, ratchet allowlist, control snapshot, or unrelated help
  page is changed.
- Commit with exactly:

  `docs(the-38): document configuration audit contract`
