# Tasks — terminal presets & launch menu

## 1. Config (thegn-core)

- [x] 1.1 `Preset` struct + `mode` `config_enum!` in a sibling config module
      (`[[presets]]`), plus `WorktreeTemplate.preset: Option<String>`.
- [x] 1.2 Validation: empty commands+no layout rejected; duplicate names,
      unknown `layout` ref (fallback to commands), unknown template `preset`
      ref warn; template `preset` exclusive with `layout`/`commands`.
- [x] 1.3 Pure resolution fold: command string → agent entry | tool entry |
      shell, per-pane cwd/env computation. Unit tests for 1.1–1.3 (95% gate).
- [x] 1.4 Document every key in `config/config.toml.example` (with a `dev`
      example and the secrets-via-env-indirection note).

## 2. Launch menu (thegn-host)

- [x] 2.1 `launch-menu` `ACTION_SPECS` entry (default chord `Ctrl Alt l` if
      the keymap uniqueness tests allow, else palette-only) + handler in
      `src/handlers/` (not run.rs).
- [x] 2.2 Dedicated picker palette: presets (name + description) then
      `agent::choices()`; pending-selection gate for the rows.
- [x] 2.3 Agent selections update `worktrees.agent` (the wizard's write);
      tools/presets don't. Unit-test the routing.

## 3. Preset application (thegn-host)

- [x] 3.1 Off-loop application pipeline: resolve launch specs on a blocking
      task, deliver over a channel + waker, spawn at the drain (`split` = even
      split in one new tab, the `WorktreeTemplate::commands` path; `tabs` =
      one tab per command; `layout` ref applies the saved layout).
- [x] 3.2 Wire template `preset` into worktree creation (after-create apply;
      template `agent` still wins the remembered agent).
- [x] 3.3 Keep the render-plan tests green (new tabs/splits ⇒ `Full`).

## 4. CLI + catalog

- [x] 4.1 `Verb::LaunchPreset` + `required_scope` mapping (exec-level, not
      `open`'s) + `CATALOG` row (Cli implemented; other surfaces per the
      coverage tests — reconcile with `complete-control-surface-coverage`).
- [x] 4.2 `thegn open --preset`: name validation (miss ⇒ candidates + exit 3),
      launch-preset intent enqueue after the focus intent; compositor consume
      (claim-and-delete, tolerate a DB missing the table) + headless
      apply-after-first-frame.
- [x] 4.3 Smoke-test the CLI path (`test/smoke.sh` addition).

## 5. Help & docs

- [x] 5.1 Help page update claiming `launch-menu` (extend
      `docs/help/terminal-and-panes.md` or add a presets page; help +
      help-prose ratchets), covering `[[presets]]`, the menu, and
      `open --preset`.

## 6. Validation

- [ ] 6.1 Re-record affected e2e baselines (picker frames) with
      `just e2e-update`; review the diff.
- [ ] 6.2 Run `just ci` once, pre-PR (includes openspec validate).
