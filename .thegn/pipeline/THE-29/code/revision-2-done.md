# THE-29 — fork existing sessions

## Findings fixed

- Agent session opens now load the effective per-request configuration once
  inside the existing blocking DB boundary and use that same configuration for
  both launch resolution and the retained in-memory fork recipe. A provider
  change after daemon startup can no longer make the retained source disagree
  with the provider actually launched.
- Added a hermetic daemon regression covering a changed provider after service
  construction and asserting that the later fork plan uses the fresh provider.
- Added a focused matching-provider `resolve_fork` regression proving that the
  selected native harness command survives launch composition while the current
  configured agent environment is applied.

## Commits

- `305c1f96` — `fix(the-29): retain effective agent provider (revision 1)`
- `48e0716a` — `fix(the-29): cover matching native harness resolution (revision 1)`
- `docs(the-29): record revision 2 completion` — this summary

## Verification

- `XDG_RUNTIME_DIR=/tmp RUSTC_WRAPPER= just quick thegn-host` — passed.
- `XDG_RUNTIME_DIR=/tmp RUSTC_WRAPPER= cargo clippy -p thegn-host --tests -- -D warnings` — passed.
- `XDG_RUNTIME_DIR=/tmp RUSTC_WRAPPER= cargo nextest run -p thegn-host agent_open_retains_the_provider_used_by_fresh_resolution matching_native_fork_preserves_source_command_and_fresh_launch_context` — 2/2 passed.
- `treefmt --no-cache --allow-missing-formatter` — passed, no changes.
- `git diff --check` — passed.

## Unverified

- Full-workspace gates (`just test`, `just ci`, coverage, rustdoc, and e2e) were not run per the revision dev-loop restriction.
- `openspec validate --all --strict` remains unverified because `openspec` is not available on PATH, as recorded by the prior revision.
- No live `thegn` invocation, migration, or normal XDG state database was used.
