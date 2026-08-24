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

Features whose substrate is inherently unix — the merge-queue headless agent
(POSIX `sh_quote` templating), the SIGUSR2 profiler, and `thegn debug`
exec-replace — SHALL return an explicit error (or logged warning, for
best-effort paths) on Windows rather than silently no-op or panic. The pane
daemon, control client, and the profile singleton lock are NOT in this set:
the daemon IPC runs over named pipes and the singleton lock uses std's
cross-platform `File::try_lock`.

#### Scenario: Merge-queue headless agent on Windows

- **WHEN** a merge-queue drain would dispatch the headless conflict agent
  (`agent_command`) on native Windows
- **THEN** the dispatch returns an explicit error naming the missing POSIX
  shell substrate rather than silently no-opping, and the branch is left for
  a human

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
- **THEN** terminating the slot's `GroupHandle` kills the runner _and_ every
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

Backend selection MAY pick an OCI runtime (podman/docker/smol) on native Windows
when — and only when — both halves of the old blocker are addressed:

1. **Mount destinations** are mapped deterministically. `Mount` carries `host`
   and `dest` separately, and every destination MUST be produced by
   `sandbox::container_path`, which maps `C:\…` into the `/mnt/<drive>/…` tree a
   WSL-backed machine already exposes. This applies to the worktree, the
   git-common dir, the host-toolchain and cache mounts, the OCI `--workdir`, the
   preflight probe, and the bind-source comparison used to decide whether a
   running container still matches its spec. A destination left as a Windows path
   emits `-v C:\…:C:\…`, which the runtime rejects, and the container never
   starts.
2. **Git metadata** resolves under that mapping. Every thegn tab is a *linked*
   worktree whose `.git` is a pointer file carrying an absolute host path, so the
   sandbox MUST be given rewritten `.git` and `gitdir` pointers
   (`sandbox_gitshim`). Without them git inside the container reports
   `not a git repository: (null)`.

A sandbox in which in-worktree `git` cannot resolve its own gitdir SHALL NOT be
selected. Should either half regress, selection MUST decline the OCI runtimes on
native Windows again and name the actual reason.

#### Scenario: Docker Desktop installed

- **WHEN** backend `auto` resolves on native Windows with `docker` on PATH and
  answering
- **THEN** docker is selected, and `git status` inside the pane reports the same
  HEAD the host does

#### Scenario: Preflight and status probes use the mapped path

- **WHEN** thegn composes the OCI preflight `exec` or verifies a running
  container's binds
- **THEN** both are expressed in the runtime's namespace via `container_path`, so
  the probe's `--workdir` exists and a correct container is never force-recreated

#### Scenario: A worktree whose metadata cannot be mapped

- **WHEN** a worktree's git metadata cannot be made to resolve under the mapped
  destination
- **THEN** the OCI runtime is not selected for it, and the decline names the
  git-metadata reason rather than a mount-path one

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

### Requirement: A backend that applies no containment MUST probe Absent

A backend SHALL report itself available only if selecting it actually applies the
containment it names. `jobobject` and `appcontainer` MUST probe `Absent` for as
long as pane spawn does not assign the pane's process to a Job Object or an
AppContainer: reporting a boundary that is never applied is a false security
claim, and a "present" backend additionally produces a `SandboxSpec` that routes
the pane through a POSIX composer it cannot satisfy.

#### Scenario: doctor on a bare Windows box

- **WHEN** `thegn doctor` runs on native Windows with no container runtime
  installed
- **THEN** `jobobject` is reported as not available, `host` is the selected
  backend, and the "no kernel boundary" caveat is shown

### Requirement: Windows panes can be confined by an AppContainer

thegn SHALL offer a native Windows containment backend that runs a pane's shell
under an AppContainer token: its own container SID, deny-by-default access to the
filesystem and registry, and network reachable only through capability SIDs. It
MUST require no VM, no image, and no path translation.

The backend's identity SHALL be deterministic per worktree, so that creation and
teardown agree without a lookup, and MUST fit the 64-character limit Windows
imposes on a profile name without allowing two worktrees to collide.

The pane's network policy SHALL map to capability SIDs, where "no network" is the
absence of any capability rather than a flag.

#### Scenario: A contained pane runs the worktree's shell

- **WHEN** a pane resolves to the AppContainer backend
- **THEN** its shell starts under the container token and can read and write the
  pane's pseudoconsole

#### Scenario: Two worktrees never share a container

- **WHEN** two worktrees under one repository resolve to the AppContainer backend
- **THEN** they receive different profiles, even when their paths share a long
  prefix and the full name would exceed the length limit

#### Scenario: The backend may now report itself available

- **WHEN** `appcontainer` probes on a Windows build where pane spawn does
  assign the pane's process to an AppContainer
- **THEN** it reports `Present`, satisfying rather than contradicting the
  earlier requirement that a backend applying no containment probe `Absent` —
  that requirement is conditional on the containment not being applied, and this
  change is what makes the condition false. `jobobject` stays `Absent`: it is a
  limits layer beneath this backend, not a boundary of its own.


### Requirement: The container token is applied through a trampoline

Because the pane's ConPTY spawn already owns its process-thread attribute list,
the security-capabilities attribute cannot be attached to it. The pane SHALL
therefore be launched through a thegn subcommand that re-launches the real shell
under the container token, inheriting the console it was given.

That indirection MUST remain visible in the launch argv, so the truth check can
confirm the containment rather than assume it.

#### Scenario: The trampoline is present in a contained pane's argv

- **WHEN** the AppContainer backend composes a pane's argv
- **THEN** the argv names the trampoline subcommand and the worktree's profile,
  and the truth check reads it as the AppContainer backend


### Requirement: Grants are attempted, never forced, and always reported

Deny-by-default means a pane cannot reach its own worktree or its toolchain until
the container SID is granted access. thegn SHALL attempt those grants and SHALL
NOT elevate to force one through.

A grant that fails for the **worktree** MUST be fatal for that backend — a pane
that cannot read its own files is not a sandboxed pane — and selection MUST fall
through to the next backend rather than start it. A grant that fails for a
**toolchain** MUST be surfaced as a warning naming the directory, the consequence,
and the exact command the user can run themselves.

#### Scenario: An unreachable toolchain is reported, not hidden

- **WHEN** thegn cannot grant the container access to a toolchain directory
- **THEN** the pane still starts and a warning names the directory and the command
  that would fix it

#### Scenario: An unreachable worktree falls through

- **WHEN** thegn cannot grant the container access to the worktree
- **THEN** the AppContainer backend is not used for that pane and selection
  continues down the chain
