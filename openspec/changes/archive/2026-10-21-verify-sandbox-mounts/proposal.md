# Verify sandbox bind mounts, and report macOS isolation honestly

## Summary

thegn bind-mounts a worktree at its own absolute path (`-v <wt>:<wt>`). That is
load-bearing: a linked worktree's `.git` is a file holding an absolute `gitdir:`
pointer, so the tree must appear at its host path or git inside the container
breaks. The existing spec states the invariant ("Worktree stays on the host and
is bind-mounted") but **nothing checked that the bind actually delivered files**.

On macOS there is no Linux kernel, so every OCI runtime runs the container inside
a **Linux VM**, and `-v` is resolved _inside that VM_ — which only sees the host
directories it was told to share. Three defects follow, all verified on macOS 26
against real runtimes:

1. **A refused bind was reported as a mystery.** podman 5.8.6 fails the create
   loudly (`Error: statfs /opt/x: no such file or directory`, exit 125, no
   container), but `ensure` discarded the runtime's stderr and reported
   `could not start podman container '<name>'`. The one line naming the cause
   never reached the user.

2. **An unshared bind could also succeed silently.** docker via colima 29.5.2
   creates the container and mounts an **empty directory**. Every check thegn had
   then agreed: `container_status` compares `spec.mounts[].host` — the strings
   thegn asked for — against `.Mounts[].Source`, which the runtime echoes back
   unchanged, so it compares a request to a copy of itself; and the preflight
   probe ran `/bin/sh -lc true` with `--workdir <worktree>`, which the empty
   directory satisfies. The pane opened on an empty worktree while thegn reported
   real containment.

3. **Host toolchain injection crossed an ABI boundary.** An earlier change gated
   `host_toolchain_mounts` on host/guest ABI, but two Nix injections bypassed the
   gate: the Tier-B nix-daemon socket and the devenv `/nix` bind. Since podman
   machine shares only `/Users`, `/private` and `/var/folders`, binding `/nix`
   failed the create — so **the podman backend could not start a single container
   on a Nix-managed Mac**, and fell through the chain to a host shell.

Two runtimes, two different failure modes, so both a create-time diagnosis and an
in-container probe are required; neither alone covers both.

Separately, `capabilities.rs` classified podman/docker as `SharedKernel`
unconditionally. On macOS they sit behind a VM, so relative to the Mac the
boundary is a guest kernel — thegn **under**-reported its own isolation, the
opposite of every other defect here.

## Approach

- **Verify from inside.** A new pure module `sandbox_mountcheck` derives sentinel
  paths that must exist in the container and rides the preflight probe that
  already runs once per launch, so the check is free. The safety property that
  makes it un-regressable: **only ever assert a path just observed on the host**.
  When nothing is provable — `file_access = none`, a non-local placement, a
  compose-with-service spec, or a path the host lacks — the probe body stays the
  literal `true` it was before, byte for byte.

- **Remove the container on a verified failure.** A container's binds are fixed
  at create and `container_status` will call it healthy forever, so detecting
  without removing would leave the user widening a share, relaunching, and
  hitting the same empty directory.

- **Keep the create's stderr** and route a recognized refused-bind through the
  same remedy matrix, skipping the `--userns keep-id` retry — that retry varies
  the user namespace, which is not what was rejected, so it only rediscovers the
  next unshared path before failing generically anyway.

- **One honest ABI predicate** (`guest_shares_host_abi`) applied at all three
  host-injection sites, replacing the ad-hoc expression that covered only one.

- **Remedies are runtime- and OS-specific**, and keyed on the _missing_ path's
  share root rather than the worktree's, so a main repo on an external volume
  with a linked worktree under `$HOME` names `/Volumes` and not `/Users`.

Not doing: asserting _every_ mount. podman machine does not share `/nix`, so that
would fail the backend for every Nix-on-Mac user — trading one silent lie for a
louder regression.

## Impact

- `tasks.md` group AX (macOS parity) — sandbox correctness on darwin.
- Affected specs: `sandbox` (bind verification), `platform-macos` (the ABI gate
  widened to all three injection sites, and honest isolation reporting).
- Affected code: `thegn-core/src/sandbox_mountcheck.rs` (new),
  `sandbox.rs`, `sandbox_preflight.rs`, `capabilities.rs`,
  `thegn-host/src/agent.rs`, `thegn-host/src/cmd/doctor.rs`.
- No Linux behavior change: the ABI predicate is true on Linux, the isolation arm
  is macOS-only, and the probe is inert wherever a failure is not provable.
