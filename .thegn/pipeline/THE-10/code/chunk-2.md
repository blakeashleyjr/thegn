# Chunk 2 — host CLI, UI labels, keymap aliases

Commit subject (exact): `feat(the-10): rename workspace presentation and preserve CLI/keymap aliases`

## Scope

Apply the product vocabulary at host edges and preserve command/keymap
compatibility. The old multi-repo project command becomes the deprecated alias
of the canonical program command; one-repo UI surfaces become projects.

## Exact files touched

- `crates/thegn-host/src/main.rs`
- `crates/thegn-host/src/cli_help.rs`
- `crates/thegn-host/src/compat.rs` (new host-edge argv warning helper)
- `crates/thegn-host/src/cmd/config.rs`
- `crates/thegn-host/src/cmd/doctor.rs`
- `crates/thegn-host/src/cmd/env.rs`
- `crates/thegn-host/src/cmd/integrate.rs`
- `crates/thegn-host/src/cmd/list.rs`
- `crates/thegn-host/src/cmd/merge.rs`
- `crates/thegn-host/src/cmd/open.rs`
- `crates/thegn-host/src/cmd/pr_queue.rs`
- `crates/thegn-host/src/cmd/project.rs`
- `crates/thegn-host/src/cmd/search.rs`
- `crates/thegn-host/src/cmd/session.rs`
- `crates/thegn-host/src/cmd/wt.rs`
- `crates/thegn-host/src/cmd/zone.rs`
- `crates/thegn-host/src/actions.rs`
- `crates/thegn-host/src/chrome.rs`
- `crates/thegn-host/src/detail.rs`
- `crates/thegn-host/src/handlers/sidebar_actions.rs`
- `crates/thegn-host/src/handlers/sidebar_activate.rs`
- `crates/thegn-host/src/handlers/sidebar_collapse.rs`
- `crates/thegn-host/src/handlers/sidebar_folder.rs`
- `crates/thegn-host/src/handlers/sidebar_keys.rs`
- `crates/thegn-host/src/handlers/sidebar_mouse.rs`
- `crates/thegn-host/src/handlers/sidebar_persist.rs`
- `crates/thegn-host/src/handlers/sidebar_reorder.rs`
- `crates/thegn-host/src/handlers/switch.rs`
- `crates/thegn-host/src/handlers/workspace_remove.rs`
- `crates/thegn-host/src/help/pages.rs`
- `crates/thegn-host/src/hydrate.rs`
- `crates/thegn-host/src/keyhint.rs`
- `crates/thegn-host/src/keymap.rs`
- `crates/thegn-host/src/keymap_merge.rs`
- `crates/thegn-host/src/keymap_specs.rs`
- `crates/thegn-host/src/menu.rs`
- `crates/thegn-host/src/onboarding.rs`
- `crates/thegn-host/src/palette.rs`
- `crates/thegn-host/src/panel/sections/across.rs`
- `crates/thegn-host/src/panel/sections/merge_queue.rs`
- `crates/thegn-host/src/run.rs`
- `crates/thegn-host/src/session.rs`
- `crates/thegn-host/src/sidebar.rs`
- `crates/thegn-host/src/sidebar_keytable.rs`
- `crates/thegn-host/src/sidebar_order.rs`
- `crates/thegn-host/src/sidebar_pipeline.rs`
- `crates/thegn-host/src/sidebar_view.rs`
- `crates/thegn-host/src/statusbar_badges.rs`
- `crates/thegn-host/src/wizard.rs`
- `crates/thegn-host/src/workspace_create.rs`
- `crates/thegn-host/src/workspace_picker.rs`
- `crates/thegn-host/src/workspace_pool.rs`
- `test/smoke.sh`

## Approach

1. Add exact raw-argv detection for `project` and `--project` only where they
   mean the existing multi-repo operation. Emit a non-JSON deprecation warning
   naming `program`/`--program`; preserve exit codes, JSON fields, and behavior.
   Use clap visible aliases for discoverability, but do not rely on clap alone
   for warning behavior.
2. Update command help/group text to distinguish “program (multi-repo group)”
   from the one-repo project UI. Keep `cmd/project.rs` as an implementation
   module in this mechanical sweep; renaming source modules would add churn
   without compatibility value.
3. Change human-facing sidebar, picker, menu, modal, status, toast, onboarding,
   panel, and palette labels to project. Preserve internal `Workspace*` types,
   DB calls, scope names, and provider concepts. In tracker-adjacent prose use
   qualified terms such as “tracker workspace” or “Linear project”.
4. Make the canonical keymap ids the six `*-project` ids from the design.
   `Action::from_key` accepts all six old `*-workspace` ids; existing chords
   remain unchanged. Keep one ActionSpec per action, with compatibility search
   keywords, and add alias round-trip tests. Do not add duplicate palette rows.
5. Keep page ids/embedded paths stable. Update host help registration only as
   needed for canonical action claims; prose and ratchet files belong to Chunk 3. Update smoke commands to use `program` canonically and assert old
   project/flag aliases still work in an isolated state directory.

## Overlap and dependency

No file overlaps Chunk 1 or Chunk 3. This chunk depends on Chunk 1’s catalog
and config names and must run after it. Chunk 3 depends on these final labels,
action ids, and CLI help outputs and must run after this chunk. The host code is
one serial landing so no coder is asked to land a partial palette/keymap/UI
rename.

## Tests to run

- `just quick thegn-host`
- `cargo nextest run -p thegn-host keymap --no-fail-fast`
- `cargo nextest run -p thegn-host cli_help --no-fail-fast`
- `cargo nextest run -p thegn-host palette --no-fail-fast`
- `cargo nextest run -p thegn-host workspace --no-fail-fast`
- Run the relevant `test/smoke.sh` command subset with a temporary
  `XDG_STATE_HOME`; do not use the live DB.

Do not run e2e, `just ci`, a full-workspace build, a migration, or the built
binary without an isolated temporary state directory.

## Done criteria

- `thegn program` and `wt new --program` are canonical; old `thegn project` and
  `wt new --project` execute the same old multi-repo operation and warn with
  their replacements, without changing JSON output.
- One-repo UI labels say project, while tracker, Cargo, container, DB, and
  internal machine terms remain correctly qualified or unchanged.
- Canonical project action ids appear once in the registry; all old workspace
  ids still parse and existing chords dispatch. Keymap/help drift tests pass.
- Scoped tests and isolated smoke checks pass, and the coder commits exactly:
  `feat(the-10): rename workspace presentation and preserve CLI/keymap aliases`.
