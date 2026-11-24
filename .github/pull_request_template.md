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

Routine pull request CI runs `fmt`, `lint`, `test`, and `smoke` on GitHub-hosted
Linux runners. It does not run coverage or cross-target checks, and it does not
provide native macOS or Windows coverage.

To run the optional macOS, Windows (including artifact smoke), and e2e
(visual-regression) jobs for a chosen branch or ref, manually dispatch the CI
workflow with its `extras` input enabled:

```sh
gh workflow run ci.yml --ref <ref> -f extras=true
```

Dispatch is restricted to users with write access to this repository, so fork
PR authors should ask a maintainer to run it. Find the resulting run in GitHub
**Actions → CI**, listed for the selected ref.

## Notes for reviewers

<!-- Anything reviewers should focus on, known follow-ups, screenshots, etc. -->
<!-- For shared contracts, cite the early combined SHA and focused test. When agent commands, instruction paths, or delivery ownership change, update AGENTS.md or its linked guide in this PR. -->
