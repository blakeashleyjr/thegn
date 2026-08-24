# Native Windows audit — build + run on Windows 11

Audit date: 2026-08-22. Machine: Windows 11 Enterprise 10.0.26200, x86_64,
12 cores, domain-joined (`USERDOMAIN=ACCOUNTS`). Windows Terminal 1.24.11321.0.

> **Status: the audit's findings have since been acted on.** See
> [What changed](#what-changed) at the end for the fixes, the measured
> before/after, and what is deliberately still open. The findings below are
> kept as written (with per-finding status markers) so the reasoning and the
> original measurements stay auditable.

## Verdict

thegn **builds and runs natively on Windows today.** Both the full debug link
and the full release build completed here — the release build is an artifact
the opt-in `windows` CI job has never produced (it has been timing out
mid-link). The compositor renders a real first frame in Windows Terminal, the
named-pipe daemon comes up, and ConPTY panes spawn.

It is **not yet "supported"**, and the gap is narrower and more specific than
`KNOWN_ISSUES.md` currently implies. Three things stand between here and a
green local gate, in priority order:

1. **`thegn-core`'s test target does not compile on Windows** (one ungated
   `std::os::unix` import). `cargo test --workspace` cannot run at all, so the
   95%-line-coverage gate crate is entirely unexercised on this platform.
2. **The ~0%-idle invariant is violated.** A release-build idle compositor
   burns **23% of one core**. Root cause is *not* the render path (which is
   healthy) — it is continuous thread churn.
3. **97 test failures** across `thegn-host` + `thegn-svc`. Nearly all are
   test-harness POSIX assumptions, not product defects.

## What was verified working

These are settled — evidence below; no need to re-litigate them.

| Area | Evidence |
| --- | --- |
| Workspace type-checks | `cargo check --workspace --locked` green, 8m10s, **1** warning |
| Full debug link | `cargo build --workspace --locked` green → `thegn.exe` 132 MB |
| Full release build | `cargo build --release -p thegn-host --locked` green, **18m01s**, 92.3 MB |
| Named-pipe daemon IPC | 3/3 pass incl. `pipe_bind_is_the_lock_and_round_trips` |
| Job Objects | 2/2 pass — `job_terminate_reaps_the_tree`, `dropping_the_last_handle_reaps_the_tree` |
| `platform::proc` | 5/5 pass |
| Small crates | `gtui-*`, `tg-kit`, `thegn-media`, `thegn-metrics` — 78/78 pass |
| Compositor first frame | renders chrome + sidebar + panel + statusbar, exit 0 |
| WT capability detection | `WT_SESSION` → Unicode glyphs chosen, not the ASCII fallback |
| ConPTY panes | `pty panes spawned spawn_ms=0 panes=1` |
| Windows sandbox policy | warns + declines OCI correctly, points at WSL2 |
| `fsperm` / `icacls` | verified on a **domain-joined** box: bare `blakea` resolved to `ACCOUNTS\blakea`, inheritance stripped, owner-only DACL |
| Path handling | `~/code` → `C:\Users\blakea\code`; `%APPDATA%` / `%LOCALAPPDATA%` honored |
| Render-plan invariants | release: `render_p50_us=2048`, `render_busy_ratio=0.002`, `idle_ratio≈0.97`, **zero** slow-frame warnings |

## Prerequisites (absent on a clean box)

This machine had **no Rust toolchain, no MSVC, and no Windows SDK**. Installed
during the audit, matching CONTRIBUTING "Windows (native) notes":

- rustup + `stable-x86_64-pc-windows-msvc` → rustc **1.98.0**
- VS Build Tools 2022, `VC.Tools.x86.x64` → MSVC **14.44.35207**
- Windows SDK **10.0.22621.0**

Good news for the build graph: **no `cmake`, no `aws-lc-sys`, no OpenSSL** in
`Cargo.lock`. Every C dependency (bundled sqlite, libgit2 via `fff-search`,
LMDB, zlib, tree-sitter, ring) builds with plain `cc`/MSVC. Nothing else is
needed.

Note `rustup-init` warns `installing msvc toolchain without its prerequisites`
if run before Build Tools — harmless, but it means the ordering in
CONTRIBUTING is worth stating explicitly.

## Findings

### W1 — `thegn-core` test target does not compile (blocker) — **FIXED**

`crates/thegn-core/src/sandbox_tests.rs:1225`, inside a plain, ungated
`#[test]`:

```rust
use std::os::unix::fs::PermissionsExt;
let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
assert_eq!(mode, 0o600, "env-file must be 0600");
```

```
error[E0433]: cannot find `unix` in `os`
error[E0599]: no method named `mode` found for struct `Permissions`
error: could not compile `thegn-core` (lib test) due to 2 previous errors
```

Consequences: `cargo test --workspace` fails to build; the crate carrying the
95% coverage gate has **zero** test coverage on Windows; `just coverage` can
never be reproduced here. This is why the CI `windows` job runs only *scoped*
subsets (`-p thegn-svc --lib ipc`, `-p thegn-host platform::`) — but the
compile failure itself is not recorded anywhere in the repo.

Mildly ironic: the test asserts the very "0600 for secrets" behavior that
`thegn_core::fsperm` exists to make cross-platform, but does so with a
unix-only assertion.

### W2 — the ~0%-idle invariant is violated (blocker) — **STILL OPEN**

CLAUDE.md treats `~0% idle CPU` as a hard invariant. Measured here, with the
compositor idle in Windows Terminal:

| Build | UI process | Daemon process |
| --- | --- | --- |
| debug | **38–39%** of one core | 0.10% |
| release | **23.3–23.5%** of one core | **0.00%** |

The daemon reading exactly `0.00%` is the control that validates the
measurement method — a genuinely blocked process does read zero here.

**It is not the render path.** thegn's own profiler, release build:

```
wakes_per_s=1.19  renders_per_s=0.64  full_frames_per_s=0.64  pane_frames_per_s=0.0
render_p50_us=2048  render_busy_ratio=0.0021  idle_ratio=0.966  hot_source="Refresh"
cpu_hydrate_ms=0.0 cpu_stats_ms=0.0 cpu_pr_ms=0.0 cpu_metrics_ms=0.0 cpu_diff_ms=0.0
```

Renders are 2 ms against a 16 ms budget, the loop believes it is 96–98% idle,
and **zero** slow-frame warnings fire. (Debug builds *do* trip the slow-frame
WARN at `render_p50_us=32768–65536`; that is debug overhead only — all 10
warnings in the log predate the release run. Do not read them as a release
regression.)

**Root cause: thread churn.** Sampling the idle release UI process over 20 s:

```
live thread count per sample : 33, 37, 36, 30, 31, 33, 37, 51, 35, 33
distinct thread IDs seen     : 69
process cpu                  : 4765.6 ms / 20296 ms = 23.48% of one core
```

69 distinct thread IDs in 20 idle seconds against a max of 51 live. Per-thread
CPU diffing accounts for only ~8% of a core; the rest is spent in threads that
are created and destroyed between samples. Thread creation is far more
expensive on Windows than the equivalent Linux `clone()`, which is exactly why
this invariant holds on Linux and breaks here.

Every per-subsystem CPU counter reads `0.0` — so **whatever is churning is
invisible to thegn's own instrumentation**. Likely candidates are
`spawn_blocking` work on the 2 s Refresh tick (`hot_source="Refresh"`), the
sysinfo metrics sampler, or the `notify` fs-watchers.

Pinning the exact call site needs a profiler, and the repo's in-process
flame-graph profiler is SIGUSR2-driven and unix-only — **it cannot be used on
the one platform that currently needs it.** A Windows-capable sampling profiler
(or extending the `cpu_*_ms` attribution to cover thread spawns) is the natural
next step.

### W3 — 97 test failures, almost all harness portability — **PARTLY FIXED**

| Target | Result |
| --- | --- |
| `thegn-host` | 1921 passed, **31 failed**, 7 ignored (98.4%) |
| `thegn-svc` | 384 passed, **66 failed** (85.3%) |
| `thegn-core` | **does not compile** (W1) |
| everything else | 78 passed, 0 failed |

Root-cause tally over the failing tests:

| Cause | Count |
| --- | --- |
| POSIX program missing (`sh`, `cat`, `/bin/cat`, `cp`, `wc`, `sha256sum`) | 33 |
| `spawn child` / `open pty: spawn child` (spawning `/bin/sh` in a PTY) | 14 |
| CRLF vs LF in expected PTY output (`left: "one\r\n"`) | 16 |
| `wsl.exe` probe (not installed here) | 2 |

Representative: `error: there was a problem with the editor 'cp '…''` — tests
use POSIX `cp` as a fake `GIT_EDITOR`. And `pane::tests::*` (PTY round-trip,
`resize_propagates_to_child_via_winsize`, backpressure) fail because they
assume `/bin/sh` and LF; ConPTY emits CRLF.

The CRLF cluster is the only one that reflects a genuine platform behavior
difference worth a product decision (does thegn normalize ConPTY CRLF, or do
the tests accept it?). The rest are test fixtures that need a portable shell
helper.

### W4 — `which_path` ignores `PATHEXT` — **FIXED**

`crates/thegn-core/src/util.rs:140`:

```rust
let p = dir.join(cmd);
if p.is_file() { return Some(...) }
```

On Windows `…\Git\cmd\git` is not a file — `git.exe` is. So `which_path("git")`
always returns `None`. Live effect, with Git 2.54 installed and on PATH:

```
Core dependencies
  git           MISSING — git reads will silently fail; install git
  gh            absent (optional — GitHub PR/issue features degrade)
```

**Blast radius is smaller than it looks.** The load-bearing callers pass an
explicit `.exe` (`util::shell()` → `pwsh.exe`/`powershell.exe`;
`desktop_notify.rs`) and work correctly. Actual git invocation goes through
`Command::new("git")`, which uses `CreateProcess` + `PATHEXT` and is fine —
verified: `thegn wt list` / `wt diff` / `repo list` all exit 0.

So this is **misleading, not breaking**: `thegn doctor` tells a Windows user to
install software they already have. The remaining bare-name callers
(`doctor.rs:701/702`, `sandbox.rs:732` `"devenv"`, `daemon/service.rs:939`
`"cat"`, and the `req.binary` OCI probes) are all Linux-only or diagnostic. It
is still a latent trap for any future probe.

### W5 — `doctor` bypasses the portable `home()` seam — **FIXED**

`crates/thegn-host/src/cmd/doctor.rs:765` uses a raw `std::env::var("HOME")`
and reports `dotfiles (HOME unset — cannot scan)`, even though
`util::home()` correctly resolves `USERPROFILE` on Windows. Same class of bug
as W4: a call site going around the seam that exists for it. Functionally moot
(the `[sandbox.home]` layer is a Linux-container feature), cosmetically wrong.

A sweep found ~50 raw `HOME` reads. Most are legitimate — `sandbox_mounts.rs`
et al. build mount specs targeting a **Linux guest**, where literal `HOME` is
correct. The host-side ones worth review: `account.rs:163`, `startup.rs:34`,
`ssh_creds.rs:37/77`, `build_cache.rs`, `agent_home.rs:20`, `tg-kit/theme.rs:232`.

### W6 — a private `expand_tilde` duplicate silently breaks `file:~/…` secrets — **FIXED**

`crates/thegn-core/src/config.rs:104` defines a **second**, private
`expand_tilde` that reads raw `HOME` and joins with `/`, shadowing the portable
`util::expand_tilde` (which goes through `home()` and works). Its single caller
is `expand_env_ref`, so the scope is precise: a config secrets-ref of the form
`"file:~/.thegn/token"` does not expand on Windows, the read fails, and the
secret **silently resolves to `None`**.

Everything else (`worktrees_dir`, `workspaces_dir`, `repo_roots`, pane `cwd`)
uses the portable helper and is correct — confirmed live:
`repo_roots: C:\Users\blakea\code`.

### W7 — the workspace is not warning-free on Windows — **FIXED**

`add-windows-job-objects`'s proposal states the windows-gnu workspace check is
"warning-free". It is not, on msvc today — exactly one warning, in both check
and release:

```
crates\thegn-host\src\agent_run.rs:34:9: warning: fields `worktree`, `prompt`,
`command_template`, `vars`, and `timeout_secs` are never read
```

`AgentTaskRun`'s fields are read only by the `#[cfg(unix)]` `run()`; the
`#[cfg(not(unix))]` stub ignores them, but the struct itself is ungated. Since
`just lint` is `clippy -D warnings`, this would fail that gate on Windows.

### W8 — CRLF hazard: `core.autocrlf=true` with no `.gitattributes` — **FIXED**

This box has `core.autocrlf=true` set globally, and the repo ships **no
`.gitattributes`**. Observed live while seeding a scratch repo:

```
warning: in the working copy of 'README.md', LF will be replaced by CRLF
```

The working copy audited here is a zip extraction and is still LF, so nothing
broke — but a `git clone` on a default-configured Windows box will rewrite
line endings, which puts at risk: the bundled hooks (`test/git-hooks/*.sh`,
`post-checkout.sh`) that Git Bash executes (CRLF in a shebang → `bad
interpreter`), `merge_guard`'s POSIX-sh hook verification, and any byte-exact
snapshot comparison. Pinning `* text=auto` plus `*.sh eol=lf` would close it.

Related: the repo could not even be *cloned* on Windows until
`store/aux.rs` was renamed (reserved DOS device name). That fix is confirmed
present — the file is now `worktree_aux.rs`, and no reserved names remain.

### W9 — the `wsl` backend is implemented but unreachable — **CORRECTED, see below**

`Backend::Wsl` exists (`sandbox.rs:125`) and is explicitly exempted from the
"decline OCI on native Windows" rule (`sandbox_backend.rs:26,140`). But:

- `config_defaults::default_backend_chain()` is
  `podman-rootless → podman-rootful → docker → apple → bwrap → jobobject → host`
  — **no `wsl`**, so it is never selected by default.
- `thegn doctor`'s backend listing omits `wsl` entirely.
- CONTRIBUTING still describes container sandboxing as absent on Windows,
  which now understates what the code supports.

Three-way drift between code, defaults, and docs.

### W10 — the dev loop and quality gates do not exist on Windows — **PARTLY ADDRESSED**

Documented and expected, but worth stating as the practical cost: `nix`,
`devenv`, and `just` do not apply, so a Windows contributor cannot run
`just lint`, `just test`, `just coverage`, `just smoke`, or `just e2e` locally.
Bare cargo is the whole loop. Two concrete snags:

- `test/smoke.sh` resolves `./target/debug/thegn` and gates on `[[ -x $SZ ]]` —
  on Windows the binary is `thegn.exe`, so it fails before running.
- All muse e2e snapshots are recorded `__linux` (e.g.
  `chrome_regions__chrome/xterm__100x30__linux.txt`). There is no Windows
  baseline set, so `just e2e` has nothing to compare against.

## On-machine checklist status

Mapping the open items in `openspec/changes/add-windows-compositor-validation`
(§2) and `add-windows-parity` (§5.2) to what this audit could establish. Items
needing a human at the keyboard are marked as such.

| Item | Status |
| --- | --- |
| 2.1 waker spike, one tick/s at ~0% CPU | **Not run** — needs an interactive TTY |
| 2.2 first frame renders (chrome + pane) | **PASS** — full frame, exit 0 |
| 2.3 idle CPU ~0% | **FAIL** — 23% of a core, release (W2) |
| 2.4 resize drag-storm | **Not run** — interactive |
| 2.5 Ctrl+C into a pane | **Not run** — interactive |
| 2.6 StderrGuard under a background warn | **Partial** — `thegn-stderr.log` created |
| 2.7 conhost refused with WT pointer | **Logic verified**, unit-tested; not exercised in a real conhost |
| 2.8 `thegn daemon` two-terminal race | **Covered by test** `pipe_bind_is_the_lock_and_round_trips` |
| 2.9 Unicode/border glyphs render | **Partial** — Unicode *selected* (not ASCII fallback); visual confirmation still needed |
| parity 5.2 activity dots | **Not run** — interactive |

Note on why the interactive items could not be closed: the agent harness runs
with stdout redirected and no `WT_SESSION`, so the conhost gate correctly
refuses to launch the TUI. The probes above worked by launching the binary
inside a **real** Windows Terminal via `wt.exe` and reading back
`THEGN_BENCH_FIRST_FRAME_EXIT` / `THEGN_BENCH_RUN_MS` results — which is a
repeatable pattern for automating more of this checklist.

## Startup timing

Launch → first frame, cold, in Windows Terminal (target is <300 ms):

| Build | first frame |
| --- | --- |
| debug | 3499 ms |
| release | 4045 ms |

Both runs include first-run seeding and the setup wizard, so these are *not*
comparable to the Linux warm number and should not be read as a regression on
their own. The waterfall shows where it goes — release:

```
terminal ready       6 ms
session loaded    2126 ms   <- dominant
config loaded     2298 ms
sidebar loaded    3181 ms
pins launched     4034 ms
first frame       4045 ms
```

`session loaded` at ~2.1 s is the item worth a look; it is a DB + git hydration
step, and the 2 s gap is suspiciously close to a timeout rather than work.
Worth re-measuring on a warm, already-seeded state dir before drawing
conclusions.

## Suggested order of work

1. **W1** — gate `sandbox_tests.rs:1225` (or route it through `fsperm`). One
   attribute; unblocks the entire `thegn-core` suite and coverage on Windows.
2. **W7** — gate `AgentTaskRun` so `-D warnings` can pass.
3. **W3** — introduce a portable test helper for shell/`cat`/`cp` fixtures and
   decide the ConPTY CRLF policy. This is the bulk of the 97 failures and is
   mechanical once the helper exists.
4. **W2** — the real engineering. Instrument thread spawns (the `cpu_*_ms`
   counters currently report 0.0 while the OS sees 23%), then eliminate the
   per-tick churn. Needs a Windows-capable profiler, since the built-in one is
   SIGUSR2/unix-only.
5. **W4/W5/W6** — three small seam fixes: `PATHEXT` in `which_path`, route
   `doctor` through `util::home()`, delete the private `expand_tilde`.
6. **W8** — add `.gitattributes` before more Windows contributors clone.
7. **W9** — reconcile the `wsl` backend across default chain, doctor, and docs.

## Corrections to existing docs

`KNOWN_ISSUES.md` and CONTRIBUTING currently say the release build has not
completed and that nobody has run thegn interactively on Windows. Both are now
out of date:

- The release build completes in **18m01s** on this hardware. The CI job's
  90-minute budget is not the constraint; a cold, uncached runner is.
- The compositor has now been launched on Windows and renders correctly. What
  remains unproven is the *interactive* behavior (items 2.4/2.5 above), not
  whether it runs at all.

## Reproducing

```powershell
# prerequisites (once)
rustup-init.exe -y --default-host x86_64-pc-windows-msvc
vs_BuildTools.exe --quiet --wait --norestart --nocache `
  --add Microsoft.VisualStudio.Workload.VCTools `
  --add Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
  --add Microsoft.VisualStudio.Component.Windows11SDK.22621

cargo check --workspace --locked
cargo build --release --locked -p thegn-host

# the Windows-specific kernel-semantics gates
cargo test -p thegn-svc  --lib --locked ipc
cargo test -p thegn-host --locked platform::

# compositor probes — must run inside a real Windows Terminal
$env:THEGN_BENCH_FIRST_FRAME_EXIT=1; thegn.exe   # renders one frame, exits 0
$env:THEGN_BENCH_RUN_MS=90000; $env:THEGN_PERF=1; $env:THEGN_LOG="info"; thegn.exe
```

State lands in `%LOCALAPPDATA%\thegn` (DB, logs) and `%APPDATA%\thegn`
(config); set both plus `THEGN_DIR` to isolate a throwaway instance, which is
the Windows equivalent of `just start`.

---

## What changed

A follow-up pass acted on the findings above. Everything here was verified on
the same machine.

### Measured before / after

| | Before | After |
| --- | --- | --- |
| `cargo check --workspace` warnings | 1 | **0** |
| `thegn-core` tests | **did not compile** | 2238 pass / 22 fail |
| `thegn-host` tests | 1921 pass / 31 fail | 1921 pass / 25 fail / 13 ignored |
| `thegn-svc` tests | 384 pass / 66 fail | 409 pass / 41 fail |
| workspace total passing | ~2383 | **4568** |

### Fixes

- **W1** — `sandbox_tests.rs` now asserts through a new portable seam,
  `fsperm::is_restricted_to_owner` (unix: no group/other bits; Windows: no
  inherited ACEs, read back via `icacls`). `fsperm`'s test module was
  `#[cfg(all(test, unix))]` and so had never run on Windows at all; it is now
  `#[cfg(test)]` with a pure, both-platform `icacls` parser test. This unblocked
  the entire `thegn-core` suite — 2260 tests that had never executed on Windows.
- **W7** — `AgentTaskRun` carries `#[cfg_attr(not(unix), allow(dead_code))]`
  with a comment explaining that only the unix `run()` reads the fields. The
  workspace now checks warning-free on msvc, so `clippy -D warnings` clears the
  compiler-warning bar.
- **W4** — `util::which_path` resolves through `PATHEXT` via a new pure
  `exe_candidates(cmd, pathext)` (unit-tested on both platforms). `thegn doctor`
  no longer reports an installed git as MISSING.
- **W5** — `doctor` routes through `util::home()` instead of raw `$HOME`.
- **W6** — the private `expand_tilde` duplicate in `config.rs` is deleted;
  `expand_env_ref` uses `util::expand_tilde`, so `file:~/…` secrets-refs resolve
  on Windows.
- **W8** — added `.gitattributes` (`* text=auto eol=lf`, LF pinned for `*.sh`
  and the muse snapshots, CRLF for `*.ps1`/`*.cmd`/`*.bat`).
- **W10** — `test/smoke.sh` falls back to `thegn.exe`.

### The biggest single win: test-fixture hermeticity

Most of the `thegn-svc` recovery came from one fix. `git_cmd` in the git test
fixtures neutralized ambient config with `GIT_CONFIG_GLOBAL=/dev/null` — which
is **not a path on Windows**, so the *system* gitconfig stayed in force, and
Git for Windows ships one containing `core.autocrlf = true`. Every byte-exact
content assertion then failed as `"one\r\n" != "one\n"`.

Fixed by a new `util::NULL_DEVICE` (`NUL` on Windows) plus a `pin_lf()` helper
applied at every fixture repo creation — repo-local config wins, so it holds
even for the fixtures that deliberately inherit global config. This took the
`git` cluster from 27 failures to 13, and is a latent bug on **any** platform
where a developer has `autocrlf` set, not just Windows.

### Corrections to the audit's own findings

- **W9 was wrong.** The `wsl` backend is not "implemented but unreachable" — its
  argv builder is labelled *"Aspirational"* in-tree and hands a **Windows** path
  to a Linux container as `--workdir` without translating it, i.e. it has
  exactly the bind-mount bug the "decline OCI on native Windows" rule exists to
  prevent. It is *correctly* absent from the default chain; adding it would have
  shipped a broken sandbox to anyone with WSL installed. Documented in
  `config_defaults.rs` rather than enabled.
- **The rebase-test failures were not a thegn bug.** `git rebase -i` runs its
  sequence editor through Git for Windows' MSYS `sh.exe`, whose emulated
  `fork()` loses races under parallel test load
  (`sh.exe: *** fatal error - add_item (...)`). All 14 pass single-threaded;
  `.config/nextest.toml` now caps them at 2 concurrent on Windows. The
  production `GIT_SEQUENCE_EDITOR='cp <scratch>'` mechanism was verified working
  on Windows (5/5 clean runs) and left alone.
- **Four `git::tests::*` failures are an artifact of this checkout**, which is a
  zip extraction rather than a clone. They call `repo_root()` and need the
  working tree to be a real git repo; they will pass in a normal clone.

### New finding: ConPTY never signals child exit (product bug, partly fixed)

`pane_pty.rs` was built on the unix guarantee *"drop the slave so the master
sees EOF when the child exits"*. ConPTY makes no such promise. A spike
(`crates/thegn-host/examples/conpty_spike.rs`, added) shows the reader thread
still blocked **3 seconds after the child was killed**, and unblocking only when
the master is dropped.

Left alone, a Windows pane whose command finishes never emits
`PaneEvent::Exit` — no "process finished" notification and no reap. Fixed by
giving Windows a dedicated waiter thread that owns the child, calls `wait()`,
and reports the exit; the reader now ends whenever the master is dropped. Unix
is untouched (`cfg`-gated).

The same spike turned up why the six bare-PTY pane tests cannot pass headless:
ConPTY emits `ESC[6n` (a cursor-position query) at startup and **stalls the
child until something answers**. The captured output is nothing but ConPTY's
init sequence — even `cmd /c echo hello` never prints. Real thegn is fine,
because its emulator replies; a bare `openpty` harness has no emulator. Those
six were `#[cfg_attr(windows, ignore = "...")]` at the time this was written;
the DSR responder that unblocks them landed later — see
[The suite goes green on Windows](#the-suite-goes-green-on-windows--48434843).

### Still open

*Superseded — this was the state at the time of writing. W2 is measured and
largely closed in [W2 progress](#w2-progress-idle-cpu-measured), and the test
failures are gone entirely; see
[The suite goes green on Windows](#the-suite-goes-green-on-windows--48434843).*

- **W2, the idle-CPU invariant** — untouched. It needs a Windows-capable
  profiler to attribute the thread churn, and the in-process one is unix-only.
  This is the single thing most worth doing next.
- **Remaining test failures.** The dominant cause — fixtures spawning POSIX
  `sh`, `cat`, `sha256sum` — turned out **not** to need per-fixture code fixes
  at all; see [Declarative dev environment](#declarative-dev-environment-the-nix-develop-analogue)
  below. What is left after that are genuine per-test issues: `host::` tests
  that probe *Linux* host resources (they want `#[cfg(unix)]`), the six
  ConPTY/DSR pane tests, and unix-path assertions in a handful of tests.
- **The interactive checklist** (resize storms, `^C` passthrough, activity dots)
  still needs a human at a keyboard.

### Installer

`install.ps1` is the Windows counterpart to `install.sh`: per-user, no admin.
It builds a release binary, installs `thegn.exe` plus `tg` / `tg-tui` shims to
`%LOCALAPPDATA%\Programs\thegn`, adds that to the user PATH idempotently, and
writes a Start Menu shortcut that launches **through Windows Terminal** — a
plain shortcut would open in conhost, which thegn refuses. It preflight-warns
when Windows Terminal or git is missing. `-DryRun` / `-NoBuild` / `-BinDir`
are supported. Verified here end to end: dry run, install, both shims running
`--version`, and a second run leaving the PATH unchanged.

Note it must stay **pure ASCII** — PowerShell 5.1 reads `.ps1` as ANSI, so a
UTF-8 em-dash in a string breaks parsing at load time.

### Declarative dev environment (the `nix develop` analogue)

The audit's W10 ("no dev loop on Windows") had a much better answer than
rewriting test fixtures one at a time.

**The key observation:** every POSIX tool the failing fixtures spawn — `sh`,
`cat`, `echo`, `printf`, `sleep`, `sha256sum`, `cp`, `wc`, `head`, `stty` — is
**already installed** on any machine that can build thegn, because Git for
Windows ships a full MSYS userland in `<git-root>\usr\bin`. thegn already
hard-requires git, so this was never a missing dependency; it was an
*undeclared* one that simply was not on `PATH`.

Evidence: `calendar::tests` went from **7 failures to 26/26 passing** with that
one directory on `PATH` and **zero** code changes. Most of what looked like a
porting problem was an environment problem.

So the fix is declarative setup rather than per-fixture edits:

| File | Role | Nix analogue |
| --- | --- | --- |
| `rust-toolchain.toml` | channel + `clippy`/`rustfmt`/`llvm-tools`; rustup applies it automatically on every platform | the flake's `rustToolchain` pin |
| `dev/windows.dsc.yaml` | `winget configure` manifest: git, rustup, VS Build Tools (VCTools + SDK), Windows Terminal | `flake.nix` package set |
| `dev/scoop.json` | same set for `scoop import` (minus Build Tools, which Scoop cannot install) | — |
| `devshell.ps1` | session `PATH` + `CARGO_BUILD_JOBS` cap + sccache wiring + cargo dev tools | `nix develop` |

`devshell.ps1` supports `-Command "..."` (like `nix develop --command`) and
`-Check` (report and exit non-zero, changing nothing).

Two constraints worth preserving:

- **The `usr\bin` entry must stay session-scoped.** It also contains `find.exe`
  and `sort.exe`, which shadow the Windows built-ins other tooling depends on.
  The script never writes the User or Machine environment.
- **`rust-toolchain.toml` is inert under `nix develop`** (the flake's pin wins,
  and cargo there is not driven through rustup), so the two must be kept in
  step. It deliberately does **not** list the darwin/windows cross targets:
  rustup would eagerly download every listed `rust-std` set, and that gate is
  Linux-only anyway.

What this does *not* give you: Nix's hermeticity, content-addressed store, or
rollback. Versions drift with upstream unless pinned, and winget pinning is far
weaker than a lockfile. Real Nix semantics still means Nix-in-WSL2, which is not
the native port.

### Incident: lsass crash and forced reboot during a test run

A full `cargo nextest run --workspace` was interrupted by Windows forcing a
restart. Because the suite exercises `TerminateProcess` / `TerminateJobObject`
and thegn's stale-PID reaping, this was investigated as a possible self-inflicted
kill. **It was not.** The record:

```
15:38:28  Application Error 1000   lsass.exe, faulting module RPCRT4.dll,
                                   exception 0xc0000005 (access violation)
15:38:48  Wininit 1015             "A critical system process, lsass.exe,
                                   failed with status code c0000005.
                                   The machine must now be restarted."
15:38:57  shutdown                 (logged as unexpected, EventLog 6008)
```

Why it cannot have been a kill from the suite:

- `0xc0000005` with a WER fault offset in a named module is an **internal
  crash**. A `TerminateProcess` reports the exit code passed to it and produces
  no faulting-module record.
- lsass runs as a **Protected Process Light (level 4)** with Credential Guard
  and VBS Key Isolation enabled (Wininit 12/14/18 at every boot). Not even an
  administrator can open that handle for termination; the agent shell was not
  even elevated.
- Contemporaneous events point at a domain/network trigger: DNS registration
  failures at 15:37:39–42, an Intel Wi-Fi driver warning (`Netwtw14`) at
  15:37:45, and `NETLOGON 5719` unable to reach domain controller `ACCOUNTS`
  after the reboot. lsass crashing inside the RPC runtime on a domain-joined
  laptop during a network transition is a known failure shape.

Honesty about what is *not* proven: this is the only lsass crash in the entire
retained Application log, and it happened during the run. Correlation that
strong should not be waved away, and there was one real coupling — thegn's
`keyring` dependency uses Windows Credential Manager, and every `CredRead` /
`CredWrite` is an **RPC into lsass**. A ~4800-test suite driving that in
parallel is a bad idea regardless of whether it caused this.

**Fix applied** (`crates/thegn-host/src/secret.rs`): a `keyring_disabled()`
guard on all four keyring entry points — `keyring_get`, `keyring_set`,
`keyring_available`, and `forget`'s keyring leg. It is **always** on under
`cfg(test)`, and `THEGN_NO_KEYRING=1` disables it at runtime too.

This is a test-isolation fix that was owed anyway, independent of the crash. The
suite had been reading *and writing* the developer's real credential store:
`keyring_available()` does a live write+delete round-trip, and `iroh_home`
persists a key through `store()`. Tests must never mutate shared, user-owned OS
state. No coverage is lost — `store()` falls back to its `0600` file backend,
which is the leg tests should exercise.

### W2 progress: idle CPU, measured

W2 is no longer "untouched". Two Windows-amplified patterns were found and cut,
and the platform now has an idle-CPU harness of its own
(`test/perf/cpu-sample.ps1` — the shell one is Linux-only by construction, which
is why this regression had no gate here).

All numbers: release build, idle compositor, the **same 14-worktree / 4-dirty
fixture** the Linux harness uses, so they are comparable with its ~0.056 cores.

| | cores (of one) | git spawns / 8s |
| --- | --- | --- |
| Baseline | **0.236** | 42 |
| After Phase 1 (spawn removal) | 0.161 | 23 |
| After Phase 3 (watcher-gated scan) | median 0.140 | ~17 |
| **Final** (+ fsperm fix) | **median 0.105** (min 0.069, max 0.135) | median 20 |

**~56% off idle CPU, ~52% off spawns.** The median now sits under the harness's
0.12-core ceiling, though the max still exceeds it and the < 0.05 target is not
reached. Run-to-run variance is large (±25%) because a third-party security
agent scans every process creation on this box — so single runs prove nothing
and every figure above is a median of repeats.

What was actually wrong:

1. **`resolve_git_path` spawned `rev-parse --git-path` on every call**, including
   local worktrees. The merge/rebase banner probe therefore cost **five**
   subprocesses on a clean repo, and hydration ran it **twice per cycle** — ten
   spawns to answer "is a merge in progress?", almost always "no". The gitdir
   needs no git at all: `<wt>/.git` is either the directory or a
   `gitdir: <path>` pointer (`gitrepository-layout(5)`). New
   `thegn_core::gitdir` resolves it from the filesystem;
   `repo::main_worktree` uses the same route.
2. **The active worktree rescanned unconditionally, forever.**
   `should_rescan_glyphs` returned `true` for it with no staleness check, while
   its fs-watcher already knew the repo was quiet and nothing consumed that.
   It is now watcher-gated with a 30 s safety scan (`THEGN_ACTIVE_SAFETY_MS`).
3. **`bg_glyph_ttl` equalled `model_refresh_interval`** (both 5 s), so
   `age >= ttl` was true at essentially every tick and background rows never
   served from cache — the TTL existed but did nothing. Now 15 s.

Instrumentation that had to be fixed first, because it was reporting zeros:

- **`thread_cpu_ns()` returned a hardcoded `0` off-unix**, so every `cpu_*_ms`
  counter read `0.0` on Windows. That is *why* W2 had to be chased with an
  external sampler. It now uses `GetThreadTimes`. Caveat recorded in the code:
  Windows quantizes thread CPU to the ~15.6 ms scheduler tick, so an individual
  `measure()` sample is coarse; only the rollup aggregate is meaningful.
- **`Subsys::Diff` had no `measure()` call site anywhere** (nor do `Pr`,
  `Issues`, `Lsp`, `Sandbox`), so `cpu_diff_ms` logged `0.0` on *every*
  platform. `Diff` is now wired around the per-worktree git fan-out.
- `benches/support/fixture.rs` hardcoded `GIT_CONFIG_SYSTEM=/dev/null`, so
  `git_hot` — the one bench that measures this exact path — could not run on
  Windows at all. Now `util::NULL_DEVICE`.

### New finding: `restrict_dir_to_owner` orphaned everything inside it

Found while trying to read the perf log back. `fsperm`'s Windows arm ran
`icacls <dir> /inheritance:r /grant:r <user>:F` — a grant with **no inheritance
flags**. Combined with `/inheritance:r` stripping the inherited ACEs, the ACL
applied to the directory object alone, so everything created inside it
afterwards landed with an **empty DACL**:

```
state\thegn        ACCOUNTS\blakea:(F)      <- no (OI)(CI)
state\thegn\logs   <empty>                  <- unreadable by anyone
```

thegn locked itself out of its own `logs/` directory. This is not what the unix
`chmod 0700` it models does — there the mode governs the directory and new files
get the process umask, which is exactly why an asymmetric seam hid it. Fixed by
granting `(OI)(CI)F` for directories, with a regression test that creates a file
*inside* a restricted directory and reads it back.

### Measured: gix vs the git CLI on Windows — and why porcelain v2 is the wrong lever

`crates/thegn-svc/benches/git_hot.rs` A/Bs the per-worktree model scan
(`is_dirty` + `ahead_behind` + `current_branch`) against both providers. It had
never run on Windows because the fixture hardcoded `GIT_CONFIG_SYSTEM=/dev/null`
(fixed in Phase 0). First results on this box:

```
model_scan/gix/1      16.9 ms      model_scan/cli/1     450 ms     27x
model_scan/gix/4      75.6 ms      model_scan/cli/4    1.77 s      23x
model_scan/gix/14    285   ms
gix_ops_14wt/is_dirty 208 ms  (~15 ms per worktree)
```

**A CLI read costs more than an order of magnitude more than the gix equivalent
here**, because each one is a ~40-105 ms process spawn. One spawn costs more
than the entire gix model scan for a worktree.

This inverts the planned "fold four reads into one `status --porcelain=v2
--branch`" optimization. That change *adds* a CLI spawn where gix currently
serves `is_dirty` / `ahead_behind` / `current_branch` in-process. It would be
roughly neutral on Linux (fork+exec is ~1-3 ms) and a clear regression here.

The right direction is the one `openspec/specs/git-backend/spec.md` already
mandates and the code has not finished: **native-first reads**. `status` and
`diff_files` are still explicitly delegated to `CliGit`
(`git/mod.rs:1319-1334`) despite the module header claiming gix covers "the hot
panel-poll path". Those two are the remaining CLI spawns in the scan, and
porting them closes a documented gap rather than trading one spawn for another.

Caveat on `diff --numstat`: porcelain v2 carries no line counts either, so it
could not have replaced those regardless. A gix port needs real blob diffing —
a separate change with its own correctness surface.

### Phase 4: SQLite connection pool

`Db::open()` is called from ~311 sites, ~40 on the event loop. Benched warm
(`cargo bench -p thegn-core --bench core_hot -- db/open_at_warm`):
**2.586 ms per open** on this box — dominated by the file open, which a security
agent scans every time.

`crates/thegn-core/src/db_conn_pool.rs` parks connections keyed by the
**resolved** database path. `Db` now holds `Option<Connection>` and returns it
to the pool on `Drop`, so `Db::open()` keeps its exact signature and all ~311
call sites benefit **without a single edit** — including the
`Db::open().ok().and_then(|db| …)` chains that cannot thread a borrow.

Two deliberate choices:

- **Keyed on the resolved path**, because `db_path()` re-reads
  `XDG_STATE_HOME` / `%LOCALAPPDATA%` on every call and the suite repoints them
  constantly (the `state-db` spec requires test isolation). A test that
  repoints its state dir lands in a different bucket; `open_memory` / `open_at`
  are not pooled at all.
- **The `user_version` check is NOT cached.** A pooled checkout still runs it
  (`Db::verify_schema`), so a migration written by another process is detected
  exactly as before. Only the expensive file open is skipped.

Bounded at 12 idle connections per path (`sched::BG_PERMITS` 8, plus the loop,
ticker, writer and headroom); past that a returned connection is closed, so the
pool never blocks a caller and never grows without bound.

## The suite goes green on Windows — 4843/4843

`cargo nextest run --workspace` now passes on this box with no ignored-on-
Windows escape hatch beyond one documented test. Getting there turned up four
**product** bugs; the test-only fixes are listed after them.

### Product bug: a pane's last output was lost when its child exited

`PaneEvent::Exit` could overtake the child's final output. `child.wait()`
returns the instant the process dies, while its last bytes are still sitting in
the pseudoconsole waiting to be read, so the Windows waiter thread (added
earlier, above) raced the reader. Anything that stops on `Exit` — the drain
helper, and the loop paths that tear a pane down on it — then saw an empty
pane: **a command that printed and quit came out blank.** Timing-dependent, so
it passed alone and failed under load, which is exactly how it hid.

Unix has no such race: there the reader itself sees EOF *after* the final read
and reports the exit from there.

Fixed in `pane_pty.rs`: the reader publishes a byte counter, and the Windows
waiter reports `Exit` only once that counter stops moving (60 ms quiet, 1.5 s
ceiling so a chatty grandchild can never withhold the exit forever). This was
the single change that took the real-pane tests from "flaky under any
parallelism" to a 4-second group.

### Product bug: every spawned pane got an unusable environment

The pane env firewall is clear-then-allowlist, and the allowlist
(`HOST_ENV_ALLOW_EXACT`) was spelled entirely for POSIX. On Windows that meant
a pane started with **no `SystemRoot`, no `Path`, no `PATHEXT`, no
`USERPROFILE`, no `TEMP`** — and `SystemRoot` is load-bearing for Winsock, the
CRT and .NET, so `powershell.exe` in a pane exited instantly without printing
anything.

Worse, `PATH` would not have survived even if it had been the only var needed:
Windows env names are case-insensitive and the OS spells it `Path`, so the
case-sensitive `contains("PATH")` match dropped it on the floor.

Fixed in `util.rs` with `HOST_ENV_ALLOW_EXACT_WINDOWS` (infrastructure only —
the `*_TOKEN`/`*_KEY`/`*_SECRET` families stay firewalled on both platforms)
and a new pure `host_env_allowed(key, windows_host, extra)` that folds ASCII
case on the Windows arm. The platform is a parameter, not a `cfg`, so the table
test covers both arms on every OS.

### Product bug: `output_bounded` was not bounded

`thegn-svc`'s `output_bounded` killed the child at its deadline and then
`join()`ed the pipe readers — but EOF is not the child's to give. The pipe
closes when the LAST writer lets go, and on Windows MSYS `sh` *forks* a
grandchild that inherits stdout rather than exec'ing into it. So
`sh -c "sleep 30"` killed at a 5 s deadline still sat there for the full 30
seconds. (Unix never showed it: `sh` execs into a single command, so the
process being killed IS the one holding the pipe.)

The drains now publish as they read, and the call waits for them on a 2 s leash
rather than joining unconditionally — the same "hand it off and return at the
deadline" call `sandbox::output_with_timeout` already makes about reaping a
wedged probe. The wedged-command test went 30.5 s → 2.6 s.

### Product bug: secret resolvers had no shell to run in

`[secrets.resolvers]` templates are documented as shell commands and ran via a
bare `sh -c`, which does not exist on Windows unless git's `usr/bin` happens to
be on `PATH`. They now resolve through `util::posix_shell()` (the `sh.exe` git
ships), and degrade with a warning if there is no POSIX shell at all — rather
than running a POSIX template through `cmd.exe`.

### Test-only: DB isolation was a silent no-op off unix

`handlers::sidebar_reorder` and `agent_tests` isolated the user DB by setting
`XDG_STATE_HOME` — which `util::xdg_state_home()` does not read on Windows (it
reads `%LOCALAPPDATA%`). Every such test therefore shared the developer's
**real** database, and rows from unrelated tests turned up inside the
sidebar-reorder assertions. Both now go through `testenv::STATE_HOME_VAR`, and
the reorder guard uses `EnvVarGuard` so the prior value is restored rather than
unset (unsetting `LOCALAPPDATA` would strip a real Windows var out from under
every test that follows).

### Test-only: ConPTY children must be reaped explicitly

Nothing in the product leaves a pane dangling, but a test that spawns one and
never closes it does — and a live ConPTY child holds the test binary's
inherited handles open, so the harness waits out its 5-minute cap on a test
whose assertions finished in milliseconds. `testenv::reap_panes` (and a
`live_pid` + `terminate_pid` pair at the two sites that drop a pane directly)
closes that.

### Test-only: the ConPTY DSR handshake, answered

The six bare-PTY tests previously marked `#[cfg_attr(windows, ignore)]` for the
`ESC[6n` stall now run. `drain_until_exit` answers the query — while draining it
*is* the terminal on the other end of the PTY, so it owes the child a cursor
report — and the daemon WS test plays terminal the same way before typing. Also
fixed there: Enter is **CR** on the wire, not LF (a unix pty maps CR→NL via
ICRNL; ConPTY recognises only CR), and the WS test uses `cmd.exe` as its echo
target because MSYS `cat.exe` never echoes under ConPTY.

Net: five of those six now run on Windows, plus three tests that were
`#[cfg(unix)]`. The one still gated is
`panes::tests::toggle_drawer_spawns_and_closes_drawer_pane` — it holds two live
panes and drops one mid-test, and that dropped pane's pseudoconsole outlives
even a terminated child. Its subject is plain table bookkeeping; the
Windows-specific behaviour around it is covered by the two sibling tests that
do run.

### Test-only: two concurrency caps in `.config/nextest.toml`

Both are Windows-only, both for contention rather than logic:

- `conpty-windows` (4 at a time) — each real-pane test costs a `conhost.exe`
  plus a PowerShell child, far heavier than a unix pty.
- `git-subprocess-windows` (2 at a time) now also covers `bridge::tests`,
  `plugin::proc::tests` and `bundle::tests`, which spawn MSYS `sh.exe`
  directly and lose their stdout to the same emulated-`fork()` race the git
  tests hit.

And `activity::tests::concurrent_ack_and_poll_never_lose_ack_or_tear` is now
bounded by wall clock as well as round count: each round is a real
load→mutate→save, ~5 ms on Linux but ~45 ms on a box whose security agent
inspects every temp-file write, so a fixed 2000 rounds meant 10 s vs >3 min —
close enough to the harness cap to read as a hang. The race it hunts is the
interleaving, not the round count.

## Making it good, not just green

The suite passing was not the same as the product working. Every finding below
came from driving the actual shell-invocable surface (`test/smoke.sh`) and the
actual compositor on Windows, and the common thread is that **the test suite
runs from Git Bash, where MSYS's `usr\bin` is on `PATH`** — so anything that
shelled out to `sh` or resolved a bare program name passed in CI and failed for
a real user in PowerShell or Windows Terminal.

### The isolation knob was a no-op, and the "hermetic" test was not

`test/smoke.sh` opens with *"hermetic, non-interactive end-to-end check … in an
isolated HOME"*. On Windows it was neither. It exported `XDG_CONFIG_HOME` /
`XDG_STATE_HOME`, which `util::xdg_config_home()` / `xdg_state_home()` do not
read there — so every check ran against, and **wrote to**, the developer's real
`%APPDATA%\thegn\config.toml` and `%LOCALAPPDATA%\thegn\thegn.db`. Two of its
own checks left `picker = "fzf"` in a daily-driver config.

The same pattern is everywhere in the repo — `just start`, `just bench`, the
e2e per-case env, a dozen justfile recipes — all of them silently non-isolating
on Windows.

Fixed in the product rather than in fifteen harnesses: an explicitly set
`XDG_CONFIG_HOME`/`XDG_STATE_HOME` now wins over `%APPDATA%`/`%LOCALAPPDATA%`.
Nothing on Windows defines those names — not the native environment, not Git
Bash (checked both) — so one being present is always a deliberate instruction,
never an accident. `home()` deliberately still ignores `HOME`, because MSYS
*does* define that, as a POSIX path Win32 cannot open.

Harnesses additionally have to pass **native** values: `cygpath -m` gives the
mixed form (`C:/Users/…`) that bash and Win32 both accept.

### `sh` is not on PATH, and neither is `.cmd`

Two distinct resolution bugs, both invisible from Git Bash:

- **`Command::new("sh")`** — used by the merge-queue `gate_command`,
  `gate_setup_command` and `regenerate_command`, the `[[git_commands]]` custom
  command seam, `[notify] sound_command`, doctor's `which_ok` probe, the
  nix-closure probe, and `sha256_local`. A native Windows session has no `sh`.
  The merge queue's gate could not run at all, and failed the way a failing
  gate looks. Now `util::sh_command` / `util::posix_shell`, which find the
  `sh.exe` Git for Windows ships regardless of `PATH`. Two of those sites did
  not need a shell in the first place: doctor's probe is now a `PATH` walk
  (it reported *every* optional tool as absent on Windows), and `sha256_local`
  hashes in-process with the `sha2` crate the same crate already links.

- **A bare program name never resolves to a `.cmd`.** Neither
  `std::process::Command` nor portable-pty's `CommandBuilder` consults
  `PATHEXT`; both only ever try `<name>.exe`. So every tool that installs as a
  `.cmd` shim — `npm`, `pnpm`, `yarn`, `tsc`, `gh`, and much of what
  `[[agents]]`/`[[tools]]` would launch — came back "program not found". Now
  `util::resolve_program` at the spawn seams. `which_path` had a matching bug:
  it tried the bare name *before* the `PATHEXT` suffixes, and Windows cannot
  execute an extensionless file at all, so a directory holding both `foo` and
  `foo.cmd` resolved to the one that cannot run.

A third bug fell out of testing the first two: `share`'s spawn does
`current_dir(statedir)` unconditionally, but the state dir was only created
when the plan materialized files — so a config-less provider (iroh) chdir'd
into a directory nobody had made. Latent on unix too; it only ever worked
because an `frp` share of the same worktree+port had run first.

### thegn refused to start in most Windows terminals

The startup gate wanted `WT_SESSION`, a known `TERM_PROGRAM`, or a 256-color
`$TERM`. Windows has no `$TERM` convention, so a plain `powershell.exe`, an IDE
terminal, a launcher, or `thegn.exe` double-clicked from Explorer was turned
away with *"legacy conhost.exe is not supported"* — in consoles that render VT
perfectly well. And when it did start, an empty `$TERM` reads as
`ColorDepth::None`, so it drew **monochrome with ASCII box-drawing**.

`platform::console_caps` asks the console instead: whether it accepts
`ENABLE_VIRTUAL_TERMINAL_PROCESSING` (exactly the capability the compositor
needs — true for conhost since Windows 10 1903, Windows Terminal, VS Code,
JetBrains, ConEmu) and whether its output code page is already UTF-8. The gate
refuses only when the console says no *and* the environment offers no evidence;
`termcaps::apply_console_caps` lifts color to truecolor and, when the code page
can carry it, glyphs to full Unicode.

Measured, in a real console with `TERM`/`COLORTERM`/`TERM_PROGRAM`/`WT_SESSION`
all stripped: **monochrome + ASCII → truecolor**. Pinned by
`pty_launch::a_bare_windows_console_still_resolves_truecolor`.

The code page is read, never set. Forcing `CP_UTF8` would make Unicode chrome
safe everywhere, but it is console-wide state that outlives the process — thegn
would silently re-encode the shell it was launched from.

### The compositor had no Windows coverage at all

`test/pty-smoke.sh` — the only thing that launches the real compositor — drives
`script(1)`, and its missing-tool guard is a *skip*. On Windows it printed
`skip PTY smoke: script(1) not found`, exited 0, and asserted nothing.

`crates/thegn-host/tests/pty_launch.rs` replaces it with portable-pty (ConPTY on
Windows), answers the ConPTY DSR handshake, and requires a *readable frame*
rather than just a zero exit — at a normal geometry and at 40×8, where the
chrome cannot have everything it wants. It rides `cargo nextest`, so it runs on
every platform without a POSIX shell.

Relatedly, `test/perf/cpu-sample.ps1` no longer has to open a Windows Terminal
window to measure anything; a hidden console works now.

### Path separators

`Path::join("thegn/config.toml")` renders `…\Roaming\thegn/config.toml` on
Windows. Harmless to the API, but it is what `thegn doctor` prints and what a
user copies. The user-visible ones (config, DB, logs, gate, profile, worktree
excludes, git config) now join one segment at a time. Roughly a hundred remain
in remote/POSIX contexts, where a forward slash is correct.

### Idle CPU, re-measured

`test/perf/cpu-sample.ps1`, release, 14-worktree fixture, three runs:
**0.0952 / 0.1139 / 0.1037 cores** (median 0.104), 10–14 `git` spawns per 8s
window. Under the harness's own 0.12 ceiling.

Not directly comparable to the earlier 0.088: that run redirected stdout to a
file, so it was not rendering to a terminal at all. These leave stdout on a
(hidden) console, so the number now includes real frame output — a more honest
figure that happens to be slightly higher. Run-to-run spread stays ±25% on this
box, where a security agent inspects every process creation.

Still roughly 2× the Linux reference (~0.056 on the same fixture). Closing that
is the remaining git-spawn work (porcelain-v2 status, the watcher-gated active
scan), which is its own change.

### What is still open on Windows

- **The interactive checklist.** Resize storms, `^C` passthrough, activity dots,
  and how the chrome actually *looks* in Windows Terminal — a human at a
  keyboard. `pty_launch` proves a frame renders and the caps resolve; it does
  not prove the frame is right.
- **Mouse reporting** resolves to `no` in a bare console, because detection is
  still env-based for that field. Whether termwiz's Windows input path delivers
  SGR mouse events was not verified, so the conservative value stands.
- **`curl --unix-socket`** has no named-pipe equivalent, so two smoke checks
  (open a session over the control socket, then snapshot it) are skipped there.
  The same pipeline is covered in-process by
  `daemon::service::tests::ws_warm_attach_pipeline_over_a_real_socket`.
- **`sqlite3` is not on PATH**, so two forward-compat DB checks skip.
- **~100 multi-segment `Path::join`s** remain in remote/POSIX contexts, where a
  forward slash is correct, plus test fixtures.

### Corrections to the dev-environment docs

`dev/windows.dsc.yaml` and `dev/scoop.json` both declared Windows Terminal as
*"a hard requirement to run what you build"* because thegn refused conhost.
That is no longer true — it stays in the declared set because it is what gets
full Unicode chrome (its output code page is UTF-8) and undercurl, not because
nothing else launches.

`test/dev-tui-plan.sh` inspects a `just` recipe, and `just` is deliberately not
part of the Windows dev environment (the recipes are bash; bare `cargo` is the
documented native loop). It died with a bare exit 127 and no output; it now
says so and exits 0. `test/pty-smoke.sh`'s skip message names the Rust test
that replaced it, so the skip reads as a duplicate rather than a hole.

### Mouse was off in Windows Terminal

Found while answering "which terminal should I use". `detect()` decides `mouse`
from `$TERM` alone — empty or `dumb` means "reports mouse poorly, don't ask".
`$TERM` is empty in **every** native Windows shell, Windows Terminal included
(it does not set it), so the most common Windows setup ran with mouse reporting
switched off in a terminal that handles SGR 1006 perfectly. Colour and Unicode
were rescued by the `WT_SESSION` branch; mouse had no such rescue.

`apply_console_caps` now enables it whenever the console takes VT processing —
a console that accepts VT output accepts mouse input. Pinned by
`console_caps_tests::a_vt_console_reports_mouse`.

(Undercurl and synchronized output need no equivalent: `detect()` already
credits Windows Terminal for both via `WT_SESSION`.)

## The interactive checklist, finally run — in WezTerm

Four symptoms, reported from a real session: panes crashing in a loop, missing
fonts, no colour, no mouse, "a basic PTY view compared to mac/linux". All four
were real, all four are fixed, and none of them were visible from the test
suite. `dev/wezterm-debug.ps1` is the harness that found them — it captures the
terminal's own environment, what the console reports about itself, `thegn
doctor`, and a `THEGN_LOG=debug` compositor run, from *inside* the terminal
under test. None of that is observable from a redirected shell, which is why
this class of bug survived a green suite.

### Panes crash-looped because a POSIX snippet was fed to PowerShell

The log said it outright:

```
ERROR thegn::pty_drain  sandbox pane kept crashing; not respawning
  tail=+ ... if ... devenv shell -- sh -lc "$sel" && exit;
       FullyQualifiedErrorId : MissingOpenParenthesisInIfStatement
```

The chain is: `available_probe` reports `jobobject` as **Present** on Windows
(reasoning that the Job Object API is part of the OS) → a `SandboxSpec` exists →
`compose_spec` sets `in_oci = sb.spec.is_some()` → the pane runs the OCI
login-probe snippet → `enter_argv` for the Windows-native backends re-invokes it
through `util::shell()`, i.e. PowerShell → parse error → exit 1 → respawn →
repeat until the crash-loop guard gives up.

Three separate faults, fixed at each level:

- **`available_probe` answers the wrong question.** It is not "does the OS have
  this API", it is "does selecting this backend contain a pane" — and nothing on
  the pane spawn path assigns the child to a Job Object or an AppContainer.
  `enter_argv`'s own comment admits it ("we could intercept and wrap in a job
  object"). Reporting `Present` also let `doctor` advertise a containment
  boundary that is never applied, which is a security claim, not a cosmetic one.
  Now `Absent`, so the chain falls through to `host` — the honest state, and the
  same degradation a Linux box without podman already gets. `doctor` now says
  `selected host`.
- **`in_oci` conflated "has a spec" with "inner is POSIX".** New
  `Backend::inner_is_posix()`; the Windows-native pair is the only `false`.
- **`shell_inner(false)` was POSIX on every platform** — `${SHELL:-/bin/sh} -l`
  is a syntax error in PowerShell, not a fallback. It now names the resolved
  shell through the call operator, so a spaced install path
  (`C:\Program Files\PowerShell\…`) still invokes.

Two smaller ones fell out: the host-fallback arm hardcoded `-lc` (producing
`powershell -lc <cmd>`), and `sandbox_wrap_shell` would have re-run an
interactive shell through `powershell -NoProfile -Command` — non-interactive and
profile-less, which is a large part of what "a basic PTY view" meant.

### Missing fonts: a modern terminal demoted by a locale it never sets

`detect_unicode` gated on `LANG`/`LC_*` being UTF-8, with `WT_SESSION` as the
single escape hatch. Those are a POSIX convention that **no Windows terminal
sets**, so every modern emulator on Windows except Windows Terminal fell through
to `UnicodeLevel::Ascii` — WezTerm, which advertises `TERM_PROGRAM=WezTerm` and
`COLORTERM=truecolor`, drew `+ - |` box art and no chrome glyphs.

Terminal identity now beats an absent locale: a terminal that names itself as
one of `MODERN_TERMS` is UTF-8 by construction (kitty, WezTerm, ghostty, foot
have no non-UTF-8 mode), and `LANG` selects a *libc* locale that says nothing
about what the emulator renders. This also corrects the unix case — the old rule
demoted kitty under `LANG=C`.

### No colour was mostly my own contamination

Worth recording as a methodology note. The first capture reported
`color monochrome`, and `NO_COLOR=1` was in the captured environment — but at
User and Machine scope `NO_COLOR` is unset. It came from the shell that launched
WezTerm, which was Claude Code's own, and Claude Code sets `NO_COLOR`. Re-run
with a clean environment, WezTerm resolves **truecolor**. The harness now
strips it explicitly. A capture is only as trustworthy as the environment it
inherited.

### Verified in WezTerm, after

```
color         truecolor (24-bit)
glyphs        full (Unicode + wide glyphs)
undercurl     yes
mouse         yes
osc52 copy    yes
sync output   yes
```

and one pane spawn, no respawns, no errors in the debug log.

### Round two in WezTerm: the panes were mis-wrapped, and "no colour" was NO_COLOR

A screenshot from a real session showed the chrome and glyphs correct but the
first pane's PowerShell banner duplicated and wrapping at the wrong column, and
the whole UI monochrome.

**The mangled pane was a ConPTY reflow.** A `pane resize` trace answered it in
one line:

```
INFO  thegn::startup  pty panes spawned  panes=1
DEBUG thegn::pane     pane resize  from_rows=20 from_cols=65 to_rows=18 to_cols=63
```

`materialize_with_specs` spawned every leaf at the whole `center` rect;
`relayout` then corrected each one to its framed content rect — the 2-row,
2-column border inset — about a second later. On unix that first resize is
nearly free: the pty raises SIGWINCH and the child decides whether to redraw.
ConPTY does not work that way — it reflows its own buffer and *repaints*, so a
shell that had already printed its banner at 65 columns had it re-wrapped and
re-emitted over the old text. Hence one banner drawn twice, at two widths.

`layout_framed` already computes the per-leaf rect; the spawn simply was not
using it. Now it is, the resize is a no-op, and the trace shows **zero** pane
resizes at startup. (Cross-platform win too: one fewer SIGWINCH per pane.)

**"No colour" was `NO_COLOR=1`,** inherited from the shell that launched
WezTerm — Claude Code sets it for every process it spawns, and it survives into
any terminal started from one of those shells. `doctor` had the answer
(`NO_COLOR yes`, `degraded: no color`) three sections above the verdict line,
where nobody looks after concluding the theme is broken. Two changes: the
verdict now reads `no color (NO_COLOR is set)`, and `dev/wezterm-debug.ps1`
warns and strips it, because a debug harness that faithfully reproduces the
wrong environment is worse than none. My own first capture made the same
mistake and I nearly diagnosed a bug that did not exist.

### The statusbar memory icon was not in real Nerd Fonts

WezTerm popped a modal on every launch:

> Font problem — No fonts contain glyphs for these codepoints: `\u{efc5}`

`\u{efc5}` was `[stats] mem_icon`. Its nine siblings are all classic Font
Awesome (`nf-fa-*`, U+F000–U+F2FF), which is in every Nerd Font build ever
shipped; U+EFC5 comes from a set added later, so a font that rendered CPU,
net, GPU, temp, swap, freq, load, uptime, battery and disk still drew a
placeholder box for memory. It was also the only icon in the list with no
`nf-fa-*` comment naming it — the outlier was visible in the source the whole
time.

Now `nf-fa-server` (U+F233). `cpu_icon` stays on Octicons deliberately and is
commented as such: Font Awesome 4 has no CPU glyph and `nf-fa-microchip` is
already the GPU icon, so it is the one remaining icon a pre-v3 Nerd Font can
miss.

## W2 closed: the git-spawn work, measured end to end

The idle-CPU number never explained *what* thegn was spawning, so the first
change was a ledger: every git subprocess funnels through
`git::output_bounded_with`, which now logs its argv and wall time under
`THEGN_LOG=thegn::git=debug`. Free when off, and it made the problem obvious in
one capture.

### Baseline: 159 spawns in 30 idle seconds

```
   21   3910 ms   186 ms  status --porcelain=v1 -z --no-renames
   21   3693 ms   176 ms  -c core.quotePath=false diff --numstat HEAD
   22   2631 ms   120 ms  diff --numstat HEAD          <- the same answer, twice
   21   2518 ms   120 ms  rev-parse -q --verify CHERRY_PICK_HEAD
   21   2506 ms   119 ms  stash list --format=%h
   21   2390 ms   114 ms  rev-parse -q --verify REVERT_HEAD
   21   2346 ms   112 ms  rev-parse -q --verify MERGE_HEAD
```

Seven spawns per refresh cycle, ~950ms of process creation, on a ~1.4s cycle.
For scale: a bare `git rev-parse --git-common-dir` — a command that does no
work — measures **176ms** on this box, and `git status` 246ms. The cost is
process creation, not git. That is also why the panel's `thread::scope` fan-out
does not help on Windows: parallel spawns do not parallelise when a security
agent inspects every process creation, so seven "concurrent" reads serialise
into the sum.

### The finding that reframed it: the worktree was not a repository

`C:\Users\blakea` — thegn's own default home workspace — is not a git repo, and
`git` says so immediately. thegn was firing seven subprocesses at it every
cycle, each failing with "not a git repository" after paying full
process-creation cost. Every fresh Windows user gets this workspace.

`gitdir::local_git_dir` now **ascends** the way git's own discovery does, so it
answers for any path inside a worktree rather than only at its root — and its
`None` became a real answer ("no repository contains this path") instead of "I
could not tell". `is_git_worktree` is the stat-only predicate on top, and both
`build_panel` and `glyph_reads` short-circuit on it to the same defaults those
failing subprocesses already degraded to.

### The two that mattered for real repos

- **`diff --numstat HEAD` ran twice per cycle** for the same worktree — once
  from `build_panel` via `diff_files`, once from `glyph_reads` for its line
  totals. They now share a memoized read (`numstat`, 1s TTL — long enough to
  collapse the two same-cycle readers, deliberately shorter than a refresh
  cycle so a fresh cycle always re-reads).
- **`stash list --format=%h` is a file read.** `git stash list` IS
  `git log -g refs/stash`: the stash is a reflog, one line per entry at
  `<common gitdir>/logs/refs/stash`. Counting lines gives the same number
  without a process. (Remote/provider locs keep the subprocess — their gitdir
  is on another machine.)

### Measured, same 30s idle window

| | before | after |
|---|---|---|
| git spawns (home workspace, not a repo) | 159 | **0** |
| `build_model_ms` p50 | 2182 | **242** |
| `build_model_ms` p90 | 5393 | **342** |
| `build_model_ms` max | 20447 | **414** |

And on the 14-worktree **real-repo** perf fixture, median of three runs:
**0.104 → 0.087 cores** idle, 10–14 → 9–12 spawns per 8s. A smaller win, as it
should be — those worktrees are real repositories, so only the numstat dedupe
and the stash read apply there.

The `p50 2.2s → 0.24s` model build is the one a user feels: the refresh ticker
fires about every 1.4s, so at 2.2s per build thegn had a git fan-out in flight
roughly 76% of the time on an idle repo, and every log line appeared twice
because builds overlapped.

## Sandbox, part 1: OCI containers work on Windows now

Windows had no sandbox at all — `pick_backend` refused every OCI backend there,
and the reason was one invariant: thegn bind-mounts the worktree **at its real
path**, so host git and container git agree by construction, and
`C:\Users\you\wt` is not a path a Linux container can have.

The invariant turned out to be stronger than it needed to be. `Mount` already
carries `host` and `dest` separately, and everything inside the container —
`--workdir`, the pane's `cd`, `THEGN_WORKTREE` — is composed from `dest`. The
path does not have to be the *same*, only **deterministic**.

`sandbox::container_path` supplies that: identity on unix (the contract is
unchanged there, byte for byte), and on Windows a mapping into the same
`/mnt/<drive>/…` tree WSL itself uses — chosen so a user who shells into the
podman machine sees the path they already expect rather than a thegn invention.
It handles the `\?\` verbatim prefix `canonicalize` returns and UNC paths
(`\server\share` → `/mnt/unc/server/share`), and it is pure, so both platforms'
arms are table-tested from the Linux coverage gate.

With that, `podman-rootless`, `podman-rootful` and `docker` are ordinary
candidates on Windows. A Windows `podman.exe`/`docker.exe` (Podman Desktop,
Docker Desktop, Rancher) translates the *host* half of `-v` itself and reaches
the same WSL2 machine directly — no `wsl.exe --` hop, no guessing a distro.
`thegn doctor` on a box without them now says:

```
podman-rootless  not installed   ↳ install `podman`
docker           not installed   ↳ install `docker`
host             ready
selected         host
```

which is the honest, actionable state — rather than "unsupported", which told a
Windows user their platform was excluded.

`Backend::Wsl` stays out of the default chain, but the reason changed: its argv
is correct now that translation exists, it is simply redundant when the Desktop
CLIs reach the same machine more directly. Opt into it explicitly for a
particular distro's runtime.

## Sandbox, part 2: the AppContainer spike

`crates/thegn-host/examples/appcontainer_spike.rs` — run it with
`cargo run -p thegn-host --example appcontainer_spike`.

### The design question

thegn cannot ask portable-pty to spawn into an AppContainer: the ConPTY spawn
owns the `STARTUPINFOEX` attribute list (it must set
`PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`) and does not expose it, so there is
nowhere to add `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`. The way around is
a **trampoline**: thegn spawns a small helper into the ConPTY normally, and the
helper re-launches the real program with the AppContainer attribute list,
inheriting its own already-console-attached std handles.

### What the spike established

| | result |
|---|---|
| Create / derive / **reuse** an AppContainer profile | works |
| Spawn into it (`STARTUPINFOEX` + `SECURITY_CAPABILITIES`) | works |
| Child inherits the trampoline's std handles | works |
| Read + write a directory granted to the container SID | works, and writes are visible on the host |
| Read a file **not** granted, in the same parent dir | **denied** — the boundary is real |
| Execute a binary from a granted directory | works |

So the trampoline is viable, and AppContainer is a genuine boundary rather than
a label. Notably it needs **no path translation** — same filesystem, weaker
token — so unlike the OCI path it satisfies thegn's mount contract for free.

### The finding that decides the design

`git-on-PATH: UNAVAILABLE`, and the ACLs say why:

```
C:\Windows\System32\cmd.exe   APPLICATION PACKAGE AUTHORITY\ALL APPLICATION PACKAGES:(RX)
C:\Program Files\Git          (no APPLICATION PACKAGES ACE at all)
C:\Users\<you>\.cargo\bin     (no APPLICATION PACKAGES ACE at all)
```

Windows pre-grants AppContainer access to System32 and the UWP world and
nothing else. `cmd.exe` ran for exactly that reason; **the entire developer
toolchain is invisible inside the container by default** — git, cargo, rustup,
node, and the user's shell if it is not in System32.

That is the real cost of this approach, and it is not a bug to be fixed but a
policy to be designed: an AppContainer pane only works if thegn grants the
container SID read+execute on every toolchain directory the pane needs.
`%USERPROFILE%`-owned directories (`.cargo\bin`, `.rustup`) can be granted
without elevation; `C:\Program Files\Git` cannot. Granting
`ALL APPLICATION PACKAGES` instead would weaken those directories for *every*
AppContainer app on the machine, which is not thegn's call to make.

### A trap worth recording

An earlier revision of the probe reported `EXEC from granted dir: DENIED` and
sent this straight down an access-control rabbit hole. It was a quoting bug: an
inner `"C:\path\x.exe"` inside `cmd.exe /c "…"` breaks the outer quoting, so the
`||` fired on a **parse** error that is indistinguishable from a denial. The
direct spawn — where a failure is `CreateProcessW`'s and carries a real error
code — showed execution working fine. The probe now carries a comment saying
not to nest quotes there.

### Verdict

The mechanism works. Before building it into the pane path, the toolchain-ACL
policy has to be settled: which directories thegn grants, whether it does so
per-worktree container or one shared `thegn` container, how it behaves when a
needed directory requires elevation, and how `thegn doctor` reports a pane whose
toolchain is only partly reachable. Shipping the plumbing before that decision
would produce panes that start and then cannot run `git`.
