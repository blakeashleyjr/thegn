# Tasks — complete localization surfaces (THE-51)

## Delivered substrate

- [x] Embed en-US and ja-JP Fluent resources with per-key en-US fallback.
- [x] Resolve locale once at startup with deterministic precedence.
- [x] Pin `THEGN_E2E` to en-US and implement a pure pseudolocale generator.
- [x] Enforce exact embedded-locale key parity.
- [x] Provide pure integer/date/plural formatting primitives.
- [x] Migrate bounded statusbar and palette proof surfaces.
- [x] Add a shrink-only raw-literal debt inventory for remaining chrome.

## Milestone 1 — native chrome

- [ ] Migrate all user-visible native chrome strings to stable Fluent keys and
      shrink the debt inventory to zero or documented non-user-facing cases.
- [ ] Report resolved locale and exact bundle parity/coverage in `thegn doctor`.
- [ ] Add pseudolocale render tests for truncation, flex, popup, and narrow-width
      behavior across representative chrome zones.
- [ ] Update help/configuration guidance for selection, fallback, and restart.

## Milestone 2 — time and date

- [ ] Complete the shared relative-age/duration/month/weekday formatter with
      locale plural selection and name tables.
- [ ] Route panel/detail/calendar/statusbar chrome through it and remove private
      English formatter duplicates.
- [ ] Preserve configured format structure and byte-stable en-US output.

## Milestone 3 — bidi and RTL safety

- [ ] Neutralize bidi controls in user data at the chrome composition edge,
      leaving pane content untouched.
- [ ] Test branch/path/host interpolation and document the LTR-only stance.

## Milestone 4 — localized help

- [ ] Add per-locale page lookup with page-by-page canonical fallback.
- [ ] Prove translated trees cannot satisfy or break canonical help ratchets.
- [ ] Keep generated config reference English; localize eligible keybinding
      chrome only after its native labels are migrated.

## Milestone 5 — CLI stability

- [ ] Prove JSON, exit codes, doctor result words, and `keys list` are invariant
      under en-US, ja-JP, pseudolocale, and host-locale changes.
- [ ] Document that human CLI prose remains English for this phase.

## Milestone 6 — plugin strings

- [ ] Specify versioned namespaced keys, fallback text, locale selection,
      sanitization, size/budget limits, and compatibility behavior.
- [ ] Implement registration/render integration and contract tests without
      allowing plugins to mutate host bundles.

## Validation

- [ ] Run focused core/host tests, literal/help ratchets, schema checks, strict
      OpenSpec validation, and the full repository gate for each milestone.
