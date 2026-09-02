# THE-10 reconcile — merge current main into the lane

## Files to touch (exact paths)

- `config/config.toml.example`
- `crates/thegn-host/src/keymap_specs.rs`
- `crates/thegn-host/src/run.rs`
- `crates/thegn-host/src/workspace_create.rs`
- `docs/help/cli.md`

## State you are landing in

`git merge main` is ALREADY IN PROGRESS in this worktree and left the five
files above conflicted. Do not abort or restart the merge. Resolve the
conflicts, then `git commit` the merge.

## Approach

Main has advanced by several landed lanes. Every conflict in this drain so far
has been "both sides added different things in the same place", not a genuine
disagreement. Default to **keeping both sides** unless one side deliberately
DELETED something the other kept — verify that with `git log` before dropping
any code.

Named reconcile items:

- `config/config.toml.example` and `docs/help/cli.md` are documentation
  corpora with ratchets behind them. Keep both sides' entries; do not
  hand-write anything that is generated.
- `keymap_specs.rs` — keep both sides' action/keybind entries. The help
  ratchet requires every `ACTION_SPECS` id to be claimed by a `docs/help/`
  page, so a dropped entry fails a test rather than failing quietly.
- `run.rs` is the event loop; preserve the lane's renames AND main's
  additions. Do not reorder unrelated statements.
- This lane renames project/workspace vocabulary. Where main added NEW code
  using the OLD vocabulary, carry main's code forward and apply the rename to
  it — do not revert the rename, and do not leave the two spellings mixed.

## Verification required before you report

- `RUSTC_WRAPPER= just quick thegn-core` and `RUSTC_WRAPPER= just quick thegn-host`
- `RUSTC_WRAPPER= cargo nextest run -p thegn-host complete help catalog_tests`
- `RUSTC_WRAPPER= cargo nextest run -p thegn-core config_example`
- `git diff --check`

If sccache fails with a read-only cache, re-run with `RUSTC_WRAPPER=` as above;
that is a known sandbox limitation (THE-90), not your bug.
