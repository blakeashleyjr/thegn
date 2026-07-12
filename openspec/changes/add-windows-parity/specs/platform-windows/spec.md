# Platform: native Windows

## ADDED Requirements

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
