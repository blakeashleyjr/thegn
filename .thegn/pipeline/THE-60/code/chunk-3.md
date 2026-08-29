# Chunk 3 — explicit install action, help, and ratchet closure

## Scope

Expose the explicit install operation through the existing action registry and
document it. This chunk is serial after chunk 2 and owns all action/help
ratchet work. It must not add an external control/API capability.

## Files touched

- `crates/thegn-host/src/keymap.rs`
- `crates/thegn-host/src/keymap_specs.rs`
- `crates/thegn-host/src/run.rs`
- `docs/help/sandboxing.md`
- `test/help-ratchet.txt`
- `test/help-prose-ratchet.txt`
- `test/help-panel-prose-ratchet.txt`

The completion and control snapshot files are verification inputs, not expected
diffs: `test/completion-slot-ratchet.txt` and `docs/api/control-v1.json` must
remain unchanged. Do not modify `test/help-context-ratchet.txt` because this
does not add a panel context or section.

## Approach

1. Add `Action::InstallToolchain` with stable key `toolchain-install`, a
   palette spec with keywords `toolchain`, `mise`, `install`, `missing`, and no
   default chord. Keep it provider-generic; the label must not hard-code a
   vendor in the action id.
2. Dispatch it through the existing run-loop action site. Resolve the active
   worktree and selected `[env.<name>]`, then call chunk 2's provider operation
   on a background task. Show a confirmation/status for missing trust or no
   binary; on success/failure pulse the existing refresh path. Never execute a
   shell string on the input loop and never retry implicitly.
3. Update `sandboxing.md` frontmatter and prose to explain detection, shims vs
   env, trust, precedence, missing-tool token, and the explicit install action.
   Keep generated config-reference/keybindings pages generated; do not hand-edit
   them.
4. Run the help ratchet updater/validator so the new action is claimed and
   actually described. Run completion-slot coverage and the control-schema
   snapshot test; a diff in either is a failure of this design because the
   action takes no value and adds no wire route.

## Dependency/overlap

Serial after chunk 2 because the dispatch calls its provider operation. Files
are disjoint from chunks 1 and 2. This is the final code chunk before the
architectural integration review.

## Tests to run

- `just quick thegn-host`
- `cargo nextest run -p thegn-host keymap`
- `cargo nextest run -p thegn-host palette`
- `cargo nextest run -p thegn-host help`
- `cargo nextest run -p thegn-host completion`
- `cargo nextest run -p thegn-svc --test control_schema control_wire_matches`

Use a temporary `XDG_STATE_HOME` for any binary/manual check. Do not run e2e,
`just test`, `just ci`, or a full-workspace build.

## Done criteria

- `toolchain-install` round-trips through `Action`, `ACTION_SPECS`, palette,
  keymap dispatch, and its help page.
- Install is explicit, trust-aware, off-loop, bounded, and provider-generic.
- Help ratchets pass with no new debt; completion-slot ratchet and
  `docs/api/control-v1.json` are unchanged and verified.
- Commit exactly as: `feat(ui): expose explicit toolchain install action`
