## 1. Core: Cargo source in the resolver

- [x] 1.1 Add `Source::Cargo { crate_name: String }` to `managed_tool.rs`; update `asset_for`/`repo` to return `None` for it.
- [x] 1.2 Add `ManagedTool::cargo(name, crate_name, bin_name, version)` (default dir `~/.thegn/tools/<name>`, `bin_rel = bin/<bin_name>`, marker `.version`).
- [x] 1.3 Unit-test: a Cargo tool resolves to `<managed_dir>/bin/<name>` and has no release asset.

## 2. Core: debugger module

- [x] 2.1 Add `crates/thegn-core/src/debug.rs` and export from `lib.rs`.
- [x] 2.2 `BS_PIN` const + `bs_tool()` (`Cargo` "bugstalker", bin "bs", `Once`, PATH fallback `["bs"]`).
- [x] 2.3 Pure `bs_supported(os, arch) -> bool` (Linux && x86-64) and `platform_supported()` (over `Os/Arch::current()`).
- [x] 2.4 Pure `launch_argv(bin, program, args)` and `attach_argv(bin, pid)` (verify `bs` attach flag against its docs).

## 3. Core: unit tests (95% gate)

- [x] 3.1 `bs_supported` matrix (Linux/x64 true; others false); `bs_tool` shape (cargo source, fallback, pin).
- [x] 3.2 `launch_argv`/`attach_argv` exact argv (program + args; pid form).

## 4. Host: Cargo install arm + generic install

- [x] 4.1 Extend `thegn-host::managed_tool::acquire` with the `Cargo` arm: `cargo install <crate> --version <v> --root <managed_dir>` via `run_setup_cmd` (ensure `cargo` present with a helpful message).
- [x] 4.2 Add the generic `install(tool, force)` = `needs_install` gate → `acquire` → `mark_installed` (BugStalker is its first consumer).
- [x] 4.3 Add `bs_tool()` to `managed_tool::known()`.

## 5. Host: `thegn debug` CLI

- [x] 5.1 New `crates/thegn-host/src/cmd/debug.rs` with a clap `Action` (`Setup{force}`, `Path`, `Run{program, args}`, `Attach{pid}`); register `mod debug` + `Command::Debug` in `main.rs`.
- [x] 5.2 `setup`: platform gate → `install(bs_tool(), force)` → report; unsupported ⇒ clear message + non-zero exit, no install.
- [x] 5.3 `path`: resolve `bs` via config `[managed_tools.bs]` override + PATH + managed; print tier + path.
- [x] 5.4 `run`/`attach`: platform gate → resolve (install managed copy if selected-and-stale) → build argv → exec-replace (`CommandExt::exec`).

## 6. Host: doctor

- [x] 6.1 `doctor` managed-tools section already iterates `known()` → bs appears; add the platform-support note for unsupported hosts.

## 7. Verification

- [x] 7.1 `cargo test -p thegn-core` green (new debug + Cargo-source tests).
- [x] 7.2 `cargo clippy -p thegn-core -p thegn-host --all-targets` + `cargo fmt --check` clean; god-file ratchet OK (new code in new modules; `main.rs` command wiring only).
- [x] 7.3 Manual: `thegn debug path` (resolves bs / reports tier), `thegn debug setup` on Linux-x86-64 (installs or reports present), `thegn doctor` lists bs; on a non-supported platform the verbs refuse cleanly. NOTE: full `cargo install` fetch is a smoke/manual seam.
