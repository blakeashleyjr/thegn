# THE-89 architect review

## Verdict

REVISE

Revision chunk: `.thegn/pipeline/THE-89/code/chunk-3.md`

## Review basis

- Merged `main` into `tg/the-89-error-glyph-tool-calls` as merge commit
  `a429d2be`; no conflicts occurred.
- Reviewed the complete `git diff main...HEAD`, the design, both chunk specs,
  both completion reports, and every `Unverified` section.
- Applied and committed the small lifecycle correction in `9b266cee`:
  input and OSC-blocked transitions now clear and publish transient agent-error
  state.

## Blocking gaps

1. The event bridge has no initial state snapshot, so an already-active error in
   a detached session is missed after compositor restart/reconnect.
2. A daemon/WebSocket loss without `SessionExit` leaves active entries in the
   host-global cache, allowing a stale glyph to persist indefinitely.
3. Cache transitions do not pulse the compositor's existing waker/model refresh
   path, contrary to the repository's off-loop producer contract.
4. The default `permission denied` substring can promote ordinary tool-call
   output (`Error: permission denied`) to `Failure`, violating THE-89's noise
   boundary. Authentication defaults need narrowing or context gating.

These are covered with acceptance tests and implementation direction in the
revision chunk. The missing manual live-agent check and unrun broad gates remain
non-blocking verification follow-ups, but the four issues above require code
before approval.

## Mandatory verification

Passed with `XDG_STATE_HOME=/tmp/thegn-review-state.EHxYZC`:

- `cargo nextest run -p thegn-core -E 'test(env_overlay) | test(config_example) | test(control_schema) | test(capability) | test(db)'` — 512 passed.
- `cargo nextest run -p thegn-host -E 'test(complete) | test(help) | test(catalog_tests) | test(platform_ratchet) | test(mq_assets) | test(render_plan)'` — 120 passed.

Also passed after the review correction: formatting, `git diff --check`, and
the focused host lifecycle/cache tests (2 passed).
