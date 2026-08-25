# Tasks — submodule integration

Testing ground rules (repo-memory traps that bite exactly this change):
submodule fixtures clone from local paths, which modern git blocks — fixture
setup must pass `-c protocol.file.allow=always` (test-only; production never
sets it) — and every fixture commit passes `-c commit.gpgsign=false` or a
global signing config hangs the suite at a pinentry. Anything opening the DB
isolates `XDG_STATE_HOME`. Use `cargo nextest run -p <crate> <filter>` while
iterating; full gates once at the end.

## 1. Pure core: classification + patch engine (thegn-core, 95% gate)

- [ ] 1.1 `.gitmodules` parser (paths + URLs, pure, mtime-keyed cache
      struct); reject path entries that escape the checkout root.
- [ ] 1.2 `FileKind::Submodule` in `patch.rs` (`160000` modes, `Subproject
commit` lines); `render_patch` round-trip fixtures for gitlink
      add/move/delete.
- [ ] 1.3 `Selection` validation: a partial gitlink selection is rejected at
      the pure layer; whole-entry selection maps to add/restore, not
      `git apply`.
- [ ] 1.4 Same recognition in `forge::model::parse_unified_diff`
      (`DiffFile` gains the submodule flag) with fixtures.
- [ ] 1.5 Pointer-move summary model: direction classification
      (forward/rewind/diverged) as pure logic over ancestor facts.

## 2. Reads (thegn-svc + hydrate)

- [ ] 2.1 `GitBackend::submodule_states` defaulted method (CLI impl:
      `submodule status --recursive` + status rows), skipped when the
      `.gitmodules` set is empty; batched into `glyph_reads` / the bridge
      batch.
- [ ] 2.2 `GlyphReads.submodule_dirty` + `GlyphRow` 9th slot — update
      `glyphs_from_row`, `merge_glyph_scan` (+ its five `glyph_scan_*`
      tests), warmcache serde, and the `glyph_cache` JSON default-on-missing
      together.
- [ ] 2.3 Change-row classification: rows whose path is a submodule are
      flagged in `build_change_rows`; numstat `-\t-` no longer renders as
      `+0/-0`.

## 3. Rendering (thegn-host)

- [ ] 3.1 Changes/diff row: `⊂ <path> old → new` (glyph via
      `caps::active_glyphs()`; both literals ratchet-clean).
- [ ] 3.2 Drilled view: off-loop bounded `log --oneline old..new` summary,
      degrading to bare SHAs; direction label.
- [ ] 3.3 Sidebar badge + `[ui]` visibility toggle wired like the existing
      `sidebar_show_status_icon` family.

## 4. Lifecycle (core worktree pipeline + host)

- [ ] 4.1 `[git] submodules` config key (`auto`/`off`, `config_enum!`),
      documented in `config/config.toml.example`; honored by
      `Config::repo_git` once `add-scm-workflow-customization` lands the
      overlay (soft dependency — key works globally either way).
- [ ] 4.2 Worktree create: post-`worktree add` off-thread
      `submodule update --init --recursive`, non-fatal, progress + failure
      notice; shared by wizard and `wt new` (one pipeline).
- [ ] 4.3 Workspace clone `--recurse-submodules`; remote
      `provision_repo_script` appends the init step.
- [ ] 4.4 Trust gate: init in a repo whose trust class lacks repo-driven
      execution prompts with the submodule URLs (TOFU), per
      `add-config-trust-resolution`'s flow.

## 5. Fold reporting (thegn-host)

- [ ] 5.1 Gitlink conflicts named as `submodule pointer conflict:
<path> (<ours> vs <theirs>)` in drain outcomes and agent prompt vars;
      excluded from driver/rerere routing; table-test the classification.

## 6. Docs + validation (once, at the end)

- [ ] 6.1 Help/config docs: `[git] submodules`, the `[ui]` toggle, and the
      changes-view submodule rendering described on the relevant
      `docs/help/` page (prose ratchet); generated pages regenerate.
- [ ] 6.2 Verify muse snapshots (expected 0–1 re-records:
      `panel_git__branches` only if header text shifts); `just e2e-update`
      only for genuinely changed frames.
- [ ] 6.3 Run `just ci` (includes `openspec validate --all --strict`).
