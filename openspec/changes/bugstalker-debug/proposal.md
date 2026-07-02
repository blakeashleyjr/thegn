## Why

superzej has no debugging story. BugStalker (`bs`) is a self-contained Rust/Linux
debugger with its own TUI that speaks standard DAP — the cheapest way to give
superzej a real debugger is to _run `bs` in a pane_, acquired and pinned through
the Phase-1 managed-tool resolver. Because a pane already runs inside the
worktree's sandbox and placement, a debug session started in a pane inherits
them for free — a session on an ssh-placed worktree runs `bs` on the remote with
no DAP client required.

## What Changes

- Add a **`Cargo` source** to the managed-tool resolver (`superzej-core`):
  `cargo install <crate> --version <v> --root <managed_dir>`, binary at
  `<managed_dir>/bin/<name>`. BugStalker distributes via crates.io
  (`cargo install bugstalker`, binary `bs`), not GitHub-release binaries, so
  this is the right acquisition path (and generally useful for Rust tools).
- Add a **`debugger` capability** in `superzej-core` (`debug.rs`): the pinned
  `bs` [`ManagedTool`] spec, a pure **platform gate** (Linux x86-64 only — the
  debugger's own constraint), and pure **session-argv builders** (launch a
  program, or attach to a pid).
- Add a **`szhost debug` CLI subcommand**: `setup [--force]` (ensure `bs` is
  installed via the resolver — the generic host `install()`), `path` (print the
  resolved binary + tier), `run [program] [-- args]` and `attach <pid>` (start a
  BugStalker session by exec-replacing the current process, so it owns the
  terminal — run inside a superzej pane to debug within that pane's
  sandbox/placement). All platform-gated with a clear message where unsupported.
- **`szhost doctor`** lists `bs` among managed tools, with the platform note.

Non-goals (deferred): a first-class in-app "Debug" tab/keybind (would grow the
ratcheted `run.rs`/`keymap.rs`; the native DAP-client panel — the separate
Phase 1.2b follow-on — is its proper home) and driving `bs` over DAP as a client.
This change ships the acquire + launch-in-a-pane path.

## Capabilities

### New Capabilities

- `debugger`: how superzej acquires and launches a debugger — the pinned
  BugStalker managed-tool spec, the Linux-x86-64 platform gate, the
  launch/attach session-argv builders, the `szhost debug` verbs, and the
  "debug session inherits the pane's sandbox/placement" model.

### Modified Capabilities

- `managed-tools`: add a `Cargo` acquisition source (`cargo install --root`)
  alongside `GithubRelease` and `Npm`.

## Impact

- **Code:** `crates/superzej-core/src/managed_tool.rs` (+`Source::Cargo`),
  new `crates/superzej-core/src/debug.rs` (+`lib.rs` export);
  `crates/superzej-host/src/managed_tool.rs` (Cargo acquire arm + generic
  `install()`), new `crates/superzej-host/src/cmd/debug.rs`, `main.rs` command
  wiring, `cmd/doctor.rs` + host `managed_tool::known()` gain `bs`.
- **Dependencies:** none new (uses `cargo` on PATH as the installer, like the pi
  install uses `npm`).
- **Invariants:** core stays pure + 95%-coverage-gated (spec, platform gate, and
  argv builders fully unit-tested); the `cargo install` fetch is a
  `cov_ignore`/smoke seam; no event-loop or render-plan surface touched (the CLI
  `run` exec-replaces; it never runs on the compositor loop).
- **Roadmap (`tasks.md`):** opens a debugger track under **AQ** (IDE tooling);
  consumes the Phase-1 managed-tool resolver; composes with remote placement
  (**J**) via the pane a session runs in.
