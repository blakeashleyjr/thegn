# Tasks

## 1. Core DB (superzej-core)

- [ ] 1.1 `Db::delete_workspace(repo_path) -> Result<u32>` (removes row, returns
      orphan worktree count) — **unit test** `workspace_delete_removes_row_and_reports_orphans`.
- [ ] 1.2 `util::is_url` helper — **unit test**.

## 2. Actions + keymap (host)

- [ ] 2.1 `Action::DeleteWorkspace` + binds (`Alt Shift X`, vim `Space X`) +
      palette item — **unit test** `delete_workspace_keybind_maps_correctly`.

## 3. Prompt mode + flows (host run.rs)

- [ ] 3.1 `prompt`/`prompt_mode` handled before palette/keymap; status-bar
      render of the buffer.
- [ ] 3.2 `create_workspace_from_input` (path validate / URL clone → register →
      switch) — **unit test** `prompt_creation_switches_to_new_workspace`.
- [ ] 3.3 Delete confirm → `delete_workspace` → switch to next or empty home;
      orphan warning — **unit tests** `delete_last_workspace_creates_empty_tab`,
      `prompt_cancellation_returns_to_normal_mode`.
- [ ] 3.4 Sidebar `d`/`x` on a workspace row starts delete confirm; `Enter` still
      switches.

## 4. Validate

- [ ] 4.1 Run `just ci` (includes `openspec-validate`).
