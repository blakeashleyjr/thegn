# THE-9 chunk 3 completion

Implemented the opt-in bottom-bar merge-queue widget and detail bridge.

- The default bottom bar no longer emits the legacy merge-queue badge.
- `[bars] bottom_right = ["mq"]` renders a scoped, core-policy-driven queue
  summary using the existing capability glyphs and palette tokens.
- The widget opens the existing unified merge-queue detail surface.
- Added the widget's fit priority and documented `mq` in config, the example
  config, bars/sidebar/merge-queue help, and the Unreleased changelog.
- Retained the legacy badge enum/detail plumbing for compatibility without
  emitting it from the default bar.

## Verification

- `cargo fmt --all -- --check`
- `just quick thegn-core` (with `RUSTC_WRAPPER=` and `XDG_RUNTIME_DIR=/tmp`)
- `just quick thegn-host` (with `RUSTC_WRAPPER=` and `XDG_RUNTIME_DIR=/tmp`)
- `cargo nextest run -p thegn-core bars_config_defaults`
- `cargo nextest run -p thegn-core env_overlay`
- `cargo nextest run -p thegn-host statusbar_badges`
- `cargo nextest run -p thegn-host statusbar_fit`
- `cargo nextest run -p thegn-host detail`
- `cargo nextest run -p thegn-host completion_slots_are_bound_or_pinned`
- `cargo nextest run -p thegn-host cli_control_verbs_cover_catalog`
- `cargo nextest run -p thegn-host ratchet`

All executed targeted tests passed. The host test build reported one existing
unused-import warning in the concurrent chunk-2 sidebar test while that chunk
was in flight; the final scoped host quick check passed after its wiring landed.

## Unverified

- E2E was not run and no muse snapshots were re-recorded, as required by the
  chunk policy. The conservative baseline list remains the one in the
  architect design.
- Full-workspace gates (`just test`, `just ci`, coverage, and e2e) were not run.
