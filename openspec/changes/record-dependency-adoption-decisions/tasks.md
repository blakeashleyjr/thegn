# Tasks

Only the Windows bindings alignment merited code work; every other verdict is
keep-as-is or reject (see design.md). Iterate with scoped checks
(`just quick thegn-host`, `cargo check -p thegn-media --target
x86_64-pc-windows-gnu` if the target is installed); the full gates run once
at the end.

## 1. Windows bindings version alignment

- [ ] 1.1 Bump the workspace `windows-sys` pin 0.59 → 0.61 in `Cargo.toml`,
      keeping the same feature list (Win32_Foundation, Win32_System_Console,
      Win32_System_Threading, Win32_System_JobObjects) and the
      target-gated-to-`cfg(windows)` comment. Fix any declaration renames in
      `crates/thegn-host/src/platform/windows.rs`. Coordinate with
      add-windows-native-compile if its tasks have started — land this first
      or rebase it in, so new Windows code is written against 0.61.
- [ ] 1.2 Bump `windows` 0.58 → 0.62 in the workspace manifest and migrate
      `thegn-media`'s SMTC code across the windows-rs API churn. If the
      add-windows-parity wave is already touching `thegn-media`, fold this
      task into that wave instead (design.md open question 2).
- [ ] 1.3 Verify the dedupe: `cargo tree --target x86_64-pc-windows-msvc -i
windows-sys` / `-i windows` show our direct pins resolving with the
      transitive cohorts (0.61.x / 0.62.x); confirm the legacy 0.45/0.48/0.52
      splits are unchanged (they are upstream pins, out of scope).
- [ ] 1.4 Update deny.toml's `[bans]` comment: drop `windows` from the named
      known-splits list, leaving syn 1+2 and clap 2+4 as the remaining
      blockers for the warn → deny ratchet.
- [ ] 1.5 Confirm the check-cross windows-gnu lane still passes in its
      current shape (leaves-only check without a mingw cc; full check with
      one) — windows-sys ≥ 0.60 links via `windows-link`/raw-dylib, which
      should need no import libraries. This runs inside the final `just ci`,
      not per-edit.

## 2. Spec

- [ ] 2.1 The `architecture-gates` delta in this change is the spec
      deliverable; confirm its described behaviours (deny.toml's advisory
      exception format, license allowlist, wildcard/source denies,
      multiple-versions warn with named splits, cargo-machete) still match
      `deny.toml` and the `deps-audit` recipe after task 1.4's comment edit.

## 3. Validation

- [ ] 3.1 Run `just ci` once, when the change is complete (includes
      `deps-audit`, `check-cross`, `check-msrv`, and
      `openspec validate --all --strict`).
