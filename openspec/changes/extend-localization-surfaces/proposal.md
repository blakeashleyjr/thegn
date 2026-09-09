# Complete localization surfaces

Linear: THE-51 (In Progress, Plugin UI Platform)

## Why

thegn now has a credible localization substrate, but not full localization.
Fluent loading, en-US/ja-JP bundles and fallback, deterministic startup locale,
pseudolocale tooling, exact bundle parity, basic formatting primitives, and a
bounded statusbar/palette proof are delivered. Most native chrome, time/date
phrases, help, bidi safety, CLI invariants, and plugin-authored strings remain.

## What Changes

Work proceeds as six independently reviewable milestones:

1. **Native chrome:** migrate user-visible host chrome to keyed Fluent messages,
   shrink the literal-debt ratchet, expose locale/coverage in doctor, and prove
   long-string layout with the pseudolocale.
2. **Time/date:** replace duplicated English age, duration, month, weekday, and
   name-producing clock paths with the shared locale-aware formatter layer.
3. **Bidi/RTL safety:** publish the LTR-only product stance and neutralize bidi
   controls in user data at the chrome composition boundary.
4. **Help:** add per-page localized help lookup with canonical English fallback;
   canonical pages remain the only help-ratchet authority.
5. **CLI stability:** prove JSON, exit behavior, doctor vocabulary, and key-list
   machine surfaces are locale-independent; human CLI prose remains English.
6. **Plugin strings:** define a versioned plugin string-key/fallback contract so
   plugins can participate without injecting untrusted Fluent resources into
   the host bundle.

## Delivered substrate (not completion)

- en-US/ja-JP Fluent resources, per-key fallback, and startup-only resolution.
- `THEGN_E2E` locale pin and a deterministic pseudolocale generator.
- exact embedded-locale key parity and pure integer/date/plural primitives.
- a bounded adapter used by statusbar and palette, plus a shrink-only raw-
  literal debt inventory.

These establish feasibility; they do not satisfy the six milestones above.

## Impact

- Specs: `localization`, `help`, `cli`, and `plugin-api` deltas.
- Roadmap: the full THE-51 milestone plan; plugin strings complement the v0.3
  contract alignment in THE-106.
- No new catalog verb, database migration, poll, or runtime locale switching.

## Non-goals

Translating user data or pane output, loading arbitrary runtime locale packs,
mirrored RTL layout, localized config keys, and hosted translation services.
