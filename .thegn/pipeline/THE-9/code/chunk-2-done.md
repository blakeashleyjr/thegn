# THE-9 chunk 2 completion

Implemented workspace merge-queue token rendering, shared paint/hit geometry,
rail urgency tinting, token click routing, and the workspace context-menu
route to the existing Work → Merge queue panel section.

The token uses the core rollup policy, capability-resolved queue markers, and
palette tokens. Full sidebars degrade count → marker → hidden while preserving
the workspace label floor; rail mode keeps one row and only tints the existing
workspace initial. Token spans are recorded by the sidebar placement pass and
copied into `RowHit`.

## Verification

- `RUSTC_WRAPPER= just quick thegn-host`
- `RUSTC_WRAPPER= cargo nextest run -p thegn-host sidebar_view` — 35 passed
- `RUSTC_WRAPPER= cargo nextest run -p thegn-host sidebar_mouse` — 24 passed
- `RUSTC_WRAPPER= cargo nextest run -p thegn-host sidebar_keys` — 11 passed
- `RUSTC_WRAPPER= cargo nextest run -p thegn-host render_plan` — 20 passed
- `cargo fmt --all -- --check`
- `git diff --check`

## Unverified

- E2E/muse baselines were not run or re-recorded, per the chunk policy.
- Full-workspace gates and repository-wide ratchet suites were not run; they
  remain review/lead-stage verification.

Commits: `8f151e12` early adapter checkpoint; `41c44229` final code commit
(`feat(the-9): add workspace merge queue token`).
