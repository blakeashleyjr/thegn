# Platform: macOS

## Purpose

thegn runs natively on macOS (aarch64-apple-darwin): the compositor drives any
terminal on the platform, panes ride the same `portable-pty` seam as elsewhere,
and every Linux-substrate feature either has a Darwin-native twin or is
**declined visibly** rather than degrading in silence. The port is seam-based —
`crates/thegn-host/src/platform/{proc,qos}.rs` for syscalls,
`thegn_core::termcaps` for terminal capability, `thegn_core::sandbox_backend`
for the OS gate on container backends — so platform code never spreads inline
through call sites.

The governing rule, learned the expensive way: **macOS diverges from Linux most
dangerously where the two look identical.** A `container` CLI that takes
docker's flags but not its nouns, a bind mount that succeeds while shadowing the
guest's own binaries, a `nice` wrapper that is detected but unreachable, a
gitignore matcher whose paths silently stop matching — each of these presented
as "works" and behaved as "broken". Where a mechanism cannot apply, the system
SHALL report what it observed, never what it selected.

## Requirements

### Requirement: Container backends receive their own runtime's dialect

An OCI backend's argv SHALL be built from that runtime's actual command
vocabulary, not from a docker/podman template. Apple's `container` puts image
operations under an `image` noun, has no `container` noun for `inspect`, accepts
no Go templates, and rejects `--security-opt` and `--pids-limit` outright.

#### Scenario: Image probe and pull use the runtime's verbs

- **WHEN** the `apple` backend prefetches its image
- **THEN** it runs `container image inspect` and `container image pull`, not
  `container image exists` / `container pull`, both of which exit 64 (EX_USAGE)
  and would fail the backend out of the chain on every launch

#### Scenario: Unsupported hardening flags are dropped and reported

- **WHEN** a hardened profile resolves onto a backend whose `run` rejects
  `--security-opt` / `--pids-limit`
- **THEN** those flags are omitted so the container can start, **and** the
  narrowing is surfaced in the pane warnings rather than silently applied

### Requirement: Host-toolchain mounts require a matching guest ABI

The host-toolchain substrate (`/usr`, `/bin`, `/lib`, `/nix/store`, `$HOME`
dotfiles) SHALL only be injected into a container whose guest shares the host's
ABI. An OCI guest is always Linux; on a non-Linux host those paths hold foreign
binaries.

#### Scenario: A Linux guest on a macOS host gets no host toolchain

- **WHEN** a spec is built for an OCI backend on macOS
- **THEN** no host `/usr`, `/bin`, `/lib` or `/nix/store` bind is emitted —
  mounting them shadows the guest's own binaries, producing "failed to find
  target executable" and "Exec format error" at container start

#### Scenario: A Linux host is unaffected

- **WHEN** the same spec is built on Linux
- **THEN** the toolchain mounts are injected exactly as before, because host and
  guest are the same system

### Requirement: Container health is verified in the runtime's own language

A create SHALL be confirmed by a probe the runtime actually understands, and a
probe that cannot parse its answer MUST read as "not running" rather than as
healthy.

#### Scenario: Apple's JSON inspect replaces the Go-template probe

- **WHEN** thegn verifies an `apple` container reached RUNNING
- **THEN** it runs `container inspect <name>` and parses the JSON `status.state`
  and `configuration.mounts[].source`, because `container container inspect` is
  not a command and `--format` is an unknown option

#### Scenario: Unparseable output is not success

- **WHEN** the probe's output is empty or not JSON
- **THEN** the container reads as not-running, so the caller recreates rather
  than execing into something that may not exist

### Requirement: Process introspection uses the libproc seam

Per-process cwd, argv, child lists and CPU time on macOS SHALL come from
`proc_pidinfo` / `proc_listallpids` / `KERN_PROCARGS2` behind the platform seam,
not from a whole-process-table refresh.

#### Scenario: The activity scanner does not enumerate the process table

- **WHEN** the activity scan runs (up to 1 Hz)
- **THEN** it issues one `proc_listallpids` plus one cwd probe per pid, and a
  CPU probe only for pids whose cwd matched a worktree — not sysinfo's ~5
  syscalls and an `ARG_MAX`-sized allocation per process

#### Scenario: Mach absolute units are converted, not assumed

- **WHEN** a process's CPU time is read from `proc_taskinfo`
- **THEN** it is scaled by `mach_timebase_info` before conversion to jiffies,
  because the fields are mach absolute units (125/3 on Apple silicon) and
  reading them as nanoseconds understates CPU by ~41×, leaving every worktree
  permanently idle

### Requirement: Off-loop work declares a scheduler quality-of-service

Threads that do not serve the render/input loop SHALL declare a QoS class via
`platform::qos`, so the OS can schedule them on efficiency cores.

#### Scenario: The loop and its workers are classified

- **WHEN** the compositor starts
- **THEN** the event loop declares `Interactive`, model hydration declares
  `Utility`, and the samplers, refresh ticker and fs-watch builder declare
  `Background`

#### Scenario: A no-op elsewhere

- **WHEN** the same code runs off macOS
- **THEN** the call compiles to nothing, because Linux's analogues are process-
  or cgroup-scoped rather than per-thread

### Requirement: Capability enforcement is reported as observed, not as probed

Where a mechanism is detected but cannot reach the thing it would govern, thegn
SHALL report that it does not apply.

#### Scenario: CPU capping on macOS

- **WHEN** `thegn doctor` reports the CPU cap on macOS
- **THEN** it says the mechanism does not apply, because `cap_prefix` only wraps
  `bwrap` (Linux-only) or a local `Backend::None` (which never produces a spec),
  even though `nice` is on PATH and the probe therefore selects `NiceSoft`

#### Scenario: Linux is unchanged

- **WHEN** the same report runs on Linux with cgroup delegation
- **THEN** it names the real mechanism, because there it genuinely applies

### Requirement: The fs-watcher filter matches the paths FSEvents delivers

Watch roots and the gitignore matcher SHALL be canonicalized, because FSEvents
resolves symlinks and reports fully resolved paths.

#### Scenario: A symlinked worktree prefix still filters build churn

- **WHEN** a worktree lives under `/tmp`, `/var/folders`, or a user symlink
- **THEN** writes under `target/` are still dropped, rather than escaping the
  filter — or, worse, reaching `matched_path_or_any_parents`, which **panics**
  on a path outside its root and takes the watcher thread down with it

#### Scenario: An out-of-root path degrades, never panics

- **WHEN** an event path falls outside the matcher's root
- **THEN** it is treated as an ordinary edit, because a filter must not be able
  to kill the thread that feeds it

### Requirement: Terminal capabilities are detected, probed, and reported consistently

`thegn doctor` and the compositor SHALL resolve the same capabilities for the
same terminal, and version-gated capabilities MUST be gated on a verified
threshold.

#### Scenario: doctor runs the same probe the compositor does

- **WHEN** `thegn doctor` runs on a tty
- **THEN** it performs the DA/XTVERSION probe and reports the probe-refined
  capabilities, so it cannot contradict what the compositor renders over
  ssh/tmux

#### Scenario: Terminal.app's truecolor gate is colour-only

- **WHEN** Terminal.app reports a `TERM_PROGRAM_VERSION` at or above the
  verified floor
- **THEN** only `ColorDepth` is upgraded — glyph level, undercurl and
  synchronised output stay off, because Terminal.app has none of them

### Requirement: Silent degradations are visible in doctor

Every macOS integration that degrades by absence SHALL be reported.

#### Scenario: The macOS section names what is missing

- **WHEN** `thegn doctor` runs on macOS
- **THEN** it reports the Option-as-Meta setting for the detected terminal,
  `RLIMIT_NOFILE` against `kern.maxfilesperproc`, whether `$TMPDIR` can shorten
  the pane-daemon socket, and the presence of `osascript`, `afplay`, `pbcopy`,
  `fc-list` and `mediaremote-adapter`

#### Scenario: Unavailable metrics hide rather than lie

- **WHEN** a metric backend cannot produce a reading on this hardware
- **THEN** it is not selected, and the widget hides — the same outcome as having
  no such hardware, never a row of zeroes

### Requirement: Font enumeration and application match the platform

Font discovery SHALL search where macOS actually resolves fonts, and font
application SHALL target the terminal that is running.

#### Scenario: Font directories are searched recursively

- **WHEN** fontconfig is absent (stock macOS)
- **THEN** the macOS font directories are walked recursively, so
  `/System/Library/Fonts/Supplemental` and nix-darwin's
  `Nix Fonts/<hash>-<pkg>/share/fonts/...` are found

#### Scenario: An unsupported terminal is declined with instructions

- **WHEN** the font picker runs under WezTerm, Terminal.app or iTerm2
- **THEN** thegn declines and prints the exact setting to change, rather than
  editing a config the running terminal does not read

### Requirement: The pane-daemon socket fits Darwin's sun_path

The daemon socket path SHALL respect macOS's 104-byte `sun_path` limit, four
bytes tighter than Linux's 108.

#### Scenario: The socket is relocated under a vetted TMPDIR

- **WHEN** the natural socket path would overflow
- **THEN** it is relocated under `$TMPDIR` if and only if that directory is
  owner-owned and not group/world-accessible, because `$TMPDIR` is
  attacker-settable and `[serve] local_admin` trusts socket peers

#### Scenario: No usable short directory is reported, not hidden

- **WHEN** `$TMPDIR` is unset or fails the ownership check
- **THEN** `thegn doctor` says so, because the failure otherwise manifests only
  as panes silently running in-process
