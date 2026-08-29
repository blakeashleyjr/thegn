# THE-89 architect review

## Verdict

APPROVED

## Review basis

- Merged `main` into `tg/the-89-error-glyph-tool-calls` as merge commit
  `3756011e`; the merge completed cleanly and passed repository hooks.
- Reviewed the complete `git diff main...HEAD`, the architecture design,
  chunks 1–3, all completion reports, and every `Unverified` section.
- The implementation provides pure harness-banner classification, config
  override/validation, daemon session state, authoritative reconnect
  bootstrap, generation-scoped cache cleanup, and model-refresh/waker pulses.
- Applied and committed two small corrections in `29655324`:
  - removed the retired broad auth/permission defaults from the shipped config
    example;
  - processed completed output lines in order so normal output following a
    banner in the same PTY chunk clears the error state.

## Mandatory verification

Passed after the correction, with separate temporary `XDG_STATE_HOME` values
for each run:

- `cargo nextest run -p thegn-core -E 'test(env_overlay) | test(config_example) | test(control_schema) | test(capability) | test(db)'` — 513 passed.
- `cargo nextest run -p thegn-host -E 'test(complete) | test(help) | test(catalog_tests) | test(platform_ratchet) | test(mq_assets) | test(render_plan)'` — 121 passed.

Additional checks passed:

- `XDG_RUNTIME_DIR=/tmp RUSTC_WRAPPER= just quick thegn-host`
- focused agent-error/cache/session tests — 11 passed
- `cargo fmt --all -- --check`
- `git diff --check`

The manual live-agent/rendered-glyph check and broad workspace/e2e gates remain
follow-up verification items documented by the lane reports; they are
non-blocking for this review.
