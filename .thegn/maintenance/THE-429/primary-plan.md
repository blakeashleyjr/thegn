# Primary implementation approval: THE-429

Approve the removal approach in investigation484. Remove the automatic checkout-hook installer and branch-controlled hook execution. Do not create a hook trust framework or duplicate THE-371 shared-config repair. Keep existing generated pre-commit/pre-merge/pre-push gates intact.

Implementation requirements:

- Remove hookExtras post-checkout copying and the supported executable post-checkout payload (delete the tracked unsafe hook, or replace it with an explicitly inert compatibility notice if references require it). No automatic config seeding with ln -sf and no automatic healer execution. Preserve PREK_ALLOW_NO_CONFIG behavior only as existing optional developer compatibility, not a new way to waive tests.
- Preserve explicit developer commands and document their trust boundary. A user deliberately invoking just heal-git remains outside automatic checkout; do not claim the shell healer is race-safe. No changes to shared-config repair implementation in this issue.
- Do not implement automatic deletion/migration of existing hooks: an exact-content check followed by unlink still races, and no reviewed atomic ownership primitive exists. Provide a bounded, read-only legacy hook detector and actionable migration guidance scoped to the verified repository-local hooks directory. Never follow hook symlinks or read FIFOs/devices; refuse custom/global/shared hooksPath and ambiguous ownership. Unknown files remain untouched. Explain that old installed copies must be retired explicitly and remain unsafe until then.
- Current repository has an exact known legacy hook digest recorded in484. Primary will inspect, preserve and explicitly retire this known local installation after source/test review; worker must not touch the live shared hooks directory. Do not claim deployment cleanup completed in your report. General users receive clear upgrade instructions, no silent deletion of their hooks.
- Prefer a small immutable Nix-store read-only detector invoked by trusted dev-shell setup if a new helper is necessary. Do not have a Git checkout hook execute a current-branch helper. No automatic code execution merely to detect old hooks. Diagnostics should be bounded and truthful.

Regression requirements:

1. Hermetic Git repositories with malicious branch hook/healer sentinel payloads; exercise ordinary checkout, worktree add and the production native creation command seam with the repaired setup. Assert no payload runs, no per-worktree config is overwritten, and normal operations succeed.
2. Existing foreign hook files, directories, live/dangling symlinks and FIFOs remain byte/path/mtime unchanged. Custom/global hooksPath is not inspected or mutated. Multiple worktrees and concurrent setup preserve the same invariant.
3. Legacy detector identifies exact known owned regular bytes only, handles size bounds and changed leaves safely, and reports required manual cleanup. It performs zero writes in all cases; race tests should prove that, not pretend a check/unlink sequence is atomic.
4. Existing pre-commit/pre-push gate configuration and explicit CI/merge gate commands stay intact. Source assertions alone are insufficient for no-execution; run actual safe fixture setup/checkout commands. Python/shell focused tests may run in the worker, but no Cargo/builds/full suites.

Document acceptance mapping and the explicit legacy deployment step. Commit implementation, focused tests/results and artifact; report implementation-ready, with Rust/full gate pending and live legacy cleanup not yet performed. No main edits, merge, push or issue closure.
