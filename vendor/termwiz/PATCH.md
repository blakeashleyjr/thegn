# Local termwiz 0.23.3 patch (THE-618)

Source: crates.io termwiz 0.23.3, upstream repository
https://github.com/wezterm/wezterm, copied from the existing Cargo registry cache.
Published package SHA-256:
`4676b37242ccbd1aabf56edb093a4827dc49086c0ffd764a5705899e0f35f8f7`.
The upstream MIT license is retained in `LICENSE.md`.

Only `src/terminal/unix.rs` and `src/terminal/windows.rs` production code differs:
destructor cleanup ignores individual I/O/mode errors and continues attempting
all restoration and signal cleanup. `src/terminal/unix_drop_tests.rs` adds real
PTY tests; the Unix module includes it only in test builds. This fixes the
observed Unix `write.flush().unwrap()` EIO panic and the equivalent unchecked
Windows destructor operations. Windows runtime behavior requires Windows CI;
Linux PTY evidence is not a Windows compatibility claim.

`Cargo.toml` additionally carries a `[lints.rust]` table allowing
`mismatched_lifetime_syntaxes`. Cargo applies `--cap-lints allow` to registry
and git dependencies but not to path dependencies, so vendoring this crate at
`vendor/termwiz` makes rustc lint upstream's sources as if they were
first-party. That lint became warn-by-default after 0.23.3 was published and
fires 13 times, all on `Foo` versus `Foo<'_>` in return position — no behavior
is involved. The allow is at the manifest so every `.rs` file stays
byte-identical to the published package; applying the compiler's `cargo fix`
suggestion would edit 13 source sites instead. Drop the table if a later
upstream release fixes the signatures.

THE-2753b: `src/input.rs` gains an opt-in `set_hold_partial_csi`/`has_pending_csi`/
`flush_pending` trio and `src/terminal/unix.rs` enables it with a 40 ms flush. A short
read that ended inside an SGR mouse report (`ESC [ <65;..`) used to be flushed with
`maybe_more = false`, decoding `ESC [` as Alt+`[` (the previous-app chord). Default
parser behaviour is unchanged; a lone ESC still resolves immediately. An expired hold
polls the tty without blocking before flushing, so a late `poll_input` under load
completes the report instead of emitting Alt+`[`.

Upstream source, examples, benches, README, changelog and license are otherwise
unchanged, and the manifest differs only by that table. Cargo registry
bookkeeping, the package's independent lockfile, original unnormalized manifest
and release tooling are omitted.

## Ownership decision (THE-681)

**Decision:** keep termwiz vendored. thegn owns this copy deliberately.

Upstream evidence (checked 2026-11-16): the wezterm git repository has termwiz
commits through 2026-09-12 (ClearLine, winapi to windows-sys, and a
`mismatched_lifetime_syntaxes` fix on 2026-09-06), while crates.io is still at
0.23.3 (2025-03-20). Upstream is active but unpublished.

Exit/upgrade condition (replaces "remove when upstream fixes"):

- Re-evaluate at the first crates.io termwiz release after 0.23.3.
- Then diff upstream's destructor cleanup against this patch, and drop the
  `[lints.rust]` table if the lint fix is included.
- If a termwiz bug needs a fix beyond the destructor patch before then, fork
  properly: add a changelog section to this file listing each patch with its
  THE issue and test.

Do not replace the patch with `catch_unwind` or intentionally leaked terminal
handles in application code.

Alternatives considered:

- **alacritty_terminal** is a different layer (an emulator grid, versus
  termwiz's outer-terminal Surface/input), so it is not a drop-in replacement
  for termwiz's roles.
- **portable-pty** is explicitly deferred. Its surface is about 8 files
  (`pane_pty.rs`, `pane.rs`, `daemon/session.rs`, `platform/*`) and it has no
  local patch. Re-evaluate it together with termwiz.

## Changelog

- THE-2689: `src/input.rs` bracketed-paste end handling advanced the read buffer by
  the length of the lossily decoded string rather than the raw byte count. A paste
  containing invalid UTF-8 (each bad byte becomes a 3-byte U+FFFD) consumed bytes
  past the end marker, dropping or corrupting following input (and could underflow
  the buffer length). It now advances by the raw index. Tests:
  `bracketed_paste_with_invalid_utf8_keeps_following_input`,
  `bracketed_paste_markers_split_across_reads`.
