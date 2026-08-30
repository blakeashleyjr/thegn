# THE-51 — Full localization architecture

## Decision summary

Build the missing localization substrate around the existing embedded Fluent
catalog, then prove it on one deliberately bounded surface: the statusbar and
command palette with a small `ja-JP` catalog. Do not start a whole-chrome
translation sweep in this issue.

The target flow is:

```text
[ui].language (explicit) -> LC_ALL -> LANG -> en-US
                                      |
                         THEGN_E2E=1 -> en-US
                                      v
                    one startup locale resolver in thegn-core
                                      v
                  one embedded Fluent catalog lookup at UI edges
```

Locale resolution is a startup value, not live state. The host takes a small
environment snapshot before initialization and passes it to a pure core
resolver. No locale file is read at runtime, no wake source is added, and no
catalog lookup is placed in the event loop's idle path beyond the strings being
composed for a dirty frame.

The existing per-key fallback remains a defensive edge behavior for an unknown
locale or malformed translation, but the shipped-locale parity gate is strict:
every key in `en-US` must exist in every shipped locale. This is the binding
requirement for this issue and intentionally prunes the draft's proposal that
missing keys in a shipped locale are an allowed steady state.

## Verified audit matrix

| Surface / question                | Evidence on this branch                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 | Finding                                                                                                                                                                                                           | Delivery decision                                                                                                                                                                                                           |
| --------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| i18n layer and catalog            | [`i18n.rs:3-12`](../../../../../crates/thegn-core/src/i18n.rs) has `fluent_templates::static_loader!`, `./locales`, and `en-US` fallback; [`i18n.rs:47-78`](../../../../../crates/thegn-core/src/i18n.rs) defines `t!` with interpolation; [`locales/en-US/main.ftl:1-2`](../../../../../crates/thegn-core/locales/en-US/main.ftl) has only two demo messages                                                                                                                                                                                                                                                                                                                                                           | Exists, embedded, and substrate-free in dependency shape, but is not a product catalog                                                                                                                            | Harden in chunk 1; add only statusbar/palette keys in chunk 2                                                                                                                                                               |
| Locale selection                  | [`config_ui.rs:49-55`](../../../../../crates/thegn-core/src/config_ui.rs) already exposes `[ui].language`; [`config_ui.rs:122-126`](../../../../../crates/thegn-core/src/config_ui.rs) defaults it to `auto`; [`i18n.rs:17-34`](../../../../../crates/thegn-core/src/i18n.rs) currently resolves `auto` through `sys_locale` and stores a `OnceCell`; [`run.rs:580-637`](../../../../../crates/thegn-host/src/run.rs) applies the freeze and calls `i18n::init` once                                                                                                                                                                                                                                                    | Config override and once-only startup exist. The exact required `config -> LC_ALL -> LANG -> en-US` order is not explicit or pure; the draft's “sys-locale is landed” claim is therefore only partially satisfied | Make the resolver pure and explicit in chunk 1; do not add a new config key or `THEGN_UI_LANGUAGE` knob                                                                                                                     |
| Config precedence / ratchet       | [`config.rs:1-14`](../../../../../crates/thegn-core/src/config.rs) documents defaults → file → `THEGN_*` env → CLI; [`config.rs:5660-5723`](../../../../../crates/thegn-core/src/config.rs) implements that order; [`tests/env_overlay_coverage.rs:1-12`](../../../../../crates/thegn-core/tests/env_overlay_coverage.rs) gates shallow config knobs                                                                                                                                                                                                                                                                                                                                                                    | `[ui].language` currently has no `THEGN_UI_LANGUAGE` overlay. Ambient `LANG`/`LC_ALL` are locale inputs, not config-schema keys                                                                                   | Do not invent a new config knob. The resolver reads the host's `LC_ALL` then `LANG` only when config is `auto`; document that distinction. No env-overlay allowlist change is valid unless a coder adds a real config field |
| Production catalog use            | `rg 't!\\(' crates` finds only the macro's own tests; [`i18n.rs:80-91`](../../../../../crates/thegn-core/src/i18n.rs) tests lookup/fallback only                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        | The catalog is not connected to production chrome                                                                                                                                                                 | Route the bounded statusbar/palette strings through a single host adapter in chunk 2; full chrome/help/CLI extraction is follow-up work                                                                                     |
| Statusbar                         | [`statusbar_left.rs:27-83`](../../../../../crates/thegn-host/src/statusbar_left.rs) builds the left cluster and emits the help/keyhint labels; [`statusbar_badges.rs:25-102`](../../../../../crates/thegn-host/src/statusbar_badges.rs) emits attention/network/CI text; [`chrome.rs:1502-1557`](../../../../../crates/thegn-host/src/chrome.rs) emits bottom-bar widget text; [`run.rs:173-200`](../../../../../crates/thegn-host/src/run.rs) sets full and compact mode labels                                                                                                                                                                                                                                        | Existing layout is already width-aware (`seg_width`, atomic fitting), but user-visible English remains at composition sites                                                                                       | Bounded proof surface: localize static words, mode labels, and plural match/count messages while preserving glyph/capability seams and layout budgets                                                                       |
| Command palette                   | [`palette.rs:221-316`](../../../../../crates/thegn-host/src/palette.rs) draws `jump`, `menu`, `type to filter…`, `matches`, `move`, `run`, and `dismiss`; [`palette.rs:321-388`](../../../../../crates/thegn-host/src/palette.rs) builds rows from action specs; [`keymap_specs.rs:6-31`](../../../../../crates/thegn-host/src/keymap_specs.rs) defines labels used by the palette and help                                                                                                                                                                                                                                                                                                                             | Palette has a clean data path but its chrome and action labels bypass the catalog                                                                                                                                 | Localize palette chrome and resolve action labels through catalog keys. Keep hidden search keywords/search data untouched; do not translate user-provided workspace/folder names                                            |
| Date / number formatting          | [`chrome.rs:1317-1323`](../../../../../crates/thegn-host/src/chrome.rs) formats uptime with English unit literals; [`chrome.rs:1446-1451`](../../../../../crates/thegn-host/src/chrome.rs) delegates date/clock names to chrono; [`weather.rs:399-412`](../../../../../crates/thegn-core/src/weather.rs) emits `just now`/`ago`; [`detail/calendar/render.rs:426-443`](../../../../../crates/thegn-host/src/detail/calendar/render.rs) owns English month names; [`calendar/grid.rs:132-150`](../../../../../crates/thegn-core/src/calendar/grid.rs) owns English weekday headers; [`calendar/locale.rs:15-83`](../../../../../crates/thegn-core/src/calendar/locale.rs) resolves week-start and 12/24-hour preferences | No shared localized time/number formatter. `calendar::locale` exists, but it resolves week start and 12/24-hour preference, not message names                                                                     | Add and unit-test a small pure formatter in chunk 1; integrate it into the rest of the application as a follow-up. Do not add ICU or change all date sites in THE-51's implementation chunks                                |
| RTL and bidi                      | [`i18n.rs:57-60`](../../../../../crates/thegn-core/src/i18n.rs) strips FSI/PDI from Fluent output; no shipped RTL locale exists; pane emulation is separate from chrome                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 | RTL locale layout is not supportable yet, and the current isolate policy needs a deliberate edge-safety decision                                                                                                  | Follow-up: neutralize bidi controls only in composed chrome user data, preserve pane bytes, and test literal `{` data. Do not ship `ar`/`he` in this issue                                                                  |
| Help corpus                       | [`pages.rs:1-64`](../../../../../crates/thegn-host/src/help/pages.rs) embeds authored `docs/help/*.md` with `include_str!` and appends generated pages; [`ratchet_tests.rs:72-80`](../../../../../crates/thegn-host/src/help/ratchet_tests.rs) excludes generated pages from action claims; [`pages.rs:101-140`](../../../../../crates/thegn-host/src/help/pages.rs) enforces disk/include parity                                                                                                                                                                                                                                                                                                                       | Canonical English help, generated keybindings, generated config-reference, and authored/panel help ratchets already work. There is no locale tree/fallback                                                        | Follow-up: per-page locale trees with canonical-only ratchets. Never hand-write either generated page                                                                                                                       |
| CLI and machine output            | [`cmd/mod.rs:106-113`](../../../../../crates/thegn-host/src/cmd/mod.rs) centralizes compact JSON emission; [`cmd/list.rs:117-123`](../../../../../crates/thegn-host/src/cmd/list.rs) uses it for `--json`; `test/json-emit-ratchet.txt` guards drift                                                                                                                                                                                                                                                                                                                                                                                                                                                                    | JSON shape, exit codes, and probe tokens must stay byte-stable; human CLI prose is not yet catalog-backed                                                                                                         | Follow-up: catalog-back human CLI prose only where it does not alter machine output. No catalog lookup or output change in chunk 2                                                                                          |
| Glyph fallbacks / capability seam | [`caps.rs:127-135`](../../../../../crates/thegn-host/src/caps.rs) documents the glyph chokepoint; [`docs/ARCHITECTURE.md:88-91`](../../../../../docs/ARCHITECTURE.md) requires truecolor/Unicode composition and edge degradation; `test/color-literal-ratchet.txt` and `test/glyph-literal-ratchet.txt` are shrink-only                                                                                                                                                                                                                                                                                                                                                                                                | Existing i18n work must not introduce glyph literals or bypass caps                                                                                                                                               | Keep translated text independent of glyph selection; retain all `caps::active_glyphs()` use and existing ratchets                                                                                                           |

### Draft verification and pruning

The openspec draft at `openspec/changes/extend-localization-surfaces/` correctly
identifies the missing date/time, RTL, help, CLI-boundary, and determinism
surfaces. Its claims that the substrate is “landed on main” are only partly
true on this branch: the loader/config/startup pieces exist, but the catalog
has two demo keys and zero production lookups. `calendar::locale` is already
landed, but it is calendar preference resolution rather than localization.

The draft's strict orphan-key check is retained, but its “missing translations
are allowed and reported” parity behavior is cut because THE-51 requires every
default key to exist in each shipped locale. Its broad time/date, RTL, per-page
help, CLI, and workflow task groups are moved to “Follow-ups to file” below so
the issue does not become a big-bang translation or add ICU-sized machinery.
The generated-help boundary, no-new-catalog-row decision, restart-only locale
selection, and `THEGN_E2E` determinism direction are retained.

## Architecture

### Core substrate

Keep `thegn-core` substrate-free and add sibling modules rather than growing a
god file:

- `i18n.rs` remains the public facade, embedded catalog declaration, macro, and
  startup-facing API.
- `i18n_locale.rs` owns a pure resolver. Inputs are `config_language`, optional
  `lc_all`, optional `lang`, and an explicit `freeze` flag. An explicit valid
  config language wins; `auto` selects `LC_ALL`, then `LANG`, then `en-US`.
  Empty/invalid values degrade to `en-US` with a diagnostic. The resolver does
  not read process environment itself.
- `i18n_parity.rs` owns the static `include_str!` source table and pure key-set
  fold. The default key set is the schema; an orphan or missing default key
  names the locale and key in the failure. Adding a locale requires adding its
  source to the one table and the parity test.
- `i18n_pseudo.rs` owns the test/developer pseudolocale transform. It must
  preserve Fluent argument placeholders and must never become a selectable
  shipped locale. `THEGN_PSEUDOLOCALE=1` may select it only outside
  `THEGN_E2E`; the default production path remains embedded catalog lookup.
- `i18n_format.rs` owns small pure formatting primitives: the supported
  locale's plural category, deterministic integer formatting, and short date
  formatting. It may use the already-present `chrono` types, but no ICU/CLDR
  runtime is introduced. The implementation is deliberately limited to the
  shipped proof locales and a safe English fallback; broader calendar names,
  relative-time wording, and user-configured date patterns remain follow-ups.

The host's `e2e_freeze` supplies the explicit `en-US` override before the
startup call. The active locale is stored once (first-set-wins); config reloads
must not relocalize a running instance. This preserves the 0% idle contract and
the “resolve before first frame” startup contract.

No `icu4x`, locale-pack filesystem loader, runtime parser, database migration,
provider seam, capability-catalog row, or new config key is justified. Fluent
already exists in `thegn-core` (`Cargo.toml:64-67`), and the helper is pure and
unit-tested. The existing `sys-locale` dependency may be removed if the explicit
environment snapshot makes it unused; do not replace it with another heavy
locale dependency.

### One catalog and one UI adapter

There is one embedded `LOCALES` catalog. The host may add one small
`i18n_surface.rs` adapter for typed message keys and interpolation helpers, but
it must delegate every lookup to the core catalog/macro. Draw sites must not
construct localized prose with `format!`; they pass numbers and user data as
arguments. User data is data: workspace, branch, folder, query, and plugin
labels are not translated or treated as Fluent source.

For the bounded proof surface, use stable keys such as `statusbar.offline`,
`statusbar.mode.normal`, `palette.title`, `palette.filter_placeholder`,
`palette.footer.move`, `palette.footer.run`, `palette.footer.dismiss`,
`palette.matches`, and `action.<stable-action-id>`. The exact key spelling may
be normalized by the coder, but it must be declared once in the catalog and
used by both the statusbar/palette code paths. `en-US` includes all keys;
`ja-JP` includes the complete same key set, translating a small representative
subset and carrying canonical English for the rest. This proves selection,
fallback-free parity, interpolation, and width behavior without pretending to
translate the whole application.

The existing `seg_width`/`Line::split`/fit behavior is the layout seam. Tests
must assert cell width, not byte or character count, and must prove a longer
Japanese/pseudolocalized label is clipped or shed within its existing budget.
No glyph literal is added; `caps::active_glyphs()` remains the only glyph
degradation seam.

### Ratchets and gates

Add a shrink-only `test/i18n-literal-ratchet.txt` ledger and its `just lint`
invocation for raw user-facing literals in the audited chrome surfaces. The
initial ledger is the pre-change hit set; chunk 2 removes the statusbar/palette
hits it addresses. The ratchet must ignore comments, tests' explanatory prose,
catalog keys, and user data. A new production raw UI literal fails unless it is
routed through the adapter or deliberately pinned with a reason. This is a
file-level debt register like the color/glyph ratchets, not a claim that a grep
can understand arbitrary English in every Rust string.

Chunk 1 adds no config field, so `test/env-overlay-ratchet.txt` is checked but
must not gain a bogus `ui.language` entry. It adds no command, capability,
completion slot, control wire field, action id, help page, or generated-page
input; therefore completion-slot, control-schema, and help ratchets remain
unchanged and are run as regression checks. If a coder changes a ratchet-relevant
file despite that rule, the same chunk must include the corresponding shrink-only
update and explain it in the commit.

## Delivery order

Chunks are intentionally serial because the proof surface consumes the
substrate. They have disjoint ownership within each chunk; the only planned
cross-chunk overlap is the single startup call site in `run.rs`, which is called
out explicitly in both chunk specs.

1. Substrate: pure resolver, strict embedded parity, e2e pin,
   pseudolocale/formatting test seams, and raw-literal ratchet plumbing.
2. Proof surface: complete `en-US`/`ja-JP` message sets and statusbar/palette
   adapter routing with cell-width tests.

No `just test`, `just ci`, full-workspace compile, e2e, migration, or live-state
DB invocation is part of the architecture pass or coder inner loop.

## Follow-ups to file

- Full chrome inventory and extraction: sidebar, panel sections, overlays,
  masthead, notifications, status modals, splash, and all remaining draw sites.
- Pure `thegn-core` time/date/number helpers: relative ages and durations with
  plural categories, calendar month/weekday names, and locale-aware `%a/%A/%b/%B`
  expansion while preserving user format structure. Keep numeric formatting
  deliberately small; revisit ICU only with a measured need.
- RTL safety: neutralize bidi controls at the chrome compose edge, decide
  whether to preserve Fluent isolation marks or make the width pass understand
  them, test `{`-laden user data, and withhold RTL locale shipping until a
  terminal bidi/rendering story exists.
- Per-locale help page trees with per-page canonical fallback. Keep
  `docs/help/*.md` as the ratchet corpus, and keep keybindings/config-reference
  generated; never hand-edit generated pages.
- Catalog-back human CLI prose while keeping `--json`, exit codes, doctor probe
  tokens, and other machine contracts byte-stable. Add CLI locale matrix tests.
- Translation workflow in `CONTRIBUTING.md`; hosted Crowdin/Weblate only when
  there is a translator community.
- A locale-specific Muse smoke after a real second locale has enough surface to
  justify a baseline; `THEGN_E2E=1` remains pinned to `en-US`.
