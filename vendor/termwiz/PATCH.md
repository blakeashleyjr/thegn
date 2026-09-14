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

Upstream source, manifests, examples, benches, README, changelog and license are
otherwise unchanged. Cargo registry bookkeeping, the package's independent
lockfile, original unnormalized manifest and release tooling are omitted.

Remove the patch when an upstream release provides non-panicking destructor
cleanup and passes the hangup/unwind regression tests. Do not replace it with
`catch_unwind` or intentionally leaked terminal handles in application code.
