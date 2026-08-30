# THE-7 — fold-gate fix (Lead work order)

files:
  - crates/thegn-host/src/theme_store.rs
  - crates/thegn-host/src/platform/mod.rs

## Why this exists

`thegn land` refused this branch: **the fold gate (`just test`) is red.**
Row 313 reported PASS from scoped suites, which do not run the architecture
ratchets. Reproduced on the folded tree:

```
ratchet test/platform-cfg-host-ratchet.txt: new violation in ["theme_store.rs"]
Platform-conditional code belongs in src/platform/ (CLAUDE.md: keep the seam
thin, call sites platform-free). Move the `#[cfg]` arm behind a `platform::`
function.
```

The no-follow / exclusive-open hardening added in row 307 introduced a
`#[cfg(unix)]` / `#[cfg(not(unix))]` arm directly in `theme_store.rs`.

## Done criteria

- `cargo nextest run -p thegn-host -E 'test(platform_cfgs_live_in_platform_modules)'`
  passes.
- **Fix it, do not pin it.** `test/platform-cfg-host-ratchet.txt` is shrink-only
  and this is a brand-new violation, not inherited debt.
- Move the platform-conditional code behind a named function in
  `crates/thegn-host/src/platform/mod.rs`, following how the existing
  `write_state_file` / `test_symlink_supported` helpers there are shaped: the
  `#[cfg]` lives inside the platform function, and `theme_store.rs` calls it
  with no `#[cfg]` of its own.
- Preserve the security properties row 307 added exactly — exclusive create,
  no symlink following, bounded reads, permission preservation. This is a
  code-motion change, not a rewrite; if a behaviour has to change to fit the
  seam, say so explicitly in your completion artifact.
- Keep the non-unix arm's behaviour equivalent to what it is today.
- Then run the full gate yourself: `THEGN_ALLOW_HEAVY=1 just test`, and report
  its result. Do not report PASS from scoped runs alone.
