# Debugger audit — honest spec, adapters as data, gaps named

Linear: THE-26

## Why

THE-26 asks for an honest audit of the debugger capability. Verdict first:
**what the spec claims, the code delivers** — the drift is in what the
capability's name promises versus what either says.

**Spec-vs-implementation, requirement by requirement (verified in
`thegn-core/src/debug.rs` + `thegn-host/src/cmd/debug.rs`):**

- _BugStalker is a pinned managed tool_ — ✅ `bs_tool()` is a `Cargo`-source
  `ManagedTool`, three-tier resolution (override → PATH → managed), `thegn
debug setup` installs, `thegn debug path` reports path + tier.
- _Gated to its supported platform_ — ✅ `bs_supported(os, arch)` is the pure
  predicate (Linux x86-64 only); every launch/install verb refuses elsewhere
  with an actionable message.
- _A session launches for a program or pid_ — ✅ pure argv builders
  (`bs <program> [args…]` / `bs -p <pid>`), exec-replace so a session inside
  a pane inherits sandbox/placement with no extra wiring. All pure logic is
  unit-tested.

**The honest gaps, in descending order of weight:**

1. **One vendor, hard-coded — no seam.** The capability is a launcher for
   exactly one debugger: BugStalker, which debugs **Rust only**, on **Linux
   x86-64 only**. A Python/Go/C++ worktree — or any worktree on macOS/arm64 —
   has no debugger story at all, and the house rule ("seams, not vendors")
   has no purchase here: `bs` is baked into core and the CLI verb rather
   than being the built-in default of a data-driven table. Contrast the
   direction `[[lsp.servers]]` is taking (`add-generic-lsp-registry`).
2. **The doctor probe exists but is unspecified.** `thegn doctor`'s
   managed-tools section reports `bugstalker` (tier, path, pin currency) and
   flags the platform gate — implemented behaviour with no requirement, so
   nothing stops it regressing.
3. **DAP is absent — and the landscape moved.** Roadmap **AQ 525–528** (DAP
   client substrate, breakpoints/stepping, variables panel, launch configs)
   are all `[ ]`; test-explorer debug-run is explicitly deferred onto them
   (**AQ 518**). Meanwhile BugStalker itself now ships a DAP server (stdio +
   TCP, VSCode/Zed extensions) — so when the DAP substrate is built, the
   current vendor is reachable through it. This audit records the fact; it
   does not scope the substrate (a Tier-2 programme of its own).
4. **Pin staleness**: `BS_PIN = "0.4.6"`; crates.io is at 0.4.8
   (2026-08-22). Routine bump, listed as a task.
5. Deliberate and fine, re-verified: no capability-catalog row (`thegn
debug` is a local exec-replace, not an external door); non-unix
   `exec_replace` bails (moot while the only adapter is Linux-only, and the
   registry keeps the gate per-adapter for the Windows track).

## What Changes

- **`[[debug.adapters]]` — adapters become data.** A registry entry declares
  a name, run/attach argv templates, and a platform gate (os/arch list).
  BugStalker is the built-in default entry, keeping its managed-tool
  resolution and pin; user entries (gdb, lldb, delve, …) resolve from PATH
  or an absolute path. `thegn debug run --adapter <name>` selects; the
  default stays `bs`; an unknown adapter is refused naming the known set.
- **The platform gate goes per-adapter.** The pure `(os, arch)` predicate
  becomes a property of the adapter entry; `bs` keeps Linux-x86-64, a
  user's lldb entry can claim darwin. The "unsupported platform" refusal
  names the adapter, not the feature.
- **Doctor behaviour is specified** (codifying what ships): the debugger
  tool's resolution tier, pin currency, and platform-gate note — extended
  with a row per configured adapter.
- **Spec realignment**: the launch requirement is rewritten over the
  selected adapter's template; the BugStalker managed-tool requirement is
  unchanged (it remains the built-in default).
- **Pin bump** to current BugStalker as an implementation task.
- **Recorded, not scoped**: the DAP client substrate (AQ 525–528) and
  test-explorer debug-run stay open roadmap items; this change neither
  builds nor blocks them, and the adapter registry gives the future DAP
  work a config surface to hang launch configurations on.

## Impact

- **Roadmap**: audits the debugger portion of the Phase-1 capability table
  (`managed-tools`/`debugger`); leaves **AQ 525–528** (DAP tier) and the
  **AQ 518** debug-run deferral open and cited. No roadmap item is closed by
  this change; the registry is new scope under the debugger capability.
- **Specs**: `debugger` (ADDED adapter registry + doctor requirements;
  MODIFIED platform gate + session launch). No catalog row — nothing here
  is externally invokable; the CLI verb remains a local exec.
- **Code (indicative)**: `thegn-core/src/debug.rs` (adapter table + argv
  templates + per-adapter gate, staying pure), `thegn-core/src/config.rs`
  (`[[debug.adapters]]`), `thegn-host/src/cmd/debug.rs` (`--adapter`,
  refusal message), doctor section extension,
  `config/config.toml.example`.
- **In-flight changes**: `add-config-trust-resolution` — adapter commands
  are subprocess argv from config; same trust stance as the LSP registry
  (worktree-layer entries ignored until trust lands). Sibling pattern:
  `add-generic-lsp-registry` (same registry shape, no shared code, no
  dependency). The Windows programme (`add-windows-parity`) benefits from
  the per-adapter gate but is not depended on.

## Non-goals

- The DAP client substrate, breakpoint/stepping UI, variables panel, or
  launch-configuration profiles (AQ 525–528) — a separate Tier-2 programme.
- Managed installation of non-BugStalker adapters (user adapters resolve
  from PATH; managed-tool specs for gdb/lldb/delve are future work).
- Any change to how a session inherits pane sandbox/placement — the
  exec-replace contract is verified correct and kept.
