# Tasks — external IDE handoff

## Phase 1 — editor seam: project-level launch (pure core, 95% gate)

- [ ] 1.1 Extend `program_profile` with the project-open dimension
      (`code`/`idea`-family/`zed`/`subl` root argv; terminal editors → `Pane`
      at root) + project form of `EditorRequest`; template rendering with
      root-as-`{path}` and absent line/column. Unit tests beside the existing
      jump-syntax tests.
- [ ] 1.2 Reveal-payload model (`{repo, path, line?, col?}`) + pure worktree
      resolution (canonicalized path-prefix against registered roots;
      relative-path and ambiguity rules) with traversal/symlink-escape
      rejection tests.
- [ ] 1.3 Strict `thegn://open` parser beside `parse_app_link` (allowlist
      params, bounded ints, unknown-param rejection), unit-tested.

## Phase 2 — outbound action

- [ ] 2.1 `open-in-ide` action id: palette entry + sidebar worktree-row menu
      entry; spawns through the seam chokepoint (`spawn_detached_reaped` /
      center pane per placement). No new config keys.
- [ ] 2.2 `docs/help/ide-handoff.md` claiming `open-in-ide` (help + prose
      ratchets); one-line row-menu mention in `docs/help/sidebar.md`.
- [ ] 2.3 Re-record any muse baseline showing the sidebar row menu
      (`just e2e-update`, review the diff).

## Phase 3 — inbound reveal

- [ ] 3.1 `thegn open <repo> --file <path>[:<line>[:<col>]]` (grammar per
      `add-cli-namespaces-and-remote-open` conventions; `--json`/exit-code
      contract respected) → enqueue `reveal_file` intent; launch path
      enqueues before falling through.
- [ ] 3.2 Compositor claim handler: focus workspace → select resolved
      worktree tab → open via `panel_util::open_editor` (placement rules
      apply); miss/ambiguity → status line. Full-frame damage only on claim.
- [ ] 3.3 `worktrees.open` optional `path`/`line`/`col` fields on HTTP +
      gRPC + CLI projections, feeding the same intent; coordinate proto
      edits with `complete-control-surface-coverage`'s gRPC parity work; no
      new catalog row; existing `required_scope` unchanged. Schema/round-trip
      tests in `control_schema.rs`.

## Phase 4 — `thegn://` end to end

- [ ] 4.1 `thegn url <link>` hidden verb: dispatch `open` → cmd/open path,
      `pair` → existing interactive pairing flow; everything else exits
      non-zero with one line. Unit-test the dispatch decision.
- [ ] 4.2 `.desktop` entry gains `MimeType=x-scheme-handler/thegn;` and `%u`;
      `packaging/macos/make-app.sh` emits `CFBundleURLTypes`; `install.sh`
      dry-run output names both. (Windows registry: deferred,
      windows-parity.)
- [ ] 4.3 Smoke-test the dispatcher (`test/smoke.sh`: url → open path on a
      fixture repo; malformed URL exit code).

## Phase 5 — extension contract + docs

- [ ] 5.1 `docs/extending/ide-extension.md`: pairing handshake, scope
      guidance (minimum scopes), `worktrees.list`/`worktrees.open{path,line}`
      /`events.subscribe` loop, jump-to-file in both directions, explicit
      "no bespoke RPC" statement.
- [ ] 5.2 Cross-check with `complete-control-surface-coverage`: reveal fields
      appear in its coverage report unchanged (same row); audit records cover
      the reveal form.

## Phase 6 — gate

- [ ] 6.1 `openspec validate add-external-ide-handoff --strict`.
- [ ] 6.2 Run `just ci` once, pre-PR (includes openspec-validate; deliberate
      heavy run via `THEGN_ALLOW_HEAVY=1`).
