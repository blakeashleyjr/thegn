# THE-19 architect review — verdict 4

REVISE

The branch was first merged with current `main` and then reviewed as the full `git diff main...HEAD`.

Merge and review corrections:

- `6982dd42` — merge `main` into `tg/the-19-pre-post-scripts`.
- `56c258c5` — restore the active-profile and strict-validation configuration documentation lost during conflict resolution.
- `fd6ce5b3` — preserve the merge queue entry when physical cleanup fails; make lifecycle test commits independent of the reviewer’s global GPG setting.

Required revision chunks:

- `.thegn/pipeline/THE-19/architect-review/revision-4.md` — `crates/thegn-host/src/worktree_lifecycle.rs:77-198` performs synchronous DB/cache work and live pane-state capture from the compositor completion path; move that work to workers/db-task and leave the loop with pure in-memory reconciliation. The same chunk covers the missing `hooks.<event>` list/approve path in `crates/thegn-host/src/cmd/repos.rs:113-163`.

Verification:

- Core mandatory nextest expression: 527 passed.
- Host mandatory nextest expression: 104 passed (rerun after the review correction).
- `thegn-svc --test control_schema`: 1 passed.
- `just quick`: passed.
- Clippy with `-D warnings`: `thegn-core` and `thegn-host` passed.
- Rustdoc with `-D warnings`: `thegn-core` and `thegn-host` passed.
- Focused lifecycle/worker suite: 51 passed.
- `git diff --check`: passed.

Unverified/environmental items:

- `openspec validate --all --strict` could not run because `openspec` is not on PATH.
- Direct `treefmt` could not run because its `taplo` formatter dependency is not on PATH; the repository pre-commit formatter completed successfully for each review commit.
- `test/ratchet-check.sh` is not present.
- The PATH `thegn` binary rejects `dispatch`, so `thegn dispatch report` is unavailable.
- The requested understand-anything graph overlay could not be produced because `.understand-anything/knowledge-graph.json` is absent; the review proceeded from the full diff and lane documents.
- No live-state DB invocation, migration, or forbidden full CI/e2e command was run.
