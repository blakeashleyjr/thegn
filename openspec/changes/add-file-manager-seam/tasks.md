# Tasks — file-manager provider seam

## 1. Seam vocabulary (thegn-core)

- [ ] 1.1 New `file_manager.rs`: `FileManagerCaps`, `DrawerSpawn`, `DrawerCmd`
      (moved from host `queries.rs` grammar), the object-safe sync
      `FileManager` trait, and `FileManagerError` implementing `SeamError`.
- [ ] 1.2 `config_enum!` `DrawerKind` (`yazi` default, `custom`; `lf`, `broot`
      reserved) + `[drawer] kind` on `DrawerConfig`.
- [ ] 1.3 Back-compat resolution: `effective_kind(cfg)` (`kind` unset +
      non-empty `command` ⇒ custom; else yazi) + strict-validate warning for
      `kind = "yazi"` with a non-empty `command`. Unit tests for every arm.

## 2. Yazi behind the seam (thegn-core)

- [ ] 2.1 Re-home `thegn_core::yazi` as the yazi `FileManager` impl: `bin`
      resolution, `config_home`, `ensure_config` → `prepare`, `write_theme` →
      `apply_theme`, OSC decode → `control`. Existing unit tests carry over.
- [ ] 2.2 `custom` impl: argv from `[drawer] command` (shell-words split as
      today), empty caps, `prepare` = None, `control` = None.
- [ ] 2.3 Factory `file_manager_for(cfg)`; reserved kinds return none and are
      rejected by `config validate --strict` (extend the config_enum
      round-trip + reserved-kind tests).

## 3. Host plumbing goes manager-agnostic (thegn-host)

- [ ] 3.1 `drawer_state.rs` / spawn pipeline: obtain `DrawerSpawn` from the
      factory; no `yazi::` call from generic paths; pool/prewarm/flags/layout
      unchanged for every kind.
- [ ] 3.2 PTY drain: scan for drawer control bytes only when
      `caps().control_channel`; dispatch through the existing
      `dispatch_drawer_command` (editor path via the editor seam).
- [ ] 3.3 Containment wrap applies to every kind (no yazi condition).
- [ ] 3.4 A unit test asserting generic drawer modules reference no
      `yazi`-named symbol (vendor-isolation guard).

## 4. Probe + doctor

- [ ] 4.1 `Probe` impl per provider (binary resolution, config-home mode,
      caps; inert-key notes for non-yazi kinds); register in the doctor
      provider listing.
- [ ] 4.2 Conformance: probe-shape suite covers the new seam; smoke test
      asserts the drawer row appears in `thegn doctor`.

## 5. Config + docs

- [ ] 5.1 Document `[drawer] kind` (and the back-compat rule) in
      `config/config.toml.example`.
- [ ] 5.2 Update `docs/help/` drawer/file-explorer page prose for the
      provider wording (the config-reference page is generated — do not
      hand-write it); help ratchet stays green (no new action ids).

## 6. Validation

- [ ] 6.1 Update `openspec/specs/file-explorer/spec.md` via `/opsx:sync` on
      completion.
- [ ] 6.2 Run `just ci` once, pre-PR (includes openspec validate).
