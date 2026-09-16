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

Upstream source, examples, benches, README, changelog and license are otherwise
unchanged, and the manifest differs only by that table. Cargo registry
bookkeeping, the package's independent lockfile, original unnormalized manifest
and release tooling are omitted.

Remove the patch when an upstream release provides non-panicking destructor
cleanup and passes the hangup/unwind regression tests. Do not replace it with
`catch_unwind` or intentionally leaked terminal handles in application code.
