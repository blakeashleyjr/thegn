# Add native Windows support, phase 5: feature parity

## Summary

The compositor, daemon, and process substrate are Windows-native (phases 1–4);
this change closes the remaining feature gaps so a Windows thegn *feels* like
a Linux thegn:

- **Activity dots**: the `/proc` scanner gets a Windows twin — sysinfo
  (PEB-read per-process cwd + accumulated CPU time) behind the same
  `scan_proc` contract. Units are milliseconds instead of jiffies, which is
  fine: the activity state machine only diffs successive samples. sysinfo is
  target-gated into thegn-core for Windows only (Linux keeps the direct
  /proc reader).
- **Secret-file permissions**: new `thegn_core::fsperm`
  (`restrict_to_owner` / `restrict_dir_to_owner`) — `chmod 0600/0700` on
  unix, owner-only DACL via `icacls /inheritance:r /grant:r <user>:F` on
  Windows (subprocess over a page of unsafe `SetNamedSecurityInfoW`,
  matching the repo's subprocess-fallback philosophy; these are best-effort
  fallback files — keyring/Credential Manager is the primary store). Rewires
  the token file+dir (host `secret.rs`), share credentials, and VPN keys.
- **Sandbox backend policy on Windows**: `pick_backend` declines OCI
  runtimes on native Windows *even when Docker/Podman Desktop is installed* —
  their Linux containers live in a WSL2 VM that cannot bind-mount the
  worktree at its real absolute path (git worktree metadata carries host
  paths), which would silently break the sandbox contract. The warning names
  the reason and points at WSL2. The default `backend_chain` gains
  `"jobobject"` before `"host"` (probes Absent on unix), so Windows panes are
  scoped by kill-on-close Job Objects rather than silently uncontained.
  Pane-child job *resource limits* stay deferred until on-machine validation
  shows orphans/need (ConPTY already scopes the pane tree).
- **Desktop toasts**: a Windows arm for the notification dispatcher — WinRT
  toast via PowerShell (pwsh → powershell, never cmd), same best-effort
  subprocess pattern as `notify-send`/`osascript`.

merge_guard's POSIX-sh hook verification (Git for Windows runs hooks through
its bundled bash) joins the on-machine checklist from phase 4.

## Impact

- tasks.md AX 729/730/732 annotated; the parity work closes the group's
  code-side scope.
- Crates: `thegn-core` (activity windows scanner, fsperm, sandbox_backend
  policy, default chain + example), `thegn-svc` (share/vpn rewires),
  `thegn-host` (secret.rs, desktop_notify windows arm).
