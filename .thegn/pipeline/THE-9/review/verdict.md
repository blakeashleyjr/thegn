# THE-9 security/test/bug review

PASS

The branch was reviewed after the required `git merge main` (already up to
date), including the full `git diff main...HEAD`, the architecture/design
checklist, every coder `Unverified` section, and the architect approval.

One user-invoked failure path was fixed and tested in `7fd6e717`
(`fix(the-9): stop queue panel after failed activation (review)`): the new
merge-queue activation route ignored a failed workspace activation and opened
the panel for the previous workspace. It now preserves the activation failure
diagnostic and stops before panel selection.

Validation:

- `just quick thegn-core` and `just quick thegn-host`: passed.
- THE-9 scoped tests: core merge-queue policy 4/4; host sidebar view 35/35,
  mouse 24/24, keys 11/11, render plan 20/20, badges 14/14, fit 5/5, detail
  95/95, and the activation-failure regression 1/1.
- Post-fix land-gate filters: core 522/522, host 116/116, and
  `thegn-svc` control schema 1/1.
- `cargo clippy -p thegn-core --tests -- -D warnings` and the corresponding
  host check: passed.
- `treefmt`: passed, 2,212 files formatted, 0 changed.
- `openspec validate --all --strict`: 170/170 passed.
- `cargo fmt --all -- --check` and `git diff --check`: passed.

The architect-recorded unrelated baseline failure remains
`thegn-core::sandbox::tests::oci_local_secrets_go_to_env_file_not_argv`; the
THE-9 code does not touch that test or implementation. The repository's
`test/ratchet-check.sh` is absent, as previously reported by the architect.

## Snapshots

No e2e/muse snapshots were run or re-recorded, per the lane restriction. The
frame-affecting changes are expected to affect these architect-listed baselines:

- `sidebar__focused`: 100x30, 160x40
- `panel_work__work`: 100x30
- `chrome_regions__chrome`: 40x12, 80x24, 100x30, 160x40, 200x50
- `responsive_breakpoints__layout`: 40x12, 80x24, 100x30, 160x40, 200x50
- `glitch_hunt_chrome_consistency__bars`: kitty 80x24, 100x30, 160x40
- `glitch_hunt_panel_accordion__after`: 100x30, 160x40
- `themes__storm#styled`, `themes__light#styled`, `themes__abyss#styled`,
  `themes__ember#styled`: xterm 100x30
