# THE-61 architecture design: dependency adoption decision records

## Outcome

Create `docs/adr/` with an index and one decision record for each requested
crate/family. This lane is documentation plus OpenSpec synchronization only.
It does not add a dependency, change a version, or change runtime behavior.

The decisions, against the checked-out branch, are:

| Candidate                               | Decision     | Boundary                                                                                                                                                       |
| --------------------------------------- | ------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `rustix`                                | Defer        | Keep the existing `nix`/`libc` platform seams; prefer `rustix` only for a new syscall need that cannot be met by them.                                         |
| `windows-rs` (`windows`, `windows-sys`) | Adopt / keep | The project already uses the published split: WinRT through `windows`, raw Win32 through `windows-sys`. Version alignment is a separate, target-tested change. |
| `whoami`                                | Reject       | Existing hostname and environment-based account-name behavior is sufficient; do not add a dependency for unused identity fields.                               |
| `sysinfo`                               | Adopt / keep | Keep it as the selective, off-loop cross-platform sampler; retain direct platform samplers where measured hot paths require them.                              |
| `zerocopy`                              | Defer        | There is no fixed-layout Rust-owned wire format or byte reinterpretation site to replace today.                                                                |
| `tokio-tungstenite` / `tungstenite`     | Adopt / keep | Keep the direct async client and axum's shared server-side WebSocket stack at one lockfile version.                                                            |

No candidate is a trivially safe new adoption: the three rejected/deferred
crates would add no current use, while the other three are already adopted.
The Windows 0.59 → newer alignment suggested by the draft is not a one-line
change: `windows` is a WinRT leaf with API churn and the Windows target lanes
must be compiled. It is therefore explicitly not part of this lane.

## Verified repository baseline

The workspace declares MSRV 1.89 at `Cargo.toml:29-33`. The direct candidate
declarations are `tokio-tungstenite` 0.29 with `connect`, `handshake`, and
`rustls-tls-webpki-roots` at `Cargo.toml:153-160`; `windows` 0.58 and
`windows-sys` 0.59 at `Cargo.toml:165-177`; and feature-trimmed `sysinfo` 0.39
at `Cargo.toml:206-216`. There are no workspace declarations for `rustix`,
`whoami`, or `zerocopy`.

The lock confirms the direct workspace edges: `thegn-host` carries `libc`,
`nix`, and `windows-sys 0.59.0` at `Cargo.lock:8344-8393`; `thegn-media`
carries `windows 0.58.0` at `Cargo.lock:8395-8407`; `thegn-metrics` carries
`libc`, `nix`, and `sysinfo` at `Cargo.lock:8409-8437`; and `thegn-svc`
carries `tokio-tungstenite` at `Cargo.lock:8439-8480`. The lock also contains
transitive `rustix 1.1.4` at `Cargo.lock:7161-7172` and `zerocopy 0.8.56` at
`Cargo.lock:10349-10354`; their presence is not an adoption or a permission to
use them directly.

The current direct Windows versions are not inferred from another branch or a
Dependabot commit: `Cargo.lock:9607-9628` has both `windows 0.58.0` and
transitive `windows 0.62.2`, while `Cargo.lock:9810-9840` has the direct
`windows-sys 0.59.0` alongside other transitive versions. The checked-out
`deny.toml:70-74` documents the existing duplicate-major warning policy.

## Per-candidate architecture decisions

### `rustix`

The current pane/pty/signal/fd implementation is deliberately split by the
platform seam. `thegn-host/src/platform/unix.rs:7-20` uses `nix`'s owned-fd
API for stderr restoration; `:37-78` keeps `libc::termios` for the panic-safe
restore; `:103-151` uses `nix` for fd duplication and PID signals; and
`:176-210` uses process groups. `crates/thegn-host/src/fd_limit.rs:17-40`
uses direct `libc` for `getrlimit`/`setrlimit`. The pane itself uses
`portable-pty` and an off-thread blocking reader (`crates/thegn-host/src/pane.rs:1-13`),
not a syscall-wrapper abstraction. The manifests own `nix`/`libc` under the
Unix target sections (`crates/thegn-host/Cargo.toml:132-140`,
`crates/thegn-core/Cargo.toml:78-93`, and `crates/thegn-metrics/Cargo.toml:19-36`).

`rustix` would improve some fd/errno APIs, but it would add a second direct
syscall vocabulary without replacing `portable-pty` or the libc-only macOS
interfaces. The macOS activity scanner uses libproc and Mach timebase calls
(`crates/thegn-core/src/activity.rs:664-823`), and the metrics thermal seam
uses runtime `dlopen`/`dlsym` (`crates/thegn-metrics/src/thermal.rs:110-166`);
neither becomes a rustix migration. It also cannot remove the several
transitive `nix` versions held by upstream crates. MSRV 1.89 and musl are not
the blockers; maintenance surface, duplicate APIs, and zero user-visible gain
are. Binary size would not improve meaningfully because rustix is already
transitive (`Cargo.lock:7161-7172`), while a direct addition would still
increase the maintained dependency surface.

Decision: defer a broad migration. A future new Unix syscall should first be
considered behind `platform/`, with a measured reason and a scoped cross check;
do not migrate existing libc/nix call sites as cleanup. If identity lookup or
another missing operation appears, prefer the smallest existing seam rather
than making `thegn-core` a syscall-vendor layer.

### `windows-rs`

This is already adopted in two intentionally different forms. The raw Win32
host seam imports `windows_sys` for console handles, process liveness and
termination, and Job Objects (`crates/thegn-host/src/platform/windows.rs:17-27`),
with the target-gated declaration at `crates/thegn-host/Cargo.toml:132-140`.
The media leaf imports the WinRT SMTC API from `windows` (`crates/thegn-media/src/smtc.rs:1-25`)
and requests only `Media_Control`, `Foundation`, and
`Foundation_Collections` (`crates/thegn-media/Cargo.toml:37-43`). This is the
right provider-seam placement: platform code stays in platform/ or a leaf,
and `thegn-core` has no Windows binding.

The direct pins currently produce duplicate Windows cohorts: `windows 0.58.0`
is the media direct edge and `windows 0.62.2` is transitive; `windows-sys 0.59.0`
is the host direct edge while 0.60/0.61 are transitive (`Cargo.lock:9607-9628`
and `Cargo.lock:9810-9840`). This is a real maintenance opportunity, but
alignment is not safe to fold into a records-only change: the `windows` WinRT
API can require source edits, and Windows behavior is covered by the mingw
lane plus the opt-in MSVC job. The cross gate typechecks the leaf crates for
`aarch64-apple-darwin` and `x86_64-pc-windows-gnu` (`justfile:111-143`,
`flake.nix:179-189`); a full Windows-gnu workspace check needs the configured
mingw C compiler (`justfile:103-140`).

Decision: adopt / keep the split, defer version alignment to a separate
dependency-update commit. That follow-up must update the lock deliberately,
compile the Windows leaf and full mingw lane, run the MSRV check, and compare
`cargo tree --target all -i windows-sys@…` / `-i windows@…`. It must preserve
the feature list, target gates, and the raw-vs-WinRT ownership boundary. A
binding update is a replacement of an existing dependency, not a reason to
add a second binding family.

### `whoami`

Hostname resolution already has a tested, degrading path. On Linux it reads
`/proc/sys/kernel/hostname`; off Linux it calls `sysinfo::System::host_name()`;
then it falls through `HOSTNAME` and `COMPUTERNAME`, trimming blanks, in
`crates/thegn-core/src/util.rs:919-956`, with precedence and non-empty tests at
`:963-990`. The daemon and control server consume this shared helper
(`crates/thegn-host/src/daemon/mod.rs:252-260` and `:337-349`). Account names
are narrower uses: `USER` fills the bare-host control-path name in
`crates/thegn-core/src/remote.rs:147-164`, and Windows `icacls` uses
`USERNAME` in `crates/thegn-core/src/fsperm.rs:43-68`.

`whoami` would add a direct dependency for real-name, language, or account
metadata the product does not display or use. It would not replace sysinfo,
`nix`, or the platform seam, and it would not improve the configured musl or
mingw products enough to justify another platform implementation and advisory
surface. Existing environment values are also the intended session identity
for the SSH and ACL operations. No candidate is currently in the lock, so
rejecting it adds zero binary size and zero build cost.

Decision: reject. If a future daemonized path must resolve a local account with
no environment, file a focused identity change and prefer a narrowly scoped
`getpwuid_r` seam with explicit degradation, rather than adopting `whoami` for
unused fields.

### `sysinfo`

`sysinfo` is an existing adoption, not an alternative to evaluate from zero.
The metrics leaf declares it directly (`crates/thegn-metrics/Cargo.toml:16-25`)
and thegn-core declares it only off Linux for hostname and the Windows activity
scanner (`crates/thegn-core/Cargo.toml:88-93`). `StatsSampler` reuses one
selectively configured `System`, refreshes CPU/memory/network each ticker tick,
and refreshes slow fields every fifth tick (`crates/thegn-metrics/src/sample.rs:1-23`
and `:46-88`). It runs on a background sampler thread rather than the UI loop
(`crates/thegn-host/src/hydrate.rs:608-613`); the Observe source uses a separate
thread for the same blocking sampler (`crates/gtui-query/src/host.rs:31-58`).

The boundary is intentional: sysinfo is the cross-platform cold/periodic
sampler, not a license to enumerate processes on every tick. The targeted
process refresh is explicitly deduplicated because the prior sysinfo path could
abort on a duplicate PID (`crates/thegn-metrics/src/sample.rs:339-417`), and
the full process tab has a separate gate and two-second cadence
(`crates/thegn-metrics/src/procs.rs:1-21`, `:157-172`). macOS activity bypasses
sysinfo for a libproc scan because the code measured the whole-table refresh as
roughly five syscalls plus an ARG_MAX allocation per process
(`crates/thegn-core/src/activity.rs:664-683`); the shipped regression and
0.075 → 0.058 idle measurement are recorded in `CHANGELOG.md:880-900`.
GPU, battery, and Apple Silicon thermals remain edge-specific fallbacks
(`crates/thegn-metrics/src/lib.rs:1-24` and `crates/thegn-metrics/Cargo.toml:27-36`).

Feature trimming limits size and maintenance. The dependency is already paid
in the Linux host, musl bridge, macOS leaf check, and mingw leaf check; the
Windows activity/metrics path is checked by `just check-cross`, while the musl
build is the static `thegn-host` bridge (`flake.nix:226-255`). MSRV 1.89 is
already the workspace floor. Replacing sysinfo would be a substantive sampler
migration with no current product benefit.

Decision: adopt / keep. Preserve the `StatsSampler`/`ProcSampler` seams,
selective refreshes, background ownership, PID deduplication, and “missing
metric hides/degrades” behavior. Any future replacement must be a measured,
cross-target migration behind these owned types, not a direct dependency leak
into callers.

### `zerocopy`

There is no current byte-layout site for `zerocopy` to replace. The macOS
libproc code uses `MaybeUninit` only for kernel-filled C out-parameters and
`from_raw_parts` only to flatten a libc path buffer
(`crates/thegn-core/src/activity.rs:730-765`); these layouts are supplied by
the OS ABI, not Rust-owned packet structs. The other unsafe families are
`dlsym` function-pointer casts (`crates/thegn-metrics/src/thermal.rs:110-166`),
rlimit/sysctl calls (`crates/thegn-host/src/fd_limit.rs:21-40` and
`crates/thegn-host/src/platform/unix.rs:258-278`), and Win32 out-parameter
calls (`crates/thegn-host/src/platform/windows.rs:217-241`). `zerocopy` cannot
make those foreign-function contracts safe.

The wire formats also do not present a candidate: control attach input is JSON
text while daemon output is encoded `EventFrame` bytes and SSE is JSON
(`crates/thegn-svc/src/control/http.rs:1449-1558`); the service API uses
`prost` for gRPC and serde models elsewhere. The transitive
`zerocopy 0.8.56` lock entry (`Cargo.lock:10349-10354`) does not justify a new
direct import. Adding it would bring derive/proc-macro maintenance without
reducing unsafe surface, and it has no meaningful binary-size benefit here.

Decision: defer. Reopen only if a measured, hot, fixed-layout wire or storage
format is introduced. Then place a small codec module at the owning service
edge, pin the layout/endian/alignment contract, fuzz malformed input, and keep
thegn-core's substrate-free domain model independent of the codec.

### `tokio-tungstenite` / `tungstenite`

The async WebSocket client is already used by `thegn-svc`. Sprites native exec
uses WSS for PTY sessions and a raw TCP-over-WebSocket proxy
(`crates/thegn-svc/src/provider.rs:1311-1405`), while the control client uses
`client_async` for both event subscription and warm attach over Unix/TCP
(`crates/thegn-svc/src/control/client.rs:470-517` and `:526-600`). The control
server's axum adapter provides both the WebSocket event/attach routes and an
SSE convenience route (`crates/thegn-svc/src/control/http.rs:1406-1468` and
`:1554-1570`); SSE does not require tungstenite, but the WebSocket client and
server do. `axum 0.8.9` resolves the same `tokio-tungstenite 0.29.0`, and the
lock contains exactly `tokio-tungstenite 0.29.0` and `tungstenite 0.29.0`
(`Cargo.lock:428-451`, `:8675-8688`, and `:9060-9073`).

The direct feature set is intentionally narrow and selects webpki roots to
stay aligned with the workspace reqwest rustls stack
(`Cargo.toml:149-160`). It is owned by the async service layer, never core;
moving it to a different WebSocket implementation would add a second adapter
and lose axum sharing. The existing transport is compiled for the musl bridge
through the service graph and for mingw through the workspace, so any update
must preserve static-rustls and Windows cross checks. Binary size and MSRV are
already paid costs; maintenance risk is dominated by version skew.

Decision: adopt / keep. On a future update, change the direct pin together with
the axum-compatible lock cohort, verify a single tungstenite version with
`cargo tree --target all`, and keep all blocking/network work in service tasks
behind the existing pane/channel seam. No transport behavior changes in this
records lane.

## Audit and architecture gates

Every future adoption or version alignment must be reviewed through the
existing gate, not by a lockfile diff alone:

- `deny.toml:8-39` fails vulnerability advisories unless ignored with a
  documented reason and exit condition; unmaintained, unsound, and notice
  advisories retain their configured severity. `:41-63` limits licenses;
  `:70-78` warns on duplicate versions and permits only the documented
  workspace path wildcard; `:94-97` denies unknown registry/git sources.
- `justfile:455-462` defines `just deps-audit` as `cargo deny check` followed
  by `cargo machete`. The `deps-audit` CI job runs that recipe at
  `.github/workflows/ci.yml:121-138`; `just ci` includes it at `justfile:394-397`.
  `just lint` is a separate formatting/lint gate and does not itself invoke
  `deps-audit` on this branch.
- A direct dependency declaration must retain a nearby rationale when its
  target gate, feature trim, version pin, or ownership is not self-evident.
  `cargo machete` is the guard against leaving an unused direct dependency.
  Multiple-major warnings must either be accepted as upstream/documented or
  resolved in the same deliberate update; do not silently change
  `multiple-versions` to deny while known upstream splits remain.

No new config key, action, capability, provider, or help context exists. The
env-overlay, completion-slot, control-schema, platform, and help ratchets are
therefore unchanged and must not receive placeholder entries. No render damage
channel or event-loop wake path is touched; no SQLite schema or `user_version`
change exists; no `thegn` binary is invoked, so the temporary `XDG_STATE_HOME`
rule is vacuously satisfied.

## OpenSpec reconciliation and delivery

The existing draft correctly identified the already-adopted `sysinfo`,
Windows-rs, and tungstenite usage, but it must be pruned in three places:

1. Remove its proposed Windows version bump and `deny.toml` comment edit from
   this lane. Those are substantive cross-target dependency work, not a safe
   documentation-only adoption.
2. Correct its claim that `deps-audit` runs from `just lint`; on this branch it
   is a recipe included by `just ci` and a dedicated CI job.
3. Retain the architecture-gates delta, corrected to describe current gate
   behavior and the decision-record convention, then sync it into
   `openspec/specs/architecture-gates/spec.md` before archiving the change.

The OpenSpec proposal's claim that no direct candidate existed is satisfied
for `rustix`, `whoami`, and `zerocopy`; the existing Windows/macOS parity work
already satisfies the “use the correct platform seam” framing, but not the
unrequested version alignment. The final commit must contain the ADR set,
pipeline design/chunks, the synced canonical spec, and the dated archived
OpenSpec folder with subject:

`docs(the-61): architect design + chunk specs`
