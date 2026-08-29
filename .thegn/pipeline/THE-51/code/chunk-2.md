# Chunk 2 — statusbar and palette proof locale

## Objective

Route one bounded, user-visible surface through the shared catalog and prove a
real locale without starting a whole-chrome translation sweep. The bounded
surface is the statusbar plus command palette, including their static labels,
mode labels, action labels, and pluralized palette match count. User data and
glyphs remain untouched.

## Files touched (exact paths)

- `crates/thegn-core/locales/en-US/main.ftl`
- `crates/thegn-core/locales/ja-JP/main.ftl` (new proof locale)
- `crates/thegn-host/src/i18n_surface.rs` (new typed UI adapter)
- `crates/thegn-host/src/main.rs` (declare the new module)
- `crates/thegn-host/src/statusbar_left.rs`
- `crates/thegn-host/src/statusbar_badges.rs`
- `crates/thegn-host/src/chrome.rs`
- `crates/thegn-host/src/run.rs` (declared serial overlap with chunk 1 for
  mode-label composition)
- `crates/thegn-host/src/handlers/status_line.rs`
- `crates/thegn-host/src/palette.rs`
- `crates/thegn-host/src/keymap_specs.rs`
- `test/i18n-literal-ratchet.txt` (remove only literals actually routed here)

No `docs/help/*.md`, `crates/thegn-host/src/help/pages.rs`,
`crates/thegn-host/src/help/gen_pages.rs`, `config/config.toml.example`,
`test/help-*-ratchet.txt`, completion-slot files, control-schema snapshots, or
database files are in scope.

## Approach

1. Add the complete current `en-US` key set for this surface and a complete
   matching `ja-JP` key set. Translate a small representative subset (palette
   title/filter/footer/matches and at least one action/status label); keep
   canonical English values for the rest of the required keys. This is a proof
   locale, not a claim that Japanese coverage is complete for the product.
2. Use `i18n_surface.rs` as the only host adapter. It maps typed surface
   concepts/action ids to catalog keys and calls the core catalog exactly once
   per message. It must not contain a second fallback catalog. Add a stable
   message-key field to `ActionSpec` while retaining its canonical English
   `label` for the generated keybindings page; the generator does not consume
   the new field. Unknown action ids degrade to the canonical existing label
   with a diagnostic only if the data path can prove the key is not registered;
   normal shipped action ids are parity-tested.
3. Replace palette draw-site prose (`jump`, `menu`, filter placeholder, match
   count, `move`, `run`, `dismiss`, and new-terminal/folder static labels) with
   adapter calls. Select the `.one`/`.other` match key with chunk 1's pure
   `plural_category` helper, then perform one catalog lookup. The numeric range
   `1-10/total` remains numeric data, and query, workspace, folder, and
   custom-action names remain literal user data.
4. Replace statusbar static words and full/compact mode labels with adapter
   calls. Preserve the existing attention/daemon/CI/MQ/PR glyphs and
   abbreviations where they are capability/status vocabulary, and preserve
   `caps::active_glyphs()` as the only glyph fallback seam. Do not translate
   dynamic plugin labels or status messages supplied by other subsystems in this
   bounded chunk.
5. Keep all layout math intact. Measure translated strings with the existing
   `unicode-width`/segment helpers; never use byte length. Add tests that render
   the longer `ja-JP`/pseudolocale values through the statusbar and palette
   fit paths and assert no line exceeds its cell budget and no atomic keyhint or
   palette row is cut in half.
6. Remove only the addressed raw literals from the i18n literal ratchet. If a
   literal is intentionally stable technical data, leave it pinned with a
   reason; do not hide newly introduced literals by expanding the allowlist.

Generated keybindings/config-reference pages remain generated. The new action
message-key metadata is deliberately ignored by the keybindings generator, so
its canonical output and help ratchets stay unchanged; do not create a
translated hand-written page or alter canonical help ratchets in this chunk.

## Dependencies / overlap

Run serially after chunk 1 because it consumes the resolver, parity registry,
and adapter contract. It overlaps chunk 1 only at
`crates/thegn-host/src/run.rs`; the Lead must not parallelize these chunks.
Within this chunk, all other files are owned by this chunk and no second coder
may touch them concurrently.

There is no dependency on the date/time helper, help locale trees, RTL policy,
CLI localization, or any capability catalog change. Those are follow-ups.

## Tests to run

Scoped only:

- `just quick thegn-core`
- `cargo nextest run -p thegn-core i18n`
- `just quick thegn-host`
- `cargo nextest run -p thegn-host palette`
- `cargo nextest run -p thegn-host statusbar`
- `cargo nextest run -p thegn-host chrome`
- `cargo nextest run -p thegn-host help` (generated-page and canonical-ratchet
  regression check)
- `bash test/ratchet.sh i18n-literal 'jump|type to filter|matches|dismiss|offline|VIM NORMAL|VIM INSERT|NORMAL|EMACS|LOC|New terminal|New folder|Move to folder' crates/thegn-host/src/chrome.rs crates/thegn-host/src/statusbar_left.rs crates/thegn-host/src/statusbar_badges.rs crates/thegn-host/src/run.rs crates/thegn-host/src/handlers/status_line.rs crates/thegn-host/src/palette.rs crates/thegn-host/src/keymap_specs.rs`

Do not run `just test`, `just ci`, `just coverage`, e2e, or a full-workspace
compile during iteration. If a manual binary check is needed, use an isolated
state directory, for example:

```sh
XDG_STATE_HOME="$(mktemp -d)" THEGN_E2E=1 thegn --help
```

## Done criteria

- `ja-JP` is embedded, is listed in the one shipped-locale source table, and
  passes strict `en-US` key parity.
- Statusbar and palette user-visible static strings in the declared files use
  the shared adapter/catalog; no new ad-hoc draw-site format string is added.
- At least one translated action/status label and the palette plural/count path
  are observed in unit tests; unknown locale still degrades to `en-US`.
- User data (queries, branch/workspace/folder/plugin labels) is not translated
  or parsed as Fluent, and capability glyphs still flow through `caps`.
- Cell-width tests prove translated/pseudolocalized strings fit existing
  statusbar/palette budgets and preserve atomic row/keyhint behavior.
- The i18n literal ratchet shrinks only for the routed surface. Env-overlay,
  completion-slot, control-schema, and help ratchets remain unchanged because
  this chunk adds no config/command/capability/help contract; any unexpected
  change fails review.
- The coder commits exactly with subject:
  `feat(i18n): localize statusbar and palette proof surface`
