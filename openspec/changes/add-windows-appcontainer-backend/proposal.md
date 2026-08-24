# Add the Windows AppContainer sandbox backend

## Summary

Windows had two sandbox options and no middle: an OCI container through
Podman/Docker Desktop (a WSL2 VM, a runtime install, path translation and a git
metadata shim), or nothing. `Backend::WinAppContainer` existed as an enum
variant that was never applied — it probed `Absent` precisely because selecting
it did not contain anything.

This makes it real. An AppContainer is the native peer of `bwrap`: the process
runs under a token carrying its own container SID, denied the filesystem,
registry and object namespace by default, and reaching the network only through
capability SIDs. No VM, no image, and — because it is the same filesystem seen
through a weaker token — **no path translation**, which is what made the OCI
route so involved on Windows.

## The mechanism, and why it needed proving first

portable-pty owns the `STARTUPINFOEX` attribute list for a pane's ConPTY spawn
(it must set `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`) and does not share it, so
there is nowhere to add `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`. The
contained shell therefore cannot be the process portable-pty starts; it has to be
its child, via a **trampoline** (`thegn appcontainer-exec`).

That put the whole design on one unverified assumption: a console is reached
through `\Device\ConDrv`, and an AppContainer token is denied most of the object
namespace. `examples/appcontainer_conpty_spike.rs` measured it before any of this
was written — a contained grandchild both writes the ConPTY and reads a keystroke
from it, while being denied a file its uncontained sibling reads.

## Honesty

- A new `IsolationClass::OsAccessControl`, **below** `SharedKernel`. An
  AppContainer constrains what a process may *ask for*, not what the kernel will
  *execute*; every syscall still runs in the host kernel. Reporting it as
  shared-kernel would describe namespaces and cgroups it does not have.
- `jobobject` moves to `HostProcess` for the same reason: a Job Object bounds
  lifetime and resources and is not a security boundary. It keeps probing
  `Absent`.
- `sandbox_truth::observed` reads the trampoline out of the argv, so AppContainer
  is **verified** rather than taken at its word — the exemption that used to cover
  both win-native backends now covers only `jobobject`.

## Grant what we can, report the rest

Deny-by-default cuts both ways: a pane that cannot read `git.exe` is not
sandboxed, it is broken. `C:\Program Files\Git` carries no
`ALL APPLICATION PACKAGES` ACE (System32 does, which is why `cmd.exe` works
untouched). thegn grants what it can with `icacls` and never elevates to force
one through — an ACL change on `C:\Program Files\…` is the machine owner's call,
not a side effect of opening a pane. An unreachable toolchain becomes a warning
carrying the exact command; an unreachable **worktree** is fatal and the pane
falls through to the next backend.

## Impact

- **tasks.md AX 737 / 733** — Windows gains a native containment backend.
- Specs: `sandbox` (the new isolation class), `platform-windows` (the backend,
  the trampoline, and the grant policy).
- No behaviour change off Windows: the backend's probe is `cfg!(windows)`-gated,
  so a Linux or macOS chain never considers it.
