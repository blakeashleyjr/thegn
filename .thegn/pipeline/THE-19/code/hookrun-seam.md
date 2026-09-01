# THE-19 — authorized hook_run.rs platform-seam move (Lead work order)

files:

- crates/thegn-host/src/hook_run.rs
- crates/thegn-host/src/platform/mod.rs

## Authorization

Row 322 delivered the union-validator fix and then correctly escalated instead
of silencing a NEW blocker. Its diagnosis is accepted and the fix is authorized:

```
platform-cfg host ratchet violation in hook_run.rs
```

`test/platform-cfg-host-ratchet.txt` is shrink-only, so this must be **fixed,
not pinned**.

## Done criteria

- `cargo nextest run -p thegn-host -E 'test(platform_cfgs_live_in_platform_modules)'`
  passes with `hook_run.rs` absent from the violations.
- Move the platform-conditional code behind a named function in
  `crates/thegn-host/src/platform/mod.rs`. The `#[cfg]` lives inside that
  function; `hook_run.rs` calls it with no `#[cfg]` of its own — the same shape
  as the existing `write_state_file` helper there and the same fix row 318
  applied to `theme_store.rs` on the THE-7 branch. Read that commit
  (`cbea76dc`) first and follow it, so the two look alike.
- **Preserve behaviour exactly.** This is the bounded pipe-drain / process-group
  kill path that rows 303 and 310 hardened; the security and timeout properties
  must not change. If the seam forces any behavioural change, say so explicitly
  in your artifact rather than making it silently.
- Then run the full gate: `THEGN_ALLOW_HEAVY=1 just test`. Row 322 got to
  5,699/5,700 passing before this ratchet cancelled the remaining 1,444 tests —
  so this is expected to be the last blocker, but confirm the whole suite is
  green, not just the ratchet.
- If the gate dies before any test runs (sccache/RUSTC_WRAPPER), retry once with
  `RUSTC_WRAPPER=` unset and report BLOCKED with the exact error, not FAIL.
