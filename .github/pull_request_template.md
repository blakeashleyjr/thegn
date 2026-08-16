<!--
Thanks for contributing to thegn! Keep the title in conventional-commit style,
e.g. `feat(sidebar): ...` or `fix(merge): ...`.
-->

## What & why

<!-- What does this change do, and why? Link any issue with "Closes #123". -->

## Checklist

- [ ] `just ci` is green locally (or the relevant subset for a docs-only change).
- [ ] New/changed behavior has tests; new `thegn-core` logic keeps the 95% coverage gate.
- [ ] If actions/keybinds/zones/panel-sections changed, the `docs/help/` page and help ratchet are updated in this PR.
- [ ] Conventional-commit PR title.

## Optional CI

<!-- Routine CI (build/test/lint/coverage/cross-check) runs automatically.
     Add a marker to your commit message to opt into the heavier jobs: -->

- `[ci-macos]` — full macOS aarch64 build + tests (billed at 10x)
- `[ci-windows]` — full Windows msvc build + kernel tests
- `[ci-e2e]` — muse visual-regression suite

## Notes for reviewers

<!-- Anything reviewers should focus on, known follow-ups, screenshots, etc. -->
