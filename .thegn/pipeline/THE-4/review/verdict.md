# THE-4 security / test / bug review

PASS

Ready for the merge queue. The full `git diff main...HEAD` is documentation,
OpenSpec records, the justfile lint wiring, and one static docs guard; it adds
no runtime subprocess, DB, event-loop, permission, or API behavior.

## Finding fixed during review

The hand-run Muse setup in both `docs/testing-with-muse.md` and
`extensions/skills/tui-check/SKILL.md` claimed to match the hermetic e2e
environment but did not isolate global/system Git config or the session D-Bus.
That could read developer config or observe the live media session. Both
recipes now create a throwaway Git config and set `GIT_CONFIG_SYSTEM=/dev/null`
and `DBUS_SESSION_BUS_ADDRESS` to the null endpoint. The focused
`test/muse-docs-guard.sh` regression test is wired into `just lint`.

Review fix commit: `e8367838` — `fix(the-4): isolate hand-run muse sessions (review)`.

## Validation

- `just quick thegn-host` — passed.
- `cargo clippy -p thegn-host --tests -- -D warnings` — passed.
- `cargo nextest run -p thegn-host help mq_assets` — passed, 83/83; 2,589 skipped.
- `openspec validate --all --strict` — passed, 170/170.
- `treefmt --ci` — passed, 2,201 files formatted, 0 changed.
- `test/muse-docs-guard.sh` — passed.
- Review commit hooks — ShellCheck and treefmt passed; YAML checks had no files
  to check.
- `git diff --check` — passed.

## Unverified / deferred

- Full `just test`, `just ci`, coverage, and e2e remain intentionally deferred
  under the documented dev-loop policy; no frame-affecting files changed.
- The architect review records one pre-existing core sandbox test failure,
  `sandbox::tests::oci_local_secrets_go_to_env_file_not_argv`; this branch does
  not touch that code or test.
- `.understand-anything/knowledge-graph.json` is absent, so no graph overlay
  was produced; the full diff was audited manually instead.

## Snapshots

None — no frame-affecting changes.
