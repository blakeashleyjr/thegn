# Extend localization — the surfaces add-localization leaves open

Linear: THE-51

## Why

THE-51 asks for "full localization". The in-flight `add-localization` change
already owns the core mechanism, and its scaffolding is in fact **landed on
main**: `crates/thegn-core/src/i18n.rs` (fluent-templates `static_loader`, the
`t!` macro, per-key en-US fallback), `crates/thegn-core/locales/en-US/main.ftl`,
`[ui] language = "auto"` (`config_ui.rs`, `config/config.toml.example`), and a
one-shot `i18n::init` in the startup waterfall (`run.rs`). Its spec also covers
plural-capable Fluent lookup and the cell-width layout invariant. What has NOT
started is its task 2.1 — routing chrome strings through `t!` (today there are
zero `t!` call sites outside i18n.rs's own tests, and en-US carries 2 demo
keys).

"Full" localization is more than that mechanism, and the following surfaces are
covered by no change today:

- **RTL.** No stated position. Terminal grids are logical-order LTR; almost no
  terminal renders bidi correctly, and thegn is its own compositor. Worse, the
  landed `t!` macro strips Fluent's FSI/PDI isolation marks (U+2068/U+2069),
  and RTL _user data_ (an Arabic branch name) interpolated into chrome can
  visually reorder neighbouring text — a Trojan-source-style spoofing surface,
  not just a rendering nit.
- **Dates, times, plurals-in-time.** Relative ages ("2h ago") have one shared
  core helper (`util::age`) but private duplicates beside it
  (`panel/sections/mod.rs::fmt_secs`, `detail/status_modal.rs::fmt_uptime`,
  `cmd/host.rs::age`, `cmd/kaneo.rs::age_secs`) — all English `format!`
  literals with no plural handling, with the " ago" suffix appended per call
  site (`detail.rs`, `panel/sections/{ci,hosts,git,notifications}.rs`,
  `detail/status_modal.rs`) — so the extraction sweep cannot localize them
  as-is. Month/weekday names are likewise English literals
  (`calendar/grid.rs`'s weekday row, `detail/calendar/render.rs`'s month
  names), and the masthead date widget's default `%a %b %-d` renders chrono's
  English names regardless of locale.
- **The help corpus.** 30 authored `docs/help/*.md` pages are embedded via
  `include_str!` (`help/pages.rs::SOURCES`) and gated by three prose/coverage
  ratchets; the keybindings and config-reference pages are generated at
  runtime. None of that has a localization story: what does F1 show a ja-JP
  user, and which corpus do the ratchets run against?
- **CLI vs chrome boundary.** `add-localization` says "chrome" but nothing
  forbids localizing `--json` output, exit-code semantics, `doctor` probe
  words, or `keys list` — the machine-consumed surfaces scripts grep. That
  boundary must be a spec'd requirement, not an accident.
- **e2e determinism.** muse baselines are byte-identical frames; the
  `THEGN_E2E=1` freeze (`e2e_freeze.rs`) pins clock/stats/version but not
  locale, so the first real translation makes every baseline flap with the
  host's `LANG`. And with only en-US embedded, nothing exercises "a longer
  translation" — the truncation invariant is spec'd but untestable.
- **Translation infrastructure.** Who produces and maintains locale files, and
  what stops `de-DE` silently drifting from en-US as keys are added?

## What Changes

A complementary change — it does **not** edit or re-scope `add-localization`;
the chrome-string extraction sweep and the `t!`/layout mechanism remain that
change's to finish. This change adds the policy and surface requirements around
it:

- **Locale determinism under test**: `THEGN_E2E=1` pins the active locale to
  `en-US` regardless of `[ui] language` / host locale (one more pin in
  `e2e_freeze`); baselines stay single-locale.
- **A pseudolocale** generated from en-US (width-expanded, non-ASCII) as pure
  core logic, used by unit tests to prove the truncation invariant without
  waiting for real translations, and available as a dev-only hook for
  eyeballing layout.
- **Key parity as a gate**: en-US is the key schema; an embedded locale with
  keys en-US lacks fails a unit test; missing translations fall back per key
  and `thegn doctor` reports the active locale and per-locale coverage.
- **One localizable time/date layer** in thegn-core (relative age, duration,
  month/weekday names, and the name-producing tokens of the `[bars]` date
  widget) routed through Fluent plural categories, replacing the scattered
  English literals and duplicated helpers. The user's strftime strings keep
  their structure; only the names chrono would render in English come from
  locale tables.
- **An honest RTL position**: chrome layout stays LTR and no RTL locale is
  shipped until a terminal-bidi story exists; bidi control characters in user
  data are neutralized at the chrome compose edge so an RTL branch name cannot
  reorder or spoof chrome text.
- **Help corpus policy**: canonical corpus is English; optional per-locale
  page trees (`docs/help/<locale>/…`) resolve per page with fallback to
  canonical; all three help ratchets evaluate the canonical corpus only;
  config-reference stays English by design, the keybindings page's generated
  chrome localizes with everything else.
- **CLI boundary policy**: machine-readable output (`--json` via
  `cmd::emit_json`, exit codes, doctor probe words, `keys list`) SHALL be
  locale-independent; human CLI prose stays English in this phase (chrome-only
  localization), spec'd so scripts never break under a user's locale.
- **Translation workflow**: in-repo `.ftl` files maintained by PR, guarded by
  the parity test; hosted platforms (Crowdin/Weblate — both speak Fluent) are
  an explicitly deferred growth path, not part of this change.

## Impact

- **Specs**: `localization` (ADDED requirements — same capability
  `add-localization`'s delta creates; the two merge on archive), `cli` (ADDED:
  locale-independent machine output), `help` (ADDED: per-locale resolution
  with canonical fallback).
- **In-flight changes**: complements `add-localization` (mechanism + string
  sweep stay there; this change's time-layer and help-page routing depend on
  its `t!` plumbing, which is already landed). Touches the same relative-age
  sites `add-calendar-and-world-clock` renders dates for — coordinate month/
  weekday-name keys with it. No overlap with the MCP write-tools branch.
- **Roadmap**: no existing tasks.md item covers localization (grep confirms);
  the audit phase wires this and `add-localization` into the roadmap together.
- **Capability catalog**: no new externally invokable operation — no CATALOG
  rows. `doctor`'s locale line extends an existing verb's output.
- **Config**: no new keys. `[ui] language` already exists; its
  `config.toml.example` comment gains the CLI-stays-English note. The
  pseudolocale hook is a dev-only env var (`THEGN_PSEUDOLOCALE`), deliberately
  not config.
- **Render/event-loop**: none. Locale resolution stays one-shot at startup
  (restart to change language — spec'd as explicit behaviour); localized
  strings only change what a `Full` compose draws; no new wake source, no
  SQLite change, no new help context key.

## Non-goals

- **Re-scoping `add-localization`.** Its mechanism, `[ui] language`, layout
  invariant, and chrome-string sweep are untouched here.
- **Translating user data** — branch names, commit messages, pane output.
- **Runtime locale switching.** Resolved once at startup; changing
  `[ui] language` takes effect on restart.
- **RTL/mirrored chrome layout.** Out of scope until terminals give a bidi
  substrate to stand on; this change only makes RTL _data_ safe.
- **Localized CLI prose and localized config-reference.** Deliberate policy,
  revisitable post-alpha.
- **Number/currency formatting.** The TUI shows counts and sizes; locale
  decimal separators buy nothing at alpha.
- **Hosted translation platforms (Crowdin/Weblate).** Deferred until there is
  a translator community to serve; the Fluent format keeps the door open.
- **Recording per-locale e2e baseline suites.** Baselines stay en-US; a
  per-locale smoke can come with the first shipped second locale.
