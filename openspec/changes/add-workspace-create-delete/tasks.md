# Tasks

Most of the create/delete surface already shipped; these tasks reconcile the
spec and close the two remaining gaps (discovery picker + orphan reporting).

## 1. Orphan-count reporting (host)

- [x] 1.1 `remove_workspace` reports the surviving-worktree count on the
      keep-files path — **unit test** `remove_workspace_reports_orphan_count`.
- [x] 1.2 Deleting the last workspace empties the session — **unit test**
      `delete_last_workspace_empties_session`.

## 2. Auto-discover in the create flow (host)

- [x] 2.1 `menu::new_workspace_menu` (discovery `MenuOverlay`) +
      `MenuChoice::CreateWorkspaceFromPath`/`NewWorkspacePrompt` +
      `MenuKindTag::NewWorkspace`.
- [x] 2.2 `Action::NewWorkspace` is discovery-first (`repo::discover_repos`),
      degrading to the typed-input overlay; picked paths and the prompt both
      funnel through `create_workspace_from_input_with_config` — **unit test**
      `new_workspace_menu_offers_discovered_repos`.

## 3. Delete-workspace reachability

- [x] 3.1 Add a `delete-workspace` `ActionSpec` (palette-driven, no default chord)
      and drop the dead `Alt Shift X` / `Space X` binds shadowed by close-worktree;
      `Alt W` keeps `NewWorkspace` — **unit test**
      `delete_workspace_keybind_maps_correctly`.

## 4. Validate

- [ ] 4.1 Run `just ci` (includes `openspec-validate`).
