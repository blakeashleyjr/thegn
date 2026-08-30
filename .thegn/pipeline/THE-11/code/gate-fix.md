# THE-11 — fold-gate fix (Lead work order)

files:
  - config/config.toml.example
  - crates/thegn-core/src/config.rs
  - test/env-overlay-ratchet.txt

## Why this exists

`thegn land` refused this branch: **the fold gate (`just test`) is red.** The
review PASS was based on scoped suites, which do not include the two
config-coverage ratchets. Reproduced on the folded tree
(`tg/the-11-drawer-tools` merged with `main`):

1. `crates/thegn-core/tests/env_overlay_coverage.rs` —
   `config keys with neither a THEGN_* knob in env_overlay nor a
   test/env-overlay-ratchet.txt entry: ["agents.drawer_cwd",
   "agents.drawer_scope"]`
2. `crates/thegn-core/tests/config_example.rs` —
   `config/config.toml.example is missing documentation for 2 key(s):
   agents.drawer_cwd` (and `agents.drawer_scope`)

Both are new `[[agents]]` keys this branch introduced.

## Done criteria

- `cargo nextest run -p thegn-core -E 'test(/every_shallow_key_has_an_env_knob|example_config_documents_every_section/)'`
  passes.
- **Prefer the env knob over a ratchet pin.** The ratchet file is shrink-only;
  adding a line there is debt and needs a written reason. Add the
  `THEGN_AGENTS_DRAWER_CWD` / `THEGN_AGENTS_DRAWER_SCOPE` overlay entries
  alongside the existing `[[agents]]` knobs unless there is a concrete reason
  the key cannot be set from the environment — if so, state it in the pin.
- Document both keys in `config/config.toml.example` the way its neighbours
  are documented: a one-line explanation of what the key does plus a commented
  `# key = default`. Say what the value means, not that it exists.
- Then run the full gate yourself: `THEGN_ALLOW_HEAVY=1 just test`. This row
  exists because scoped tests missed a workspace-level failure; do not report
  PASS on scoped runs alone.
- No behaviour changes, no refactors.
