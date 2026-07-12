# Platform: native Windows

## Purpose

thegn runs natively on Windows (x86_64-pc-windows-msvc, no WSL): the
compositor targets Windows Terminal, the daemon speaks named pipes, process
trees are scoped by kill-on-close Job Objects, and every unix-substrate
feature either has a Windows-native twin or fails with an explicit,
actionable error. The port is seam-based — `thegn_core::shellinv` for shell
dialects, `thegn_svc::ipc` for daemon IPC, `thegn-host`'s `platform` module
for syscalls — so platform code never spreads inline through call sites.

## Requirements

### Requirement: The workspace compiles for Windows targets

The cargo workspace SHALL compile (`cargo check --workspace`) for
`x86_64-pc-windows-gnu` and `x86_64-pc-windows-msvc` with no unix behavior
change. Unix-only dependencies (`nix`, `libc`) MUST be target-gated, and
OS-conditional syscalls MUST live behind the host `platform` seam
(`crates/thegn-host/src/platform/`) rather than inline `#[cfg]` blocks at call
sites.

#### Scenario: Linux-side cross-check gates regressions

- **WHEN** `just check-cross` runs on a PR
- **THEN** `cargo check --workspace --target x86_64-pc-windows-gnu` passes,
  catching any newly introduced ungated unix API use

#### Scenario: msvc truth gate

- **WHEN** the `windows` CI job runs
- **THEN** `cargo check --workspace --locked` passes on `windows-latest` with a
  bare rustup toolchain (no nix)

### Requirement: Local shell invocations use the platform shell dialect

Argvs that hand a command string to the **local** user shell (pins, tool
drawer, custom actions, pane-run, editor-open) SHALL be built by
`thegn_core::shellinv`, which maps POSIX shells to `-c`/`-lc`, PowerShell to
`-NoProfile -Command`, and cmd.exe to `/C`. Call sites targeting a remote or
sandboxed Linux substrate SHALL keep literal `sh -lc` and carry an annotation
saying so.

#### Scenario: A pin command on Windows

- **WHEN** a pin with a bare `command` launches on a host whose shell resolves
  to `pwsh.exe`
- **THEN** the spawn argv is `[pwsh.exe, -NoProfile, -Command, <command>]`
  (no `-lc`, no `exec` prefix)

#### Scenario: A pin command on unix is unchanged

- **WHEN** the same pin launches on a unix host
- **THEN** the argv is `[$SHELL, -lc, exec <command>]` exactly as before

### Requirement: Unix-substrate features stub with explicit errors on Windows

Features whose substrate is inherently unix — the sealed-sandbox model relay
(its consumers are Linux containers that bind-mount the socket), the
merge-queue headless agent (POSIX `sh_quote` templating), the SIGUSR2
profiler, `thegn debug` exec-replace, and the ACP unix-socket transport —
SHALL return an explicit error (or logged warning, for best-effort paths) on
Windows rather than silently no-op or panic. The pane daemon, control client,
and the profile singleton lock are NOT in this set: the daemon IPC runs over
named pipes and the singleton lock uses std's cross-platform `File::try_lock`.

#### Scenario: Sealed-sandbox relay on Windows

- **WHEN** a sealed-agent launch asks for the model relay on native Windows
- **THEN** relay spawn returns an `Unsupported` error naming Linux containers
  as the missing substrate, and the caller surfaces it

#### Scenario: Singleton detection on Windows

- **WHEN** a second `thegn` launches for a profile whose compositor is live on
  native Windows
- **THEN** `instance_running` reports the live instance via the held file
  lock, the same as on unix

### Requirement: Process control routes through the platform seam

Pid liveness probes, best-effort termination, grouped spawns and tree kills,
stderr redirection, and shutdown-signal installation SHALL go through
`thegn-host`'s `platform` module. Grouped spawns use one shape on both
platforms — `spawn_grouped` returns the child plus a cloneable `GroupHandle` —
where unix keeps today's pgid semantics (`setpgid` + `killpg(SIGTERM)`) and
Windows assigns the child to a kill-on-close Job Object
(`TerminateJobObject` for explicit kills). On Windows, dropping the last
`GroupHandle` MUST also reap the tree (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`),
and a failed job assignment MUST degrade to direct-child termination rather
than failing the spawn. Termination on Windows is hard (no SIGTERM window);
call sites that rely on child-side cleanup are cancel-and-discard paths.

#### Scenario: Superseded test run is reaped whole

- **WHEN** a newer test run supersedes an in-flight `cargo test` (or its
  watchdog deadline passes) on native Windows
- **THEN** terminating the slot's `GroupHandle` kills the runner *and* every
  test binary it spawned, immediately

#### Scenario: Host death leaves no orphans

- **WHEN** the thegn process dies while a grouped child tree is running on
  native Windows
- **THEN** the job's kernel handles close with the process and the whole tree
  is reaped by KILL_ON_JOB_CLOSE

#### Scenario: Compositor shutdown signal on Windows

- **WHEN** the console window closes or Ctrl+C is delivered to the compositor
- **THEN** the shutdown flag is set and the terminal waker is pulsed — the same
  contract as SIGTERM/SIGHUP on unix — so session state persists before exit

### Requirement: Daemon IPC rides one endpoint seam on both platforms

Local daemon IPC (the pane daemon's listener, the control client's requests,
and the warm-attach WebSocket) SHALL go through `thegn_svc::ipc`: unix-domain
sockets on unix, named pipes on Windows. The pipe name MUST be derived
deterministically from the per-state-dir socket path
(`\\.\pipe\thegn-<hex(sha256(path))[..16]>`) so per-`$XDG_STATE_HOME` daemon
isolation is preserved, and a stored `\\.\pipe\…` endpoint string MUST be
recognized as-is by classification (discovery round-trips with no schema
change).

#### Scenario: Daemon serves over a named pipe

- **WHEN** `thegn daemon` starts on native Windows
- **THEN** it binds `\\.\pipe\thegn-…` derived from its state dir, registers
  that name as its `DaemonRow.endpoint`, and control-client verbs and
  daemon-backed pane attaches connect through it

#### Scenario: Pipe names isolate state dirs

- **WHEN** two daemons start with different `XDG_STATE_HOME`s (e.g. a dev
  instance under `just start` beside the daily driver)
- **THEN** their pipe names differ and neither sees the other as
  "already running"

### Requirement: The IPC endpoint is the single-daemon lock on both platforms

`bind_exclusive` SHALL preserve the daemon's bind-race semantics everywhere:
whoever binds the endpoint is the daemon; a second binder learns
`AlreadyRunning` and exits 0. On unix this keeps the connect-probe +
stale-file unlink + `AddrInUse` mapping; on Windows the first pipe instance is
the lock (`ACCESS_DENIED` for the loser), created with
`reject_remote_clients`, and a dead daemon's pipe vanishes with its process
(no stale-endpoint recovery needed).

#### Scenario: Spawn race on Windows

- **WHEN** two `thegn daemon` processes race to start for the same state dir
- **THEN** exactly one owns the pipe and serves; the other observes
  `AlreadyRunning` and exits 0, and clients connect to the winner

### Requirement: The compositor targets Windows Terminal and refuses conhost

On native Windows the compositor SHALL start only when the environment shows
evidence of a modern terminal (`WT_SESSION`, a known-modern
`$TERM`/`$TERM_PROGRAM`, an explicit truecolor advertisement, or a 256-color
`$TERM`). Legacy conhost.exe MUST be refused at startup with an error naming
Windows Terminal — degrading silently into broken rendering is not an option.
Under Windows Terminal, capability detection MUST resolve Full Unicode,
undercurl, and synchronized output without POSIX locale variables.

#### Scenario: Launch inside Windows Terminal

- **WHEN** `thegn` starts with `WT_SESSION` set and no `LANG`/`LC_*`
- **THEN** the compositor runs with truecolor + Full Unicode glyphs +
  undercurl + DECSET-2026 sync, not the ASCII/basic fallback

#### Scenario: Launch inside bare conhost

- **WHEN** `thegn` starts on Windows with no modern-terminal evidence
- **THEN** it exits with an error pointing at Windows Terminal instead of
  rendering degraded chrome

### Requirement: Pane shells resolve and invoke by platform dialect

New-pane shells SHALL resolve platform-natively: `$SHELL`/probe-chain on unix,
pwsh → powershell → `%COMSPEC%` on Windows — never a hardcoded `/bin/sh` on a
host that lacks it. Shell argv construction SHALL apply POSIX interactive/login
flags (`-i`/`-l`) only to POSIX-flavored shells; PowerShell and cmd.exe get a
bare argv.

#### Scenario: New tab on Windows

- **WHEN** a worktree tab opens its default pane on native Windows with pwsh
  installed
- **THEN** the pane spawns `pwsh.exe` with no arguments (no `-i`, no `-l`)
  under ConPTY

### Requirement: Display-path basenames are separator-agnostic

Anywhere a display name is derived from a filesystem-absolute path (tab
titles, sidebar/search labels, overlays, toasts, share labels, provider
inference) the derivation SHALL treat `/` and `\` as separators (via
`util::basename`), and provider inference SHALL strip a trailing `.exe`.
Git-relative paths (which git emits with `/` on every platform) keep plain
`'/'` handling.

#### Scenario: Windows worktree title

- **WHEN** a worktree at `C:\Users\u\worktrees\feature-x` is shown in the tab
  bar or search labels
- **THEN** the displayed leaf is `feature-x`, not the full backslashed path

### Requirement: Activity tracking works on Windows

The per-worktree activity scan (`cpu_jiffies_by_path`) SHALL return real
samples on native Windows — per-process cwd matched longest-prefix against
worktree paths, summing a monotonically increasing per-process CPU counter —
so the sidebar activity dots behave as on Linux. Processes whose cwd is
unreadable (elevated/protected) are skipped silently, mirroring unreadable
`/proc` entries.

#### Scenario: Busy pane lights the dot

- **WHEN** a build runs inside a pane whose cwd is under a managed worktree on
  native Windows
- **THEN** successive activity scans attribute growing CPU to that worktree
  and its sidebar dot goes busy, then quiet after the configured cooldown

### Requirement: Secret files are owner-only on every platform

Secret-file fallbacks (provider token files and their directory, share
credentials, VPN keys) SHALL be restricted to the owning user everywhere:
mode 0600/0700 on unix and an owner-only DACL (inheritance stripped, only the
current user granted) on Windows. Failures are best-effort — the OS keyring /
Credential Manager remains the primary store.

#### Scenario: Token file on Windows

- **WHEN** a provider token falls back to a file write on native Windows
- **THEN** the file's ACL grants access only to the current user (no
  inherited ACEs)

### Requirement: Container backends are declined on native Windows with the reason

Backend selection SHALL NOT pick an OCI runtime (podman/docker/smol) on
native Windows even when its CLI is installed and answering: Docker/Podman
Desktop containers are Linux VMs that cannot bind-mount the worktree at its
real absolute path, which violates the sandbox contract. The decline warning
MUST name that reason and point at WSL2; the chain then selects `jobobject`
(kill-on-close Job Object scoping) ahead of the bare host shell.

#### Scenario: Docker Desktop installed

- **WHEN** backend `auto` resolves on native Windows with `docker` on PATH
- **THEN** docker is skipped with the same-path/WSL2 warning and the
  `jobobject` backend is selected

### Requirement: Desktop notifications deliver on Windows

The desktop-notification dispatcher SHALL deliver toasts on native Windows
(WinRT toast via PowerShell), best-effort with null stdio on the dedicated
dispatcher thread — the same degradation contract as `notify-send` on Linux
and `osascript` on macOS: a missing/failed notifier never disturbs the
session, and the in-app inbox still records everything.

#### Scenario: Agent finishes on Windows

- **WHEN** an agent-done event meets the configured urgency threshold on
  native Windows
- **THEN** a toast titled with the event appears via the WinRT notifier, and
  a PowerShell-less system simply skips delivery
