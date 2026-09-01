# THE-27 chunk 2 complete

Implemented PR review presentation and agent handoff in the host:

- Added shared PR diff row projection with exact new-side anchoring and explicit outdated feedback.
- Added PR-view and DiffView review sources, inline threads, unresolved counts, resolved filtering, navigation, and source/status affordances.
- Added identity-checked review snapshot hydration plus generation-safe off-loop conversation/diff fetch and complete-only cache writes.
- Added live-agent discovery and non-submitting bracketed-paste handoff, with headless `PrReview` fallback using the existing safety rules.
- Documented the new review keys and stale/loading/unsupported behavior.

Verification:

- `env XDG_RUNTIME_DIR=/tmp TMPDIR=/tmp RUSTC_WRAPPER= just quick thegn-host`
- `cargo nextest run -p thegn-host pr_view`
- `cargo nextest run -p thegn-host diff_view`
- `cargo nextest run -p thegn-host review_handoff`
- `cargo nextest run -p thegn-host help::ratchet`
- `cargo nextest run -p thegn-host ratchet` (including glyph/color ratchets)

## Unverified

- E2E and full-workspace tests/builds were not run per the chunk policy.
- No live forge, agent pane, or headless agent dispatch was exercised.
