# Tasks

## 1. Viewed-state cache (thegn-core / state-db)

- [ ] 1.1 Bump `user_version`: add `pr_file_views (worktree, pr_number, file_path,
viewed_at)` + accessors `put_pr_file_viewed` / `get_pr_files_viewed` /
      `clear_pr_file_viewed` — **unit tests**: put+get round-trip, per-worktree/PR
      scoping, additive migration (absent table ⇒ empty).

## 2. GitHub sync (thegn-svc)

- [ ] 2.1 Extend `GhBackend`: read the current user's viewed files for a PR (via
      the existing GraphQL PR query where possible) and mark a file viewed/unviewed;
      reconcile GitHub's set into the local cache on refresh (GitHub wins on
      conflict). Sync failure degrades to local-only (no panic).

## 3. Panel: viewed glyph + stacked walker (thegn-host)

- [ ] 3.1 Render a viewed glyph/dim per file in the PR file list
      (`caps::active_glyphs()` + ASCII fallback); a "mark viewed" action writes the
      cache immediately and triggers off-loop GitHub sync — **render test**: marking
      viewed is a chrome repaint.
- [ ] 3.2 Add `pr_commit_idx` to `PanelUi` and a stacked/squashed toggle: in
      stacked mode render `git diff <commit~1>..<commit>` over `PanelData.commits`
      via the existing diff path; keys step the cursor.

## 4. Docs + validate

- [ ] 4.1 Document viewed-state sync + the stacked-review toggle/keys in the
      panel/review doc section + `config/config.toml.example`.
- [ ] 4.2 Run `just ci` (fmt-check + lint + build + test + coverage ≥95% core +
      smoke + nix-build + `openspec validate --all --strict`).
