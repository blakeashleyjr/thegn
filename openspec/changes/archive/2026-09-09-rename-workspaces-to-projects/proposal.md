# Rename workspace presentation to projects

Linear: THE-10 (Done)

## Why

The one-repository object users see in the sidebar is a project, while the
pre-existing `thegn project` command represented a different multi-repository
group. The product vocabulary needed to become clear without silently breaking
configuration, keybindings, scripts, schemas, or persisted state.

## What Changed

- User-facing one-repo vocabulary is now **project** across chrome, actions,
  prompts, help, configuration examples, and documentation.
- The existing multi-repo group command is now **program**. Its former
  `project` spelling remains a warned compatibility alias.
- Project spellings are canonical in config and environment. Workspace
  spellings remain accepted with deterministic canonical-wins behavior and a
  named three-stable-release removal window.
- Canonical action/capability ids use project/program terms; deprecated aliases
  keep existing keymaps and capability callers working during the same policy.
- Database tables, serialized machine fields, internal `Workspace*` types,
  provider-owned tracker fields, Cargo workspaces, and container paths retain
  their established vocabulary.

## Impact

- Specs: `config` and `workspace` presentation/compatibility deltas.
- No database migration or reinterpretation of an old command.
- Machine-readable JSON remains stable; deprecation diagnostics stay outside
  machine output.

## Archive status

The accepted implementation and compatibility policy landed in the reviewed
THE-10 series through `87042060`. Earlier draft claims of indefinite aliases,
unchanged action ids, and unchanged Home Manager output were rejected.
