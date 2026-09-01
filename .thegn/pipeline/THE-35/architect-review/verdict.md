# THE-35 architect review

REVISE

## Revision chunks

- `/home/blake/.superzej/worktrees/thegn/tg-the-35-sound-effects/.thegn/pipeline/THE-35/architect-review/revision-1.md`

## Findings

The core policy, host provider seam, bounded off-loop playback, live-attention
edge observer, trust filtering, documentation, and capability boundaries are
otherwise aligned with `architect/design.md`. The required revision is producer
coverage: `mentioned` at `crates/thegn-host/src/hydrate.rs:3715` and `overdue`
at `crates/thegn-host/src/hydrate_tracker.rs:117` still call
`put_notification_once` directly. Consequently `notifications.sound.per_kind`
cannot affect those catalog events, and moving them to the route without a
deduplicating helper would either lose emit-once behavior or replay sound on
every hydration. Implement the `record_once`-style helper and tests described
in the revision chunk.

## Review commits

- `8b74a282` — merged `main` into the feature branch before reviewing
  `git diff main...HEAD`.
- `c181b0ae` — added the missing OpenSpec scenarios, best-effort annotations,
  and a reasoned ignored-result ratchet pin.
- `ceb879cd` — recorded the semantic revision chunk.

## Verification

- Host mandatory nextest filter: passed (104/104).
- Service `control_schema`: passed.
- `just quick`: passed with `JUST_TEMPDIR=/tmp RUSTC_WRAPPER=`.
- Test-target clippy for touched code (`thegn-core`, `thegn-host`): passed.
- Core and host private rustdoc: passed with `RUSTDOCFLAGS=-D warnings`.
- Pinned `treefmt --ci`: passed, 0 files changed.
- Pinned `openspec validate --all --strict`: passed after the mechanical fix.
- Available ignored-result ratchet: passed after the scoped pin.
- Core mandatory nextest filter: one unrelated existing failure,
  `sandbox::tests::oci_local_secrets_go_to_env_file_not_argv` (329 passed
  before nextest cancelled the remaining tests); no THE-35 code was changed for
  it.
- `test/ratchet-check.sh` was not present; `test/ratchet.sh` was used.

Cross-platform player execution was not run, as recorded by the lane docs.
The required knowledge graph was absent, so the diff overlay could not be
generated. No migration or live-state binary invocation was performed.
