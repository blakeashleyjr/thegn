REVISE

## Review basis

`main` was merged first in `b1ff0e46` (`Merge branch 'main' into
tg/the-29-fork-sessions`). I reviewed the full `git diff main...HEAD`, all
THE-29 lane documents and their Unverified sections, `CLAUDE.md`,
`docs/ARCHITECTURE.md`, and the active OpenSpec change.

The mechanical platform-ratchet correction was committed in
`f3e27993` (`fix(the-29): pin unix-only fork integration ratchet`).

## Revision required

See:

- `.thegn/pipeline/THE-29/architect-review/revision-2.md`

The blocking findings are:

- `crates/thegn-host/src/daemon/service.rs:431-463` records a configured-agent
  recipe from startup configuration while the actual open resolves against
  fresh per-request configuration. A provider change can therefore make the
  retained fork source disagree with the process that was launched.
- The required successful matching configured-agent/native-harness path is not
  exercised. `agent_open.rs` only validates the mismatch and matching predicate;
  the daemon integration covers a raw recipe, not native harness resolution.

## Gates

- Core mandatory filter: the requested run failed only at the existing,
  unrelated `sandbox::tests::oci_local_secrets_go_to_env_file_not_argv` test,
  which reports a secret on OCI argv outside the THE-29 diff. The initial run
  was also blocked by the sandboxed sccache wrapper; the equivalent requested
  filter passed through with `RUSTC_WRAPPER=` until that pre-existing failure.
- Host mandatory filter: 105/105 passed after the mechanical platform-ratchet
  pin.
- Service `control_schema`: 1/1 passed.
- `just quick`: passed with `RUSTC_WRAPPER=` and `XDG_RUNTIME_DIR=/tmp`.
- Touched-crate clippy with `--tests -- -D warnings`: passed for
  `thegn-core`, `thegn-host`, and `thegn-svc`.
- Rustdoc with `RUSTDOCFLAGS="-D warnings"`: passed for all three touched
  crates.
- Fork-focused tests: helper tests 3/3 and the actual control-path integration
  test 1/1 passed.
- `treefmt --no-cache --allow-missing-formatter`: passed with no formatting
  drift. The plain invocation could not use its read-only global cache.
- `test/ratchet-check.sh`: not present.

## Unverified / unavailable

- `openspec validate --all --strict` could not run because `openspec` is not on
  PATH. The active proposal, design, spec, and tasks were read and are
  synchronized with the implementation, but validation remains unverified.
- No live `thegn` state DB or migration was used; fork tests used temporary
  `XDG_STATE_HOME` state. The PATH `thegn` binary does not support
  `dispatch report`, so no dispatch report was filed.
- The lane's separate real-socket service test and CLI smoke were not rerun;
  the in-process `ControlApi` fork integration was run successfully instead,
  with temporary state.
- The requested `understand-anything:understand-diff` skill resource was not
  available in this environment; the diff was reviewed manually.
- No `just test`, `just ci`, coverage, or e2e command was started.
