# Tasks — cross-profile session move

## 1. Catalog and planning

- [x] 1.1 Add CLI-only Admin catalog capability `sessions.migrate` and retain
      the Admin-never-on-MCP/plugin coverage invariant.
- [x] 1.2 Resolve inactive profile roots without rerooting or loading target
      config, including socket/path edge coverage.
- [x] 1.3 Build and test a pure allowlisted move plan covering groups/tabs,
      worktree registration, exact group UI keys, dispatches/notes, active
      running-pin state, liveness blockers, collisions, and payload warnings.

## 2. Store safety

- [x] 2.1 Implement target-first transactional import, sanitized fingerprint
      readback, exact source deletion, daemon-ID clearing, and idempotent resume.
- [x] 2.2 Test collisions, target-owned registration preservation, crash-window
      duplication/resume, excluded rows, and the credential-column boundary.
- [x] 2.3 Keep dry-run read-only even for absent/stale target stores and ensure
      human/JSON audit output never emits opaque payload contents.

## 3. CLI behavior

- [x] 3.1 Add `session move <worktree> --to-profile <name> [--kill]
[--dry-run] [--json]` with exact-path selection and complete reporting.
- [x] 3.2 Refuse any running source profile; fail closed on uncertain daemon
      liveness; kill and re-list only after explicit `--kill`.
- [x] 3.3 Send a best-effort non-secret target-daemon notification only after a
      confirmed move.
- [x] 3.4 Cover cold, kill, collision, resume/read-only, and redacted dry-run
      behavior with hermetic tests/smoke fixtures.

## 4. Documentation and reconciliation

- [x] 4.1 Document the cold move, exact transferable boundary, liveness/kill
      rules, opaque payload warning, and retry behavior.
- [x] 4.2 Reconcile this change with the shipped implementation and validate it
      with `openspec validate add-session-profile-migration --strict`.
