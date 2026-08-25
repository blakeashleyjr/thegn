# Design — localization surfaces beyond the core mechanism

## Reconciliation: what add-localization covers vs. what this change adds

| Surface                                         | add-localization                                  | This change                            |
| ----------------------------------------------- | ------------------------------------------------- | -------------------------------------- |
| String extraction (`t!`, Fluent, embedded)      | Owns it (mechanism landed; sweep is its task 2.1) | —                                      |
| Locale selection (`[ui] language` + sys-locale) | Owns it (landed)                                  | —                                      |
| Plurals (Fluent syntax)                         | Owns it                                           | Applies it to the time layer           |
| Cell-width layout safety                        | Owns the invariant                                | Makes it testable (pseudolocale)       |
| RTL / bidi                                      | Silent                                            | Explicit policy + user-data safety     |
| Dates / relative times / clock                  | Silent                                            | One core time layer through Fluent     |
| Help corpus (30 pages + 2 generated)            | Silent                                            | Per-locale resolution + ratchet policy |
| CLI vs chrome boundary                          | "chrome" implied                                  | Spec'd machine-output requirement      |
| e2e determinism per locale                      | Silent                                            | `THEGN_E2E` locale pin                 |
| Translation maintenance                         | Silent                                            | Parity gate + in-repo workflow         |

Landed reality check (2026-08-25): `crates/thegn-core/src/i18n.rs` exists with
`static_loader!` (fallback `en-US`), `t!` with args, and a `OnceCell` active
language; `locales/en-US/main.ftl` holds two demo keys; `i18n::init` runs in
`run.rs`; **zero** production `t!` call sites exist. So everything below can be
built now without waiting for the sweep — with only en-US embedded, routing a
literal through the new helpers renders identically.

## The CLI boundary (why English stays)

The `cli` spec already pins machine-readable output to one emitter
(`cmd::emit_json`), an exit-code contract, and no-ANSI JSON. Localizing any of
that would break every script that greps thegn's output — the same reason git
localizes porcelain but keeps plumbing stable. Decision: **machine-consumed
output is locale-independent by requirement** (JSON keys/values thegn
synthesizes, exit codes, `doctor` probe words like `present`/`absent`,
`keys list`), and **human CLI prose stays English in this phase** as deliberate
policy, not omission. The chrome (TUI) is the localization surface. Revisit
CLI prose post-alpha if translator supply materializes.

## RTL: an honest position

Terminal emulators render cells in logical order; bidi-aware terminals
(ECMA TR/53-style) are rare, and thegn composes its own chrome into that grid.
Shipping an `ar`/`he` locale would produce reversed-looking chrome nearly
everywhere. Decision:

- **No RTL locale is embedded** until a bidi story exists; `[ui] language =
"ar"` resolves normally and falls back per key to en-US (fluent-templates'
  fallback), which is correct behaviour, not an error.
- **RTL user data must be safe today.** Branch/file/host names containing
  bidi control characters (U+202A–U+202E overrides, U+2066–U+2069 isolates,
  RLM/LRM) can visually reorder adjacent chrome — the Trojan-source spoofing
  class. They are neutralized (stripped or replaced) at the chrome compose
  edge for thegn-drawn text. Pane _content_ is the guest program's business
  and is not touched.
- **Open question — isolate stripping in `t!`:** the landed macro strips
  FSI/PDI from Fluent output "to keep the TUI layout clean". With bidi user
  data interpolated into a translated string, those isolates are exactly what
  prevents reordering. Since chrome-edge neutralization (above) removes bidi
  controls from the interpolated values themselves, stripping the isolates
  stays safe — but the two must land as a pair. The unit test locks that:
  interpolating an RTL-override-laden branch name yields output with no bidi
  control characters.

## The time/date layer (`thegn-core`)

Relative ages are the most-repeated localizable pattern in the codebase.
Verified inventory (2026-08-25): the core has ONE shared helper —
`thegn_core::util::age` (`2h`, `3d`; no " ago", no plurals) — used by host
sites like `detail.rs`, plus private English duplicates beside it:
`panel/sections/mod.rs::fmt_secs` (callers in `panel/sections/{ci,hosts,git,notifications}.rs`),
`detail/status_modal.rs::fmt_uptime`, and on the CLI side `cmd/host.rs::age`,
`cmd/kaneo.rs::age_secs`, `cmd/doctor.rs`'s `probed {}s ago`. The " ago"
suffix is a per-call-site `format!` in every case, which is exactly the
untranslatable shape (suffix order differs across languages). Month/weekday
names are English literals in `calendar/grid.rs` (the `["Mon", …]` weekday
row) and `detail/calendar/render.rs` (`"January"`, …), and the masthead date
widget (`chrome.rs::wall_clock` + `[bars] date_format`, default `%a %b %-d`)
renders chrono's English names. Decision: one pure module (e.g.
`thegn_core::i18n_time`) that renders

- relative age (`3m ago`) and duration (`2h 10m`) through Fluent keys with
  CLDR plural categories (Fluent handles `one`/`few`/`many` natively — this is
  where "plurals" becomes real, not just supported syntax), absorbing
  `util::age` and the duplicate helpers,
- month/weekday name tables via locale keys, feeding the calendar grid and
  detail views (coordinate with `add-calendar-and-world-clock`, which renders
  date grids) **and** the name-producing strftime tokens (`%a`/`%A`/`%b`/`%B`)
  of the `[bars]` date/clock widgets — the user's configured format string
  keeps its structure; name tokens are pre-expanded from the locale tables
  before chrono formats the numerics (the default clock `%H:%M` is numeric
  and locale-neutral),

and the scattered sites route through it. One deliberate exception:
`axis.rs::age_unit`'s single-char unit suffixes (`s/m/h/d`) on gitviz axis
labels stay untranslated — a width-fixed axis budget where one cell per unit
is the design. Chrome sites localize; CLI sites (`cmd/host.rs`, `cmd/kaneo.rs`,
`doctor`) keep English per the boundary above but may share the same helper
pinned to en-US. Full ICU (icu4x datetime/decimal) is deliberately
NOT pulled in: heavyweight for a TUI that shows short ages and a clock, and
Fluent's plural rules cover the hard part. Alternatives considered: `icu4x`
(overkill now, clean upgrade path later), `chrono`'s `unstable-locales`
(pulls pure-data locale tables into every build for little gain).

## Help corpus

- **Canonical corpus is English** and remains the single ratchet target: all
  three ratchets (`help-ratchet`, `help-prose-ratchet`,
  `help-context-ratchet`) and the registered/orphan-page tests evaluate only
  the canonical `docs/help/*.md` set. Running prose ratchets against
  translations would demand every locale mention every chord — an impossible
  gate that would freeze translation at zero.
- **Per-locale trees, per-page fallback**: `docs/help/<locale>/<page>.md`,
  embedded at build like the canonical set; F1 lookup resolves the active
  locale's page, else canonical. A partially translated locale is normal and
  invisible except that untranslated pages render English.
- **Generated pages**: the keybindings page is built from the keymap fold —
  its headings/labels localize through `t!` as part of the ordinary string
  sweep (action labels live in `ACTION_SPECS`, which the sweep owns). The
  config-reference page is the example config's own comments — reference
  material keyed to English config keys — and stays English **by design**.

## e2e determinism + pseudolocale

- `THEGN_E2E=1` adds a locale pin next to the existing clock/stats/version
  pins (`e2e_freeze`): active locale forced to `en-US` before `i18n::init`
  resolves, so baselines can never flap with the host `LANG` or a user
  config. Cheapest possible insurance, needed _before_ the first real
  translation lands.
- **Pseudolocale**: a pure generator derives a pseudo-translation from en-US
  (accented substitution + ~1.4x width expansion, e.g. `Ẇörkŝpàçé…`), giving
  unit tests a "longer, non-ASCII translation" to prove the truncation
  invariant today, with zero translator dependency. Exposed to developers via
  `THEGN_PSEUDOLOCALE=1` (env, dev-only, never config, never listed as a
  language); ignored when `THEGN_E2E=1` so baselines stay safe. This is the
  standard trick (Android `en-XA`) and is the only way the layout requirement
  in `add-localization` is testable before real locales exist.

## Translation infrastructure (the honest recommendation)

For a solo-maintainer alpha: **in-repo `.ftl` files, contributed by PR,
guarded mechanically**:

- en-US is the key schema. A unit test (pure, in `thegn-core`, inside the 95%
  gate) fails on **orphan keys** (a locale key en-US lacks — always a bug) and
  _reports_ missing keys (per-key fallback is the designed steady state, not
  an error).
- `thegn doctor` prints the resolved locale and per-locale key coverage — the
  human-visible version of the same fold.
- Crowdin (what the Termix reference uses) or Weblate is the right answer
  only once there are external translators; both ingest Fluent, so nothing
  here forecloses it. Standing one up now means maintaining sync automation
  for zero contributors — recommended deferral (P3) with the format chosen so
  adoption later is a workflow change, not a migration.

## Security

- **No new attack surface from locale data**: translations remain compiled
  into the binary (no runtime locale-pack loading or parsing of untrusted
  files); the deferred "filesystem language packs" stay deferred.
- **Bidi spoofing is the real one**: user-controlled strings (branch names,
  worktree names, remote hosts) carrying bidi override characters can make
  chrome text read differently than it is — e.g. disguising a destructive
  target name. The compose-edge neutralization requirement exists for this,
  and its unit tests are the gate. Pane content is untouched (the guest owns
  its bytes; sanitizing them would corrupt legitimate programs).
- **Fluent args are data, not code**: `t!` interpolation cannot inject
  markup/format directives; a test locks that a `{`-laden branch name renders
  literally.
- **Env hooks**: `THEGN_PSEUDOLOCALE` only swaps which embedded bundle
  serves keys — it cannot load external content; `THEGN_E2E` already implies a
  driven test instance. Neither widens any write surface; no credential,
  sandbox, or scope/permission changes anywhere in this change.

## Open questions

1. Isolate-mark policy in `t!` (strip + neutralize-at-edge, per above) — keep,
   or preserve isolates and teach the width pass to skip them? Default: keep
   stripping, locked by the pair of tests.
2. Should the pseudolocale expand width by a fixed factor or per-key worst
   case? Default: fixed ~1.4x + bracket markers (`⟦…⟧`) so unexpanded literals
   are visually obvious in a dev run.
3. When the first real second locale lands: add one muse smoke case driven
   with `[ui] language` set (launch, assert no panic/overflow), or rely on
   pseudolocale units alone? Default: decide with that locale's PR — not now.
