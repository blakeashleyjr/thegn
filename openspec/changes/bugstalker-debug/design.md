## Context

Phase 1 landed a managed-tool resolver (`superzej-core::managed_tool`) with two
sources (`Npm`, `GithubRelease`) and a host-side fetch seam
(`superzej-host::managed_tool::{acquire, mark_installed, run_setup_cmd}`, plus a
deferred generic `install`). This change (Phase 2) is its first new consumer: a
debugger.

Superzej hosts external TUIs as **PTY panes** wrapped by the worktree's sandbox
(`sandbox::enter_argv`) and remote placement (`placement::interactive_argv`).
BugStalker (`bs`) is exactly such an external TUI (console↔TUI, standard DAP),
Linux-x86-64-only, installed via `cargo install bugstalker`.

Both `run.rs` (23k) and `keymap.rs` (3028) sit at their god-file ratchet
ceilings, so a new in-app keybind/Action is expensive and out of scope; a CLI
verb is the low-churn, testable entry point.

## Goals / Non-Goals

**Goals:**

- Acquire + pin `bs` through the shared resolver (new `Cargo` source).
- A pure, tested platform gate and session-argv builders in core.
- A `szhost debug` CLI (`setup`/`path`/`run`/`attach`) that starts a session by
  exec-replacing the current process, so a session started in a superzej pane
  inherits that pane's sandbox/placement for free.
- `doctor` reports `bs`.

**Non-Goals:**

- An in-app Debug tab/keybind (ratchet cost; belongs to the native DAP-client
  panel follow-on).
- Acting as a DAP client / rendering breakpoints/vars in chrome (that is the
  deferred Phase 1.2b).
- Non-Linux support (BugStalker's own constraint).

## Decisions

### `Source::Cargo { crate_name }` in the resolver

`cargo install <crate> --version <v> --root <managed_dir>` puts the binary at
`<managed_dir>/bin/<name>`. Constructor `ManagedTool::cargo(name, crate_name,
bin_name, version)`; `asset_for`/`repo` return `None` for Cargo.

- **Why cargo, not GithubRelease:** BugStalker ships on crates.io, not as
  predictable release assets; `cargo` is the documented install path (mirrors pi
  using `npm`). Generally reusable for Rust tools.
- **Host arm:** extend `acquire()` with the cargo command; finally add the
  generic `install(tool, force)` = gate → `acquire` → `mark_installed` (now has a
  consumer, so no dead code).

### Core `debug.rs` — pure and tested

```
pub const BS_PIN: &str = "0.4.6";
pub fn bs_tool() -> ManagedTool                     // Cargo "bugstalker"/bin "bs", fallback ["bs"]
pub fn bs_supported(os: Os, arch: Arch) -> bool     // Linux && X64
pub fn platform_supported() -> bool                 // bs_supported(Os::current(), Arch::current())
pub fn launch_argv(bin: &str, program: &str, args: &[String]) -> Vec<String>   // [bin, program, ...args]
pub fn attach_argv(bin: &str, pid: i64) -> Vec<String>                          // [bin, "--pid", pid]
```

All pure → unit-tested (the coverage-gated core). `bs`'s real attach flag is
confirmed against its docs before shipping; the builder keeps it in one place.

### `szhost debug` CLI (`cmd/debug.rs`)

- `setup [--force]`: platform gate → `managed_tool::install(bs_tool(), force)` →
  report. Unsupported ⇒ a clear message, exit non-zero, no install.
- `path`: resolve `bs` (config override → PATH → managed) and print tier + path.
- `run [program] [-- args]` / `attach <pid>`: platform gate → resolve (install
  the managed copy if selected-and-not-current) → build argv → **exec-replace**
  (`CommandExt::exec`) so `bs` owns the terminal. Run inside a pane ⇒ inherits
  its sandbox/placement (no extra wrapping here — that's the whole point).
- Resolution reuses the config `[managed_tools.bs]` override tier from Phase 1.

### doctor + known()

`superzej_core::debug::bs_tool()` is added to host `managed_tool::known()`, so it
appears in the doctor managed-tools section; doctor also notes the platform gate.

## Risks / Trade-offs

- **[no in-app tab]** Users start debugging via `szhost debug run` in a pane, not
  a dedicated tab. → Documented; the CLI-in-a-pane path already yields the
  sandbox/placement-aware session the plan wanted; the richer tab is the DAP
  panel follow-on.
- **[cargo install latency]** First `debug setup`/`run` on a fresh box compiles
  `bs` (slow). → `Once` policy installs once and pins; PATH/override tiers skip
  it; the message says what's happening; it runs off the loop (CLI foreground).
- **[platform]** Non-Linux hosts can't run `bs`. → Pure gate refuses early with a
  message pointing at distro/nix installs; `doctor` shows it as unsupported.
- **[exec vs spawn]** `run` exec-replaces the `szhost debug` process. → Correct
  for a CLI that hands the terminal to `bs`; never reached on the compositor loop.

## Migration Plan

Pure addition. New `Source::Cargo` variant is additive to the resolver enum
(exhaustive matches in host `acquire` updated). New `debug` core module + CLI
verb; no config-breaking change (`[managed_tools.bs]` is optional). Rollback =
revert; no persisted state.

## Open Questions

- Pin `BS_PIN` to the latest tagged BugStalker (`0.4.6`); bump as needed. Whether
  to expose `--version`/channel override on `debug setup` is deferred.
