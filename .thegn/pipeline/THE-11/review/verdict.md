# THE-11 security/test/bug review

PASS

Ready for the merge queue.

The review covered the full `git diff main...HEAD`, including arbitrary drawer
process launches, argv/env shaping, sandbox CPU-slice wrapping, trusted versus
repo-provided configuration, event-loop boundaries, cache persistence, and
failure handling. `git merge main` was already up to date.

Review fixes committed:

- Drawer state cache reads now reject symlinked/non-regular store entries and
  store directories.
- Unix cache writes use a same-directory temporary file and atomic rename, and
  reject symlinked parents; regression tests verify outside targets are not
  read or overwritten.
- Platform-specific test helpers and cleanup comply with the host platform and
  ignored-result ratchets.

Validation:

- Architect core selector: 522 passed.
- Architect host selector: 123 passed, including platform ratchets.
- Drawer-state focused suite: 15 passed.
- `thegn-svc` control schema: 1 passed.
- `cargo clippy -p thegn-core -p thegn-host --tests -- -D warnings`: passed.
- `just quick thegn-core` and `just quick thegn-host`: passed with isolated
  temp directories and wrappers disabled.
- All six explicit shell ratchets: clean.
- `treefmt`: 3 files formatted, 0 changed.
- `openspec validate --all --strict`: 170 passed, 0 failed.
- Rustdoc with warnings denied: passed.

## Snapshots

No e2e or snapshot run was performed; this remains the lane's documented
unverified/deferred coverage. The review found no additional frame-affecting
change requiring a snapshot update.

No migration, live state DB, or built binary invocation was performed.
