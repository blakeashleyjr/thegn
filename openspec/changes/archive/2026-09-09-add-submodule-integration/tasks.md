# Tasks — submodule integration

## 1. Core correctness

- [x] 1.1 Parse strict `.gitmodules` metadata and reject paths escaping the
      checkout root, with fixture/unit coverage.
- [x] 1.2 Recognize gitlinks in patch/diff models and reject partial selection
      while preserving whole-entry stage/restore.
- [x] 1.3 Model pointer state, forward/rewind/diverged direction, summaries,
      and conflicts as pure data with tests.

## 2. Reads and rendering

- [x] 2.1 Implement the service Git submodule adapter with metadata-gated,
      bounded local reads and no fetch side effects.
- [x] 2.2 Classify submodule change rows and render pointer moves/local commit
      summaries without misleading line counts.
- [x] 2.3 Add a distinct configurable sidebar submodule indicator and update
      model/cache serialization tests.

## 3. Lifecycle and merge

- [x] 3.1 Add `[git] submodules = "auto" | "off"` and a shared strict,
      trust-gated, recursive post-checkout initializer.
- [x] 3.2 Invoke the initializer after CLI/UI worktree creation, ordinary
      workspace clone, and remote/provider checkout; surface non-fatal failure.
- [x] 3.3 Report gitlink conflicts with path and both SHAs and exclude them from
      text conflict-driver/rerere handling.

## 4. Documentation and reconciliation

- [x] 4.1 Document lifecycle, trust, atomic staging, view, and sidebar behavior
      in configuration/help surfaces.
- [x] 4.2 Reconcile this change with the shipped adapter/post-clone design and
      validate it with `openspec validate add-submodule-integration --strict`.
