# Design — dependency adoption record (THE-61 "Worth using?")

This document IS the deliverable: the per-crate evaluation, each measured
against what the tree does today (verified by grep/`cargo tree`/Cargo.lock,
2026-08-25, workspace at 0.1.0-alpha.2, MSRV 1.89). Verdicts: **adopted
(keep)**, **adopt (alignment)**, **defer**, or **reject** — with the condition
that would reopen each.

Evaluation axes per the issue: current usage → concrete benefit → migration
cost → MSRV/check-cross impact → binary size → substrate-free-core fit. All
verdicts were checked against the deps-audit gate's policy (deny.toml:
advisory exceptions need reason + exit condition; license allowlist;
`multiple-versions = "warn"` with named known splits; cargo-machete forbids
unused direct deps — a rejected crate never enters the manifest to become
machete debt).

## 1. rustix — REJECT (defer indefinitely)

**Today**: unix syscalls go through `nix` 0.31 (features fs/time/signal/
process; ~20 files across core/host/svc/metrics) and direct `libc` where nix
has no wrapper (~11 files): macOS libproc (`proc_listallpids`/`proc_pidinfo`/
`proc_listchildpids`, `mach_timebase_info`) in `crates/thegn-core/src/
activity.rs` + `crates/thegn-host/src/platform/proc.rs`, rlimits in
`fd_limit.rs`, malloc/memory introspection in `mem.rs`, `geteuid` in
`daemon/mod.rs`, IOKit-adjacent glue in `thegn-metrics/src/thermal.rs`.
rustix 1.1.4 is **already in the lock transitively** (gix-index,
alacritty_terminal, keyring→async-io).

- **Benefit**: rustix's I/O-safety types and errno handling are nicer than
  nix's; no functional capability thegn lacks.
- **Migration cost**: ~30 files of ratcheted platform code for zero
  user-visible change — and it cannot finish the job: the direct-libc sites
  are mostly calls rustix does not wrap (libproc, mach timebase, mallinfo),
  and the nix duplicates in the lock (0.28 via portable-pty, 0.29 via
  termwiz/mac_address) are upstream pins we don't control, so the tree keeps
  nix either way.
- **MSRV/check-cross**: fine (rustix MSRV ≪ 1.89); no windows-gnu relevance
  (unix-only concern).
- **Binary size**: none — rustix is compiled in already via transitives;
  dropping nix would remove one small crate but not libc.
- **Core fit**: allowed (core bans tokio/termwiz/pty/HTTP, not syscall
  wrappers) — irrelevant given the verdict.

**Reopen when**: a dependency shuffle removes nix from the transitive tree
anyway, or new platform code needs an interface nix lacks and rustix has —
then prefer rustix for the _new_ seam rather than migrating old ones.

## 2. windows-rs — ADOPTED ALREADY; ADOPT the version alignment

**Today**: both published crates of microsoft/windows-rs are direct deps,
correctly split by need and target-gated to `cfg(windows)`:

- `windows = "0.58"` — WinRT (System Media Transport Controls) in the
  `thegn-media` leaf. WinRT/COM requires the full `windows` crate.
- `windows-sys = "0.59"` (Foundation, Console, Threading, JobObjects) — raw
  Win32 in `crates/thegn-host/src/platform/windows.rs` (stderr redirect,
  process liveness/termination, Job Objects). `windows-sys` is the right
  choice for a CLI's platform seam: declaration-only bindings, no COM
  machinery, fast to compile.
- The `platform-windows` spec and every add-windows-\* in-flight change build
  on this same choice; `add-windows-native-compile` tasks name the
  `windows-sys` feature set explicitly. Notably `thegn-core`'s Windows path
  binds nothing: `fsperm.rs` shells out to `icacls`, keeping core
  substrate-free.

So "windows-rs vs whatever is current" resolves to: **we already use
windows-rs; the debt is version skew**. The lock carries `windows`
0.58 (ours) + 0.62.2 (sysinfo's Windows backend, wmi) and `windows-sys`
0.45/0.48/0.52 (legacy transitives) /0.59 (ours, beside alacritty_terminal)
/0.60.2/0.61.2 (the modern transitive cohort). deny.toml's `[bans]` comment
names the windows split as a consolidate-then-ratchet item.

- **Benefit**: bumping our pins (`windows-sys` 0.59→0.61, `windows`
  0.58→0.62) joins the majority cohorts, deleting the two duplicate majors we
  cause; windows-sys ≥0.60 links via `windows-link` (raw-dylib), which drops
  the import-library dependency — a mild simplification for the
  `x86_64-pc-windows-gnu` check-cross lane (which today skips a full link
  when no mingw cc is present).
- **Migration cost**: S for `windows-sys` (declaration-compatible; re-state
  the feature list); S–M for `windows` 0.58→0.62 in `thegn-media` (windows-rs
  API churn across windows-core reorganisations; the SMTC surface is small
  and the crate is a cfg-gated leaf).
- **MSRV/check-cross**: both crates' MSRV ≪ 1.89; raw-dylib is stable on all
  Windows targets well below 1.89. Verify the check-cross windows-gnu lane in
  the task — that gate runs in `just ci`, not per-edit.
- **Binary size**: zero on Linux/macOS (never linked); on Windows targets the
  dedupe removes redundant binding copies from the build graph (mostly a
  compile-time win; bindings are largely declarations).
- **Core fit**: unaffected — core keeps zero windows-rs deps.

**Sequencing**: land the alignment before (or fold it into) the
add-windows-native-compile wave so new Windows code is written once against
0.61, not migrated after.

## 3. whoami — REJECT

**Today**: the two concerns whoami covers are already solved narrowly:

- Hostname: `thegn_core::util::hostname()` — `/proc/sys/kernel/hostname` on
  Linux, `sysinfo::System::host_name()` elsewhere, `HOSTNAME`/`COMPUTERNAME`
  env fallbacks, blank-skipping, unit-tested (the "quietly said localhost"
  regression is pinned by a test).
- Username: `env::var("USER")` as the default ssh user (`remote.rs`) and
  `env::var("USERNAME")` for the `icacls` grant target (`fsperm.rs`).

- **Benefit**: a libc `getpwuid` fallback for unset env vars, plus realname/
  langs we have no use for (`sys-locale` already covers locale in core).
- **Cost of adopting**: a new direct dep (with a RUSTSEC history in exactly
  the FFI path that would be its value-add — RUSTSEC-2024-0020, stack buffer
  overflow in the passwd handling, fixed in 1.5.0) to replace two one-line
  env reads that are correct for their uses: an interactive terminal session
  has `USER`/`USERNAME`, and `icacls` wants the account name the environment
  advertises.
- **MSRV/size/core fit**: all fine — and all moot.

**Reopen when**: thegn needs identity resolution in a context with no
environment (a daemonised path spawned outside a session), at which point
prefer a targeted `getpwuid_r` via nix over the whole crate.

## 4. sysinfo — ADOPTED ALREADY; KEEP (record the boundary rule)

**Today**: `sysinfo` 0.39, feature-trimmed (system/disk/network/component/
multithread), direct dep of `thegn-metrics` (masthead + telemetry sampling)
and of `thegn-core` **off Linux only** (Windows activity scanner via the PEB
read; `platform_hostname` on every non-Linux target). Refreshes happen only
on the ticker tick — sysinfo does no background work, preserving 0% idle (the
manifest comment records this). `starship-battery` covers the one metric
sysinfo lacks.

**The boundary rule worth recording**: sysinfo is the cross-platform
_cold/1Hz_ sampler; measured hot paths take direct syscalls. The macOS
activity scanner deliberately bypasses sysinfo for libproc (~1 syscall per
process vs ~5 plus an ARG_MAX-sized allocation, at up to 1Hz —
`crates/thegn-core/Cargo.toml` documents the measurement). Don't "simplify"
the libproc seam back onto sysinfo; don't hand-roll a new platform sampler
where the tick cadence makes sysinfo's cost irrelevant.

MSRV fine; size cost already paid; the off-Linux core dependency is the
documented exception (sysinfo is not on core's banned-substrate list).

## 5. zerocopy — REJECT

**The issue's question**: does it replace any hand-rolled transmutes? **No.**
The `unsafe` census (grep across crates, test-only `env::set_var` blocks
excluded) decomposes into:

- macOS libproc out-param FFI: `MaybeUninit` + `assume_init` +
  `slice::from_raw_parts` over kernel-filled C structs
  (`activity.rs`, `platform/proc.rs`). zerocopy cannot model out-param
  syscalls; the structs are libc's, already layout-defined.
- dlsym fn-pointer transmutes for IOKit (`thegn-metrics/src/thermal.rs`) —
  explicitly outside zerocopy's domain (no fn-pointer support).
- rlimit/malloc/uid syscalls (`fd_limit.rs`, `mem.rs`, `daemon/mod.rs`) and
  the Win32 `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` pointer cast
  (`platform/windows.rs`) — FFI calls, not reinterpretation of Rust data.

And there is no wire-format candidate either: daemon IPC and the control
plane use serde_json text frames and prost, the WS attach protocol is JSON —
no fixed-layout struct parsing anywhere. zerocopy 0.8.56 is already in the
lock transitively (ahash via fff-search; ppv-lite86), so a future adoption
costs nothing in size — there is simply no site for it.

**Reopen when**: a genuinely hot fixed-layout wire format appears (e.g. a
binary frame protocol for pane bytes) — then zerocopy's derive is the right
tool and is already in tree.

## 6. tokio-tungstenite / tungstenite — ADOPTED ALREADY; KEEP (record the constraint)

**Today**: `tokio-tungstenite` 0.29, default-features off, `connect` +
`handshake` + `rustls-tls-webpki-roots` (deliberately matching reqwest's
rustls/ring so there is a single rustls in tree — the manifest comment
records this). Direct uses, both client-side, both in `thegn-svc`:

- The Sprites native-exec provider (`provider.rs`): PTY-over-WSS exec,
  reattach, and the TCP-over-WebSocket proxy.
- The control-plane client (`control/client.rs`): warm-attach over WS via
  `client_async` on its own stream types.

The control-plane **server** side is axum's WS extractor
(`control/http.rs`: event feed + warm attach) — and axum 0.8.9 itself
depends on tokio-tungstenite 0.29, so the lock holds exactly one tungstenite.

**The constraint to keep**: our direct pin must track axum's tungstenite
major. Bumping tokio-tungstenite ahead of axum (or lagging an axum upgrade)
splits the tree into two tungstenite copies — precisely the duplicate-major
debt deny.toml warns on. Check `cargo tree -i` on either bump.

No alternative merits a look: tungstenite is the ecosystem default, the
async wrapper is required (the WS code lives on tokio in svc — and MUST stay
out of core), and fastwebsockets/soketto would forfeit the axum sharing.

## Config.yaml design rules, addressed

- **Render damage channel / wake path**: none touched — this change is a
  decision record plus a version bump in cfg(windows) leaves.
- **SQLite schema**: none; no `user_version` bump.
- **Help context key**: no new interactive surface; no help page.

## Security

- **Supply chain**: no new crates enter the tree under any verdict. The one
  code task changes versions of two Microsoft-published crates the tree
  already carries at those exact versions transitively; the lock diff is the
  review surface, and `just deps-audit` (advisories DB, license allowlist,
  `unknown-registry`/`unknown-git = "deny"`) gates the result in `just ci`.
- **Rejections reduce risk**: whoami is declined partly because its
  value-add path (passwd FFI) is where its RUSTSEC-2024-0020 overflow lived;
  the env-var status quo has no FFI. zerocopy's rejection keeps the unsafe
  census small and local rather than spreading derive-driven layout
  assumptions with no payoff.
- **Credentials**: untouched — no new config keys, no token handling. The WS
  clients keep their existing bearer-header injection; TLS roots remain
  webpki, aligned with reqwest.
- **Blast radius**: no new write surface, no capability-catalog row, no
  externally invokable operation.

## Open questions

1. Should the multiple-versions `[bans]` setting ratchet warn→deny once the
   windows split is consolidated? Not yet — syn 1+2 and clap 2+4 (via
   tokei's legacy tree) still block it; the deny.toml comment stays accurate
   with the windows entry removed.
2. Does the `windows` 0.62 migration in `thegn-media` land here or inside
   add-windows-parity's wave? This change claims it (small, leaf-local), but
   if the windows wave starts first, fold task 1.2 into it and drop it here —
   the decision record stands either way.
3. When axum next bumps its tungstenite, do we track immediately or batch?
   Recorded constraint says: same change, verified single-copy via
   `cargo tree -i` — never split.
