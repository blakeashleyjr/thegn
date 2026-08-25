# Record the "Worth using?" dependency verdicts; align the Windows bindings versions

Linear: THE-61

## Why

THE-61 asks whether thegn should use six well-known crates: `rustix`,
`windows-rs`, `whoami`, `sysinfo`, `zerocopy`, and
`tokio-tungstenite`/`tungstenite`. The audit (design.md is the per-crate
record) finds the question is **mostly already answered in the tree**, just
never recorded:

- **`sysinfo` is adopted** (0.39, feature-trimmed, ticker-tick sampling only —
  the manifest comment documents the 0%-idle reasoning), with a deliberate,
  measured boundary: the macOS activity scanner bypasses it for direct libproc
  (~1 syscall/process vs sysinfo's ~5, on a 1Hz path).
- **`tokio-tungstenite` is adopted** (0.29, rustls-webpki-roots to keep a
  single rustls beside reqwest): the Sprites WSS exec/attach/proxy client and
  the control-plane warm-attach client use it directly; axum 0.8.9 serves the
  control-plane WS server side on the same 0.29 — exactly one tungstenite in
  the lock.
- **`windows-rs` is adopted twice** — both `windows` (0.58, WinRT SMTC in
  `thegn-media`) and `windows-sys` (0.59, raw Win32 in the host platform seam)
  are published from microsoft/windows-rs. What's left is a version-alignment
  debt deny.toml already names: the lock carries `windows` 0.58+0.62 and six
  `windows-sys` majors, and its `[bans]` comment says "ratchet
  multiple-versions to deny once consolidated".
- **`rustix`, `whoami`, `zerocopy` are rejected** with reasons (design.md):
  rustix duplicates nix/libc coverage for a ~30-file migration with no
  user-visible gain and cannot replace the macOS libproc or mingw-relevant
  paths; whoami would serve two call sites env vars already cover; zerocopy
  replaces zero of the tree's unsafe blocks (they are FFI out-param syscalls,
  dlsym fn-pointer casts, and edition-2024 `env::set_var` — not
  byte-reinterpretation), and thegn's wire formats are serde_json/prost, not
  fixed-layout structs.

The gap this change closes: the verdicts live nowhere, so the question gets
re-asked, and the deps-audit gate (`just deps-audit`: cargo-deny +
cargo-machete, run by `just ci` and `just lint`) that every one of these
verdicts leaned on is load-bearing repo behaviour with no spec requirement —
deleting deny.toml's license allowlist or the machete pass would violate
nothing.

## What Changes

- **Decision record**: design.md carries the six per-crate evaluations
  (current usage, concrete benefit, migration cost, MSRV/check-cross impact,
  binary size, substrate-free-core fit) so future "worth using?" passes start
  from a written baseline.
- **Spec**: `architecture-gates` gains one ADDED requirement describing the
  dependency-adoption gate as it behaves today — `just deps-audit` (cargo-deny
  advisories/licenses/bans/sources + cargo-machete), the
  rationale-comment-per-direct-dependency convention the workspace manifest
  already follows, and the documented-known-splits posture for duplicate major
  versions.
- **One adoption task** (the only code work that merits it): align the
  Windows bindings versions — `windows-sys` 0.59 → 0.61 (workspace manifest,
  same feature list) and `windows` 0.58 → 0.62 (`thegn-media`, with the
  windows-rs API migration that entails) — removing the two duplicate majors
  our direct pins cause and joining the cohort sysinfo and the modern
  transitives already resolve to. Everything else is keep-as-is or reject; no
  other manifest change.

## Non-goals

- No new capability, action, config key, or help page — nothing externally
  invokable is added, so no `capability::CATALOG` row and no help-ratchet
  claim.
- No rustix/whoami/zerocopy migration, and no policy that bans them — the
  record defers/rejects for today's tree and names the conditions that would
  reopen each verdict.
- No change to the deps-audit gate's behaviour, deny.toml policy values, or
  the multiple-versions warn→deny ratchet timing (still blocked by the
  syn 1+2 and clap 2+4 splits via tokei even after the windows alignment).

## Impact

- Roadmap: no tasks.md item covers dependency stewardship directly (the
  "dependency spine" section is architecture, not an item); the audit phase
  can wire this where it sees fit. THE-61 is fully covered by this change.
- Specs: `architecture-gates` — 1 ADDED requirement (the deps-audit gate and
  adoption conventions). No other capability is touched.
- Code: `Cargo.toml` (workspace `windows-sys` pin), `crates/thegn-media`
  (`windows` 0.62 migration), `Cargo.lock`, and the deny.toml `[bans]` comment
  updated to drop the windows entry from the known-splits list once
  consolidated.
- In-flight reconciliation: **add-windows-native-compile** (its tasks add the
  `windows-sys` Foundation/Console/Threading/JobObjects features this change
  re-pins at 0.61 — land the bump before or rebase it into that wave),
  **add-windows-job-objects**, **add-windows-daemon-ipc**,
  **add-windows-parity**, **add-windows-compositor-validation**,
  **add-windows-ci-distribution** (all build on the same bindings; none picks
  a different bindings crate, so the decision here is confirmation, not
  conflict). The tokio-tungstenite "track axum's version" constraint is
  relevant to any future control-plane change but conflicts with none
  in flight.
- Event loop, render damage, SQLite schema, capability catalog, keybinds:
  untouched.
