# Chunk 1 — core user-theme and Gogh contracts

## Files touched

- `crates/thegn-core/src/theme_user.rs` (new)
- `crates/thegn-core/src/theme_import.rs` (new)
- `crates/thegn-core/src/theme_resolve.rs` (new)
- `crates/thegn-core/src/lib.rs`
- `crates/thegn-core/src/config.rs` (thin delegation only)

Do not touch host code, CLI code, docs, config examples, ratchet files, or
control snapshots in this chunk.

## Approach

Add a closed/versioned `UserTheme` TOML model containing editable palette base
roles, accent/focus, and semantic hues. Keep the config schema unchanged.
Move shared “apply config overrides to a base palette, then extend” behavior to
the new `theme_resolve` module and have `Config::palette_with_preset` delegate
to it; preserve current defaultish accent/focus semantics and invalid-hex
behavior.

Add pure `theme_import` parsing for bounded Gogh YAML/JSON bytes and a
deterministic `Ansi16` mapper. Accept `background`, `foreground`, `cursor`,
`color_01..color_16`, optional `name`/`variant`; map fg→text, bg→bg0,
cursor→focus, neutral ANSI values to the ramp, six normal/bright pairs to
hues, and derive orange/magenta as documented in the architect design. Use no
filesystem, network, terminal, or host dependency. Expose serialization and
conversion errors as typed values suitable for the host provider.

Unit-test every role mapping, all 16 ANSI inputs, light variant, malformed
hex/missing fields/oversize input, user-theme round-trip, derived extension,
and contrast audit compatibility. Preserve `Palette` equality and built-in
`PRESETS` behavior.

## Overlap/dependency

This is the first chunk. It has no dependency on the other chunks. Chunk 2 is
serial after this chunk and consumes the public `UserTheme`, importer, and
resolver APIs. Chunk 3 is serial after chunk 2 for host-store CLI reuse. No
other chunk may edit the five paths above.

## Tests to run

- `just quick thegn-core`
- `cargo nextest run -p thegn-core theme_import`
- `cargo nextest run -p thegn-core theme_user`
- `cargo nextest run -p thegn-core palette`

Do not run `just test`, `just ci`, a full-workspace build, or e2e. The tests
must remain substrate-free and must not invoke `thegn`; if a manual invocation
is added while debugging, set `XDG_STATE_HOME` to a fresh temp directory.

## Done criteria

- Core compiles and the scoped tests pass.
- `Config::palette()` and existing preset/config tests are behavior-preserving.
- Gogh YAML/JSON conversion is pure, bounded, unit-tested, and maps
  16 ANSI + foreground/background/cursor into existing thegn roles without a
  new cursor slot or config key.
- User-theme TOML is closed/versioned and derives extension tokens through the
  existing seam.
- No host I/O, network, color literals, new config key, capability row, or
  ratchet allowlist entry is introduced.
- Commit with exactly: `feat(the-7): add core theme and Gogh import contracts`
