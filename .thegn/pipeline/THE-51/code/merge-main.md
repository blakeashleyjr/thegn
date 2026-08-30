# THE-51 — merge main and re-gate (Lead work order)

files:
  - (whatever the merge and the gate require)

## Why this exists

Row 346's review passed the full gate (7090) and that was true at the time.
`tg/the-52-packaging` has landed since, and `thegn land` now refuses this
branch: **gate red against the new base.** This is the fourth branch to hit
this; it is base drift, not a defect in the work.

## Done criteria

- `git merge main`. Resolve conflicts by keeping BOTH sides wherever the clash
  is two independent additions to a registry/module list/help page — the
  localization work and the packaging work are unrelated, so nothing should
  actually be in tension.
- Confirm nothing landed was lost: after the merge, main's tip content for any
  conflicted file must still be present.
- Run `RUSTC_WRAPPER= THEGN_ALLOW_HEAVY=1 just test` and fix whatever it
  reports. Name the failing tests in your artifact even if you fix them, so the
  supervisor can see what base drift actually broke.
- Preserve the row-337/343/346 work: locale precedence, ja-JP strict parity,
  the POSIX LC_ALL/LANG normalization, and the i18n literal ratchet.
- Report PASS only with a green full gate against current main.
