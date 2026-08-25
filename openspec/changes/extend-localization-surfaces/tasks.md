# Tasks — localization surfaces

Ordering note: everything below works with only en-US embedded (routing a
literal through the new helpers renders identically), so no phase blocks on
`add-localization`'s chrome-string sweep — but phase 2's chrome sites should
land after or alongside it to avoid double-touching the same lines.

## 1. Determinism + parity substrate

- [ ] 1.1 `THEGN_E2E=1` pins the active locale to `en-US` (a pin in
      `e2e_freeze`, applied before `i18n::init` resolves `[ui] language` /
      host locale) — **unit test**: init under the freeze with
      `language = "ja-JP"` still resolves `en-US`.
- [ ] 1.2 Pseudolocale generator in `thegn-core` (pure: en-US `.ftl` →
      accented, ~1.4x width-expanded bundle, `⟦…⟧` markers), served when
      `THEGN_PSEUDOLOCALE=1` and not `THEGN_E2E` — **unit tests**: expansion
      factor, non-ASCII output, interpolation placeholders preserved,
      freeze wins over pseudolocale.
- [ ] 1.3 Key-parity fold in `thegn-core` (pure): per embedded locale, the
      orphan-key set (fail) and missing-key set (report) against the en-US
      schema — **unit tests** for both directions.
- [ ] 1.4 `thegn doctor`: print resolved locale + per-locale key coverage
      from the parity fold (extends existing verb output; no new CATALOG row).
- [ ] 1.5 Truncation proof: a unit test drives a chrome layout site with the
      pseudolocale and asserts cell-width truncation within the panel budget
      (makes `add-localization`'s layout requirement testable today).

## 2. The time/date layer

- [ ] 2.1 `thegn_core::i18n_time` (pure): relative age, duration,
      month/weekday names, clock format, all through Fluent keys with CLDR
      plural categories — **unit tests**: singular/plural forms, en-US
      output identical to today's literals (no baseline churn), pseudolocale
      pass.
- [ ] 2.2 Route the chrome relative-age/duration sites through it, absorbing
      `util::age` and the duplicate helpers: `panel/sections/mod.rs::fmt_secs` + its callers (`panel/sections/{ci,hosts,git,notifications}.rs`),
      `detail.rs`, `detail/status_modal.rs` (`fmt_uptime`). CLI sites
      (`cmd/host.rs::age`, `cmd/kaneo.rs::age_secs`, `cmd/doctor.rs`) keep
      en-US per the CLI boundary; `axis.rs::age_unit`'s single-char axis
      suffixes stay untranslated by design (width-fixed).
- [ ] 2.3 Calendar month/weekday names (`calendar/grid.rs` weekday row,
      `detail/calendar/render.rs` month names) and the `[bars]` date/clock
      widgets' name tokens (`%a`/`%A`/`%b`/`%B`, pre-expanded from the locale
      tables before chrono formats the numerics) through the layer
      (coordinate with `add-calendar-and-world-clock`; e2e freeze already
      pins `now`, 1.1 pins the locale, so baselines hold).

## 3. Bidi/RTL safety

- [ ] 3.1 Neutralize bidi control characters (U+202A–U+202E, U+2066–U+2069,
      LRM/RLM) in user-supplied strings at the chrome compose edge (thegn-drawn
      text only; pane content untouched) — **unit tests**: RTL-override branch
      name renders with no bidi controls; a `{`-laden name renders literally
      through `t!` interpolation.
- [ ] 3.2 Lock the `t!` isolate-stripping policy with the 3.1 pair (design
      open question 1) — **unit test** on interpolated RTL data end-to-end.
- [ ] 3.3 Document the RTL stance (no RTL locale shipped; RTL data safe) in
      `docs/help/terminal-compatibility.md`; note the restart-to-switch
      behaviour and CLI-stays-English policy in the `[ui] language` comment in
      `config/config.toml.example` and `docs/help/configuration.md`.

## 4. Help corpus mechanism

- [ ] 4.1 Per-locale page trees: embed `docs/help/<locale>/<page>.md` beside
      the canonical `SOURCES`; F1 lookup resolves active-locale page, else
      canonical — **unit tests**: fallback per page; unknown locale serves
      canonical corpus untouched.
- [ ] 4.2 Assert the three help ratchets and the registered/orphan-page tests
      evaluate the canonical corpus only (a translated tree must not affect
      any ratchet) — **test** with a fixture translated page.
- [ ] 4.3 Generated-page boundary: keybindings page chrome through `t!`
      (labels arrive with `add-localization`'s ACTION_SPECS sweep);
      config-reference page stays canonical English — **test**: generated
      pages registered and English under a non-en locale.

## 5. CLI boundary + workflow

- [ ] 5.1 **Test**: `wt list --json` output and doctor probe words are
      byte-identical under `[ui] language = "ja-JP"` vs default (the `cli`
      delta's requirement).
- [ ] 5.2 Translation workflow note (in-repo `.ftl` by PR, en-US is the
      schema, parity test is the gate, Crowdin/Weblate deferred) in
      `CONTRIBUTING.md`.
- [ ] 5.3 Run `just ci` once (includes `openspec-validate`).
