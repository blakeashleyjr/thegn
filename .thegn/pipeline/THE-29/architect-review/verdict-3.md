APPROVED

## Review basis

`git merge main` was run first and reported `Already up to date`; the branch
already contains the prior main merge (`b1ff0e46`). I reviewed the full
`git diff main...HEAD`, the THE-29 architecture design, every lane document and
its Unverified section, `CLAUDE.md`, `docs/ARCHITECTURE.md`, and the active
OpenSpec change.

The implementation meets the design: the core fork policy is substrate-free;
native fork syntax stays behind the harness seam; recipes remain daemon-memory
only; the v62 cache stores credential-free lineage; `sessions.fork` projects
through the single catalog over HTTP, gRPC, CLI, MCP, and plugin routing; the
daemon owns fresh PTY spawn, cap reapplication, geometry, identity, handoff,
and cleanup; and UI placement uses the existing async adopt-intent path.

## Correction committed

- `6a74dcc1` — `fix(the-29): pin deliberate fork result ignores`
  - Pins the two new, intentional best-effort-result files with reasons in
    `test/ignored-result-ratchet.txt`.

No revision chunk is required.

## Verification

- Core mandatory filter: 328 passed; one unrelated pre-existing failure in
  `sandbox::tests::oci_local_secrets_go_to_env_file_not_argv` reports
  `GH_TOKEN=ghp_secret` on OCI argv outside THE-29.
- Host mandatory filter: 105/105 passed.
- Service `control_schema`: 1/1 passed.
- `just quick`: passed.
- Touched-crate clippy with tests and `-D warnings`: `thegn-core`,
  `thegn-host`, and `thegn-svc` passed.
- Rustdoc with private items and warnings denied: all three touched crates
  passed.
- `treefmt --no-cache --allow-missing-formatter`: 0 files changed.
- Ignored-result ratchet: clean (320 pinned).
- Fork/native-provider/adoption/lifecycle targeted tests: 27/27 passed.
- CLI smoke reached and passed the new `session fork --help` check, then hit
  the environment's `Operation not permitted` socket-bind restriction in an
  existing daemon-bind check.
- `test/ratchet-check.sh` is not present.

## Unverified / unavailable

- `openspec validate --all --strict` is unavailable because `openspec` is not
  on PATH. The active proposal, design, control-plane spec, and tasks were
  inspected and synchronized with the implementation.
- Plain `treefmt` cannot open its read-only global cache; the no-cache run
  passed and the commit hook also passed treefmt.
- Full `just test`, `just ci`, coverage, and e2e were not run, as required by
  the repository and review instructions.
- No live `thegn` invocation, migration, or normal state database was used;
  all fork checks used isolated temporary state.
- The PATH `thegn` does not support `dispatch report` (`dispatch` is an
  unrecognized subcommand), so no dispatch report was filed.
