# Tasks — workspace search & replace

## 1. Pure model (thegn-core)

- [ ] 1.1 New `search_replace.rs`: `SearchSpec`, `Match` (path/line/span/
      before/content-hash), `render_replacement` (literal + regex capture
      expansion), `ReplacePlan`, skip-if-changed decision, `ApplyReport`.
- [ ] 1.2 Invalid-regex classification (inline error, never a worker spawn).
- [ ] 1.3 ast-grep JSON → `Match` parser (defensive, bounded).
- [ ] 1.4 Unit tests to the 95% gate: capture expansion, bottom-up edit
      ordering, hash-drift skip, malformed JSON, glob include/exclude logic.

## 2. Config

- [ ] 2.1 `[search]` table: `respect_gitignore` (default true),
      `include_hidden` (default false), `max_results`, `structural`
      config_enum (`ast-grep` default, `none`; others reserved).
- [ ] 2.2 Document every key in `config/config.toml.example`; extend the
      config_enum round-trip + reserved-kind validation tests.

## 3. Structural seam

- [ ] 3.1 `StructuralSearch` sync trait + `SeamError` type in core; kinds via
      `config_enum!`.
- [ ] 3.2 ast-grep impl (host/svc): argv-only invocation, `--json`, no
      write flags ever; memory-cap wrap via the file-tools containment.
- [ ] 3.3 Probe (binary + version, offline) registered in `thegn doctor`;
      conformance probe-shape coverage; smoke assertion.

## 4. Search worker (thegn-host)

- [ ] 4.1 Off-thread worker on the fff grep tiers (plain/regex) + `ignore`
      walker for hidden/ignored toggles; bounded batches → channel →
      `TerminalWaker` pulse; generation-token cancellation checked between
      batches; `.git` and out-of-root symlinks excluded.
- [ ] 4.2 Drain handler discards stale generations and marks chrome dirty;
      render-plan tests stay green (idle wake ⇒ `Skip`).

## 5. Overlay surface

- [ ] 5.1 Search & Replace overlay on the layer path: fields, options,
      grouped result tree, per-match/per-file toggles, before/after preview,
      truncation indicator; session-held state.
- [ ] 5.2 Actions (`search-replace-open`, surface bindings) as `ActionSpec`s;
      palette handoff from Content mode seeds the query.
- [ ] 5.3 Editor handoff: open file at match line through the editor seam.
- [ ] 5.4 Help: `docs/help/search-replace.md` claims the new action ids and
      the `zone:search-replace` context; help/prose/context ratchets green.

## 6. Apply path

- [ ] 6.1 Off-thread apply worker: re-read, hash-verify, bottom-up edits,
      temp-then-rename preserving permissions; per-file error isolation;
      `ApplyReport` to the overlay + `model.status`.
- [ ] 6.2 Read-only worktree / permission-denied scenarios covered by tests
      (report, never abort the batch).

## 7. Catalog + CLI

- [ ] 7.1 `CATALOG` rows `search.query` (read) / `search.replace` (write);
      `every_verb_has_exactly_one_row` and scope tests updated.
- [ ] 7.2 `thegn search` CLI verb: query mode (JSON output), `--replace`
      plan mode, `--apply` gated by the write scope; smoke coverage.
- [ ] 7.3 Note the MCP write-surface gap pending the in-flight scope-gating
      work (do not re-implement it).

## 8. Preview routes

- [ ] 8.1 `.docx` extracted-text route (off-loop parse, text route render).
- [ ] 8.2 Archive listing route (`.zip`/`.tar*`, bounded entries).
- [ ] 8.3 Unknown-binary → hex-view fallback.

## 9. Validation

- [ ] 9.1 e2e spec for the overlay (record baselines with `just e2e-update`;
      pin any volatile chrome in `e2e_freeze`).
- [ ] 9.2 Sync deltas via `/opsx:sync`; run `just ci` once, pre-PR.
