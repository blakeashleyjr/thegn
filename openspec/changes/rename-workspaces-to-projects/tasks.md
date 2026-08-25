# Tasks — rename workspaces to projects

## 1. Config aliases (thegn-core)

- [ ] 1.1 Serde aliases: `projects_dir`, `[project.<slug>]`,
      `confirm_delete_project`, `sidebar_project_sort` (schema field names
      unchanged); precedence rule: `project` spelling wins when both are set.
- [ ] 1.2 `thegn config validate`: duplicate-spelling warning naming both
      locations; did-you-mean entries for `project*` near-misses.
- [ ] 1.3 Unit tests: alias parse, both-set precedence + warning, tracker
      keys (`workspace_id` etc.) unaffected. (Pure config logic — core 95%
      gate.)
- [ ] 1.4 Verify the derived home-manager module output is unchanged
      (schema-side names stay `workspace*`).

## 2. UI string sweep (thegn-host)

- [ ] 2.1 `keymap_specs.rs`: labels/hints → "project"; search phrases keep
      both words; action ids untouched (guardrail test: every previous id
      still present).
- [ ] 2.2 Sidebar heading "PROJECTS"; menus, modals (remove-project chooser
      keeps its safe-arm semantics verbatim), wizard, statusbar/toast text,
      palette items.
- [ ] 2.3 Tracker-adjacent strings: qualify foreign concepts ("Linear
      project", "tracker workspace") per the design's disambiguation rule.
- [ ] 2.4 CLI help prose (`zone`, `repos`, wizard prompts, `--help` text);
      JSON field names and verb/flag names untouched (guardrail: `--json`
      snapshots unchanged).

## 3. Docs + help corpus

- [ ] 3.1 Sweep the 17 affected `docs/help/` pages: titles + prose
      ("Projects and worktrees" — filenames and page ids stay). Keep the
      three help ratchet files shrink-only; regenerate with
      `just help-ratchet-update` only after a real burn-down.
- [ ] 3.2 `config/config.toml.example`: flip to `project` spellings, document
      the accepted `workspace` aliases once, keep tracker keys as-is.
- [ ] 3.3 README + onboarding strings; CHANGELOG entry (vocabulary change,
      config aliases, nothing machine-breaking).
- [ ] 3.4 Confirm the generated keybindings/config-reference help pages pick
      up the new labels (their tests assert every bindable action appears).

## 4. Validation

- [ ] 4.1 Full e2e re-record (`just e2e-update` — the heading/labels appear
      in nearly all 45 baselines; review the diff; batch with the THE-9/64
      sidebar changes if landing in the same window).
- [ ] 4.2 Run `just ci` once (help ratchets, config round-trips, openspec
      validate, coverage).
