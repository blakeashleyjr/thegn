# Tasks — workspace search & replace

## 1. Pure model (thegn-core)

- [x] 1.1 New `search_replace.rs`: `SearchSpec`, `Match` (path/line/span/
      before/content-hash), `render_replacement` (literal + regex capture
      expansion), `ReplacePlan`, skip-if-changed decision, `ApplyReport`.
- [x] 1.2 Invalid-regex classification (inline error, never a worker spawn).
- [x] 1.3 ast-grep JSON → `Match` parser (defensive, bounded).
- [x] 1.4 Unit tests to the 95% gate: capture expansion, bottom-up edit
      ordering, hash-drift skip, malformed JSON, glob include/exclude logic.

## 2. Config

- [x] 2.1 `[search]` table: `respect_gitignore` (default true),
      `include_hidden` (default false), `max_results`, `structural`
      config_enum (`ast-grep` default, `none`; others reserved).
- [x] 2.2 Document every key in `config/config.toml.example`; extend the
      config_enum round-trip + reserved-kind validation tests.

## 3. Structural seam

- [x] 3.1 `StructuralSearch` sync trait + `SeamError` type in core; kinds via
      `config_enum!`.
- [x] 3.2 ast-grep impl (host `structural.rs`): argv-only invocation, `--json`,
      no write flags ever; memory-cap wrap via `wrap_background_argv`.
- [x] 3.3 Probe (binary + version, offline) registered in `thegn doctor`
      (`thegn-svc` seam registry `structural_probes`).

## 4. Search worker (thegn-host)

- [x] 4.1 Off-thread worker (`search_worker.rs`) on the `ignore` walker for
      hidden/ignored/glob toggles + the pure core matcher; bounded batches →
      channel → `TerminalWaker` pulse; generation-token cancellation checked
      between files; `.git` and out-of-root symlinks excluded.
- [x] 4.2 Overlay drain discards stale generations and marks chrome dirty;
      render-plan invariants untouched (overlay uses `open_layer` → the layer
      damage path; idle wake ⇒ `Skip`).

## 5. Overlay surface

- [x] 5.1 Search & Replace overlay (`search_overlay.rs`) on the layer path:
      fields, options, grouped result tree, per-match/per-file toggles,
      before/after preview, truncation indicator; session-held state.
- [x] 5.2 Action (`search-replace-open`, `Ctrl+Shift+H`) as an `ActionSpec`;
      palette handoff from Content mode seeds the query.
- [x] 5.3 Editor handoff: open file at match line (`Ctrl+o`).
- [x] 5.4 Help: `docs/help/search-replace.md` claims `search-replace-open`;
      registered in `help/pages.rs`.

## 6. Apply path

- [x] 6.1 Off-thread apply worker (`search_apply.rs`): re-read, hash-verify,
      bottom-up edits, temp-then-rename preserving permissions; per-file error
      isolation; `ApplyReport` to the overlay + `model.status`.
- [x] 6.2 Read-only worktree / permission-denied + path-escape scenarios
      covered by tests (report, never abort the batch).

## 7. Catalog + CLI

- [x] 7.1 `CATALOG` rows `search.query` (read) / `search.replace` (write) +
      `Verb::SearchQuery`/`SearchReplace`; `every_verb_has_exactly_one_row`,
      the scope table, and the CLI surface gaps updated.
- [x] 7.2 `thegn search` CLI verb: query mode (JSON), `--replace` plan mode,
      `--apply` through the guarded write path; `--structural` via the seam.
- [x] 7.3 MCP write-surface gap recorded (SURFACE_GAPS / proposal); not
      re-implemented here.

## 8. Preview routes

- [ ] 8.1 `.docx` extracted-text route (off-loop parse, text route render).
      DEFERRED — needs new zip/xml deps; the viewer seam it extends
      (`add-viewers-and-quick-open`) is still in flight.
- [ ] 8.2 Archive listing route (`.zip`/`.tar*`, bounded entries). DEFERRED (as 8.1).
- [ ] 8.3 Unknown-binary → hex-view fallback. DEFERRED (as 8.1).

## 9. Validation

- [ ] 9.1 e2e spec for the overlay (record baselines). DEFERRED — the e2e
      baselines are stale repo-wide (see CLAUDE.md); not re-recorded here.
- [x] 9.2 Scoped tests green (`cargo nextest run -p thegn-core` /
      `-p thegn-host`). `/opsx:sync` + full `just ci` remain a pre-PR step.
