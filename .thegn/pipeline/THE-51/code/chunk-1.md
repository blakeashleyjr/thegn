# Chunk 1 — embedded localization substrate

## Objective

Harden the existing Fluent loader into a deterministic, testable substrate. The
chunk must not translate or sweep application surfaces. It establishes the
single catalog, pure locale precedence, strict shipped-locale parity, the e2e
locale pin, and the shrink-only raw-literal ratchet
that chunk 2 will burn down.

## Files touched (exact paths)

- `crates/thegn-core/src/i18n.rs`
- `crates/thegn-core/src/i18n_locale.rs` (new sibling module)
- `crates/thegn-core/src/i18n_format.rs` (new sibling module)
- `crates/thegn-core/src/i18n_parity.rs` (new sibling module)
- `crates/thegn-core/src/i18n_pseudo.rs` (new sibling module)
- `crates/thegn-core/src/lib.rs`
- `config/config.toml.example`
- `crates/thegn-host/src/e2e_freeze.rs`
- `crates/thegn-host/src/run.rs` (one startup call-site change)
- `test/i18n-literal-ratchet.txt` (new)
- `justfile`

No new config key or `THEGN_UI_LANGUAGE` overlay is added. Update the existing
`[ui].language` comment in `config/config.toml.example` to document explicit
config → `LC_ALL` → `LANG` → `en-US`, startup/restart selection, and the
TUI-only scope. Do not touch `test/completion-slot-ratchet.txt`,
`test/control-schema` snapshots, or the existing help ratchet allowlists unless
the implementation genuinely changes the corresponding contract; an
unchanged ratchet is the correct result here.

## Approach

1. Keep `i18n.rs` as a thin public facade and preserve the existing embedded
   Fluent catalog and `t!` API. Move pure concerns into sibling modules so the
   file does not become a god module.
2. Implement and unit-test a resolver with this exact behavior:
   - valid explicit `[ui].language` wins;
   - `auto` uses non-empty `LC_ALL`, then non-empty `LANG`;
   - absent, empty, or invalid input falls back to `en-US` and degrades with a
     diagnostic rather than blocking startup;
   - an explicit e2e freeze wins over every other input;
   - pseudolocale selection is dev-only and loses to the e2e freeze.
     The core function receives strings/options; it must not read process env.
3. Make the shipped-locale source table explicit with `include_str!` and use a
   pure key fold against the `en-US` schema. Fail with locale + key for both an
   orphan and a missing default key. The production lookup may still fall back
   defensively for unknown locale/corrupt data, but shipped files cannot rely on
   fallback.
4. Add `ja-JP` to the shipped-locale registry only in chunk 2; chunk 1 tests
   must be written so adding that source automatically participates in parity.
5. Add the e2e freeze pin before the existing `i18n::init` call in
   `crates/thegn-host/src/run.rs`. Do not add a doctor/CLI string in this
   substrate chunk; CLI user-visible text belongs to the later catalog-bound
   CLI follow-up and machine output remains unchanged.
6. Transform only message values in the pseudolocale, preserve Fluent argument
   placeholders, and keep it outside the selectable locale list. Prove
   non-ASCII output and cell-width expansion with pure tests.
7. Add `i18n_format.rs` with pure `plural_category`, `format_integer`, and
   short `format_date` helpers. Use the helper's plural category as the
   selector input for chunk 2's single catalog lookup; cover English,
   Japanese, invalid locale fallback, negative/large integers, and date
   determinism without changing existing application date call sites.
8. Add the file-level raw user-facing literal ratchet to `just lint`, seeded
   from the pre-change audited hit set. It is shrink-only and must not classify
   catalog keys, comments, or user data as translatable prose.

## Dependencies / overlap

Chunk 1 is the prerequisite for chunk 2 and must run first. It owns the
substrate and the startup call site. Chunk 2 may subsequently touch
`crates/thegn-host/src/run.rs` for statusbar mode-label routing; that is the
declared overlap and must be serialized, not parallelized.

No overlap with completion, control-schema, database, provider, pane, or
generated-help implementation. The `thegn-core` code must remain substrate-free
and must not add tokio, termwiz, portable-pty, HTTP, forge SDK, ICU, or runtime
file I/O.

## Tests to run

Scoped only:

- `just quick thegn-core`
- `cargo nextest run -p thegn-core i18n`
- `cargo nextest run -p thegn-core env_overlay`
- `cargo nextest run -p thegn-core i18n_format`
- `just quick thegn-host`
- `cargo nextest run -p thegn-host e2e_freeze`
- `cargo nextest run -p thegn-host help` (regression check for unchanged
  canonical/generated help behavior)
- `bash test/ratchet.sh i18n-literal 'jump|type to filter|matches|dismiss|offline|VIM NORMAL|VIM INSERT|NORMAL|EMACS|LOC|New terminal|New folder|Move to folder' crates/thegn-host/src/chrome.rs crates/thegn-host/src/statusbar_left.rs crates/thegn-host/src/statusbar_badges.rs crates/thegn-host/src/run.rs crates/thegn-host/src/handlers/status_line.rs crates/thegn-host/src/palette.rs crates/thegn-host/src/keymap_specs.rs`

The coder must not run `just test`, `just ci`, `just coverage`, `just build`,
e2e, or a >10-minute build for this chunk. Any invocation of `thegn` from this
worktree must set `XDG_STATE_HOME` to a fresh temporary directory; this chunk
does not need a binary invocation.

## Done criteria

- The resolver unit tests prove explicit config, `LC_ALL`, `LANG`, default,
  invalid/empty degradation, e2e freeze precedence, and once-only startup.
- A parity test names every orphan/missing key and passes for every source in
  the shipped-locale table after chunk 2 adds `ja-JP`.
- `THEGN_E2E=1` forces `en-US` before initialization regardless of config or
  host locale; pseudolocale is inert under the freeze.
- `thegn-core` remains substrate-free and the new helpers are pure/unit-tested.
- The existing `[ui].language` documentation records the locale precedence;
  no env-overlay ratchet entry is invented because no config field was added.
- The new raw-literal ratchet is present, shrink-only, and wired to `just lint`;
  no unrelated env-overlay/completion/control/help ratchet is changed.
- The coder commits exactly with subject:
  `feat(i18n): harden embedded locale substrate`
