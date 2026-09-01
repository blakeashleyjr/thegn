REVISE

Revision chunks:

- `.thegn/pipeline/THE-11/architect-review/revision-1.md`

The branch was first merged with `main` as required (`3181ae5d`); the config
re-export conflict was resolved by retaining both drawer and notification
exports. I reviewed the full `git diff main...HEAD` and fixed two small issues:

- `62b07ecd` — corrected the invalid private/nonexistent pane-channel constant
  in the drawer test.
- `3ccbc35c` — enabled the requested drawer indicator in the default
  `bottom_left` bars order and updated its example/coverage assertion.

The implementation is not ready to land because global occupants are not
reachable from picker/cycle/selection (all host paths hard-code Worktree), and
the old drawer pool/channel/switch lifecycle remains active beside the new
`DrawerRuntime`. These are semantic integration gaps with concrete fixes in
revision 1. The in-flight OpenSpec change also remains contradictory to the
accepted implementation design and needs synchronization.

Verification:

- Host mandatory focused gate: PASS, 105 tests.
- Service control schema: PASS.
- `just quick`: PASS.
- `cargo clippy -p thegn-core -p thegn-host --tests -- -D warnings`: PASS.
- Focused core drawer/config tests: PASS, 15 tests.
- Focused host drawer/palette/keymap/statusbar tests: PASS, 162 tests.
- `cargo fmt --all -- --check`: PASS.
- Nix-shell `treefmt`: PASS, 0 files changed.
- `openspec validate --all --strict`: PASS, 170 items; semantic sync is still
  required as described above.
- Rustdoc with `RUSTDOCFLAGS="-D warnings"`: PASS for thegn-core and
  thegn-host.
- `test/ratchet-check.sh`: not present in this checkout.
- Core mandatory focused gate: FAIL in the pre-existing, untouched
  `sandbox::tests::oci_local_secrets_go_to_env_file_not_argv` test; the failure
  reports `GH_TOKEN=ghp_secret` on the OCI argv. No THE-11 change touches that
  code, so it was not altered.

No e2e run, snapshot re-recording, built binary invocation, migration, or live
state-DB access was performed. The eight affected chrome snapshots remain
intentionally listed in the architect design for the follow-up requested by
the issue.
