# Rename workspaces to projects (UI vocabulary)

Linear: THE-10

## Why

THE-10 asks for "workspaces" to become "projects". The word shows up on every
user surface: the sidebar's "WORKSPACES" section heading, palette entries
("New workspace", "Switch workspace"), row menus and modals, the wizard,
~222 quoted strings in `thegn-host` outside tests, 37 sites in
`keymap_specs.rs`, 17 of the 30 `docs/help/` pages (one is literally
`workspaces-and-worktrees.md`), `config/config.toml.example`, and CLI help
prose (`thegn zone` — "workspace groups"). "Project" is the word users bring
from every other IDE; "workspace" additionally collides with tracker
vocabulary the config already carries (Linear/Kaneo `workspace_id`,
`workspace_slug` in `[[tracker]]` accounts — a _different_ concept sharing
the same word inside one config file).

## Decision: UI-label rename; `workspace` stays the machine term

This change renames the **user-facing vocabulary** and accepts `project`
spellings in config. It does **not** rename internals: the DB `workspaces`
table, `thegn_core` types, action _ids_ (`new-workspace` etc. — these are
keys in users' `[keybinds]` tables), control-API/CLI `--json` field names,
the `workspace` openspec capability, or code/ARCHITECTURE prose. Rationale
(full argument in design.md): a full rename multiplies through a v33 schema
migration colliding with in-flight `add-workspace-zones`, breaks every user
keymap and script that names an action id or parses JSON, and rewrites 40+
in-flight change folders — for zero user-visible gain over the label rename,
since users only ever see labels. An alias layer is required for back-compat
in either scheme, so both schemes end with two vocabularies; this one puts
the boundary in the only crisp place: **the UI says project, the machine says
workspace** (the repo already lives with "worktree" internally vs "tab" in
the UI).

## What Changes

- **UI strings.** Sidebar heading "PROJECTS"; labels/hints in `keymap_specs`
  ("New project", "Switch project", …) with search phrases keeping _both_
  words so palette muscle memory survives; menus, modals (remove-project
  chooser), wizard, statusbar text, toasts. Action **ids are untouched**.
- **Config.** `project` spellings become the documented canonical:
  `projects_dir`, `[project.<slug>]`, `confirm_delete_project`,
  `sidebar_project_sort` — serde-level aliases of the existing keys, which
  remain accepted indefinitely (no pre-1.0 deprecation removal). Both
  spellings set = the `project` spelling wins + a `thegn config validate`
  warning. `config.toml.example` flips to the `project` spellings and notes
  the accepted aliases; tracker keys (`workspace_id` etc.) are _not_ renamed
  — they name the tracker's own concept, and the design adds a
  disambiguation rule ("tracker workspace/project" qualified in UI strings
  near them).
- **Docs + help.** The 17 help pages' prose and titles flip
  ("Projects and worktrees"; filenames and page ids stay stable so `zone:*`/
  `panel:*` context mappings and `include_str!` embeds don't churn); README
  and onboarding strings; the generated keybindings/config-reference pages
  follow automatically from labels/schema. Help-prose ratchet: pages must
  mention the new labels — part of the sweep, ratchet files only shrink.
- **CLI.** Help prose flips ("project groups" for `zone`, etc.);
  verb names, flags and JSON field names stay. `thegn config validate`
  gains did-you-mean coverage mapping stray `project*` keys to their
  canonical target.

## Impact

- **tasks.md:** group **C** (Workspaces/repos — vocabulary), group **AO**
  (onboarding/DX); no existing roadmap item — the audit phase wires it.
- **Capabilities:** `workspace` — ADDED requirement (UI vocabulary);
  `config` — ADDED requirement (project spellings accepted, precedence,
  validation). Capability _ids_ unchanged (catalog has no workspace-named
  verb today — verified).
- **Reconciles:** `add-workspace-zones` (zone CLI prose says "project
  groups"; its `workspaces.zone_id` schema work is untouched — internal),
  every in-flight sidebar change (labels only), `move-merge-queue-ambient-
surface` + `add-sidebar-visual-hierarchy` (same rows; the "project name"
  in THE-9 is this change's word — batch the e2e re-record),
  `add-localization` (if a string-catalog lands first, this becomes a
  catalog edit — coordinate, don't block).
- **e2e:** the heading and labels appear in nearly all 45 baselines — full
  re-record with `just e2e-update`.
