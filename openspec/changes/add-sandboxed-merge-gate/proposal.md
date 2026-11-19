# Sandboxed merge gate

## Why

The merge queue's `gate_command` runs the lane's own code (justfile, build
scripts, tests) on the host, so a `merge` autonomy land has always stopped for
owner approval. THE-2200 lets the gate run in a sealed sandbox so a land can
proceed unattended when the owner opts in.

## What Changes

- New `[merge_queue] gate_sandbox = "off" | "sealed"` (default `off`, the host
  gate). Honoured only from global or trusted-profile config, never a repo overlay.
- With `sealed`, `gate_setup_command`, `gate_command` and `regenerate_command`
  run through the repo's sealed sandbox: local bwrap only, network none, an
  allowlisted environment, writable only the gate checkout and its target dir,
  the git common dir read-only.
- A sandbox that is refused, or fails to start, is an infrastructure hold
  (`GateVerdict::Error`), never branch blame and never a host run.
- A sealed gate that needs the network or the Nix daemon goes red with a log
  headed "sandboxed gate (network: none)" and the land asks for approval.
- The fold (merge-tree, commit-tree, CAS advance) stays thegn's own git on the host.
- An unattended drain never hands a red gate or conflict to the fixer agent.

## Impact

- **Roadmap**: item 768 (overlaps 763 sandbox programme, WS02-P5-08, and THE-2200).
- Affected specs: `merge-queue`, `agent-orchestration`.
- The W2 freeze on `sandbox*.rs` is respected: the gate builds its spec from host code.

## Unverified

Nothing here has run live. See the unticked task in `tasks.md`.
