# Tasks — sidebar actions and mouse

- [x] Replace ambiguous close/delete keys with row-specific safe/danger choosers
      and preserve dirty-tree safeguards.
- [x] Add keyboard creation, rename, branch, folder, copy, help, and explicit
      sort-menu paths with canonical action specs.
- [x] Make context-menu v2 the shared per-row action catalog and route keyboard/
      mouse outcomes through one dispatch path.
- [x] Add renderer-derived hit rows, right-click, caret/Ctrl/double-click,
      wheel, drag/reorder/file/unfile, edge scroll, and feedback with keyboard
      parity and terminal-capability gating.
- [x] Add sidebar help/statusbar discovery and update product vocabulary.
- [x] Cover menus, keys, outcomes, hit geometry, drag state/targets, and help
      invariants with focused tests and the recorded gate suite.
- [x] Land the release overhaul as `3d9bb40f` and strictly validate this change.

## Validation boundary

No separate exhaustive historical live-TUI walkthrough is claimed. Automated
gesture/model tests and shared-dispatch invariants are the accepted evidence.
