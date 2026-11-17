# Add a Vercel Sandbox provider

## Summary

Register `provider = "vercel"` as a remote sandbox kind. It follows the sprites
shape (HTTP control plane, a thegn self-bridge for exec, no vendor CLI) with
fly's spend safety (intent-before-create ledger, account fingerprint, lifetime
reaper). Vercel has no SSH, so exec rides the hidden `thegn vercel-exec` bridge.

Credentials never enter the VM: the repo goes in as a host-built git bundle,
no git token is forwarded, and the agent-config, dotfiles, atuin and
home-closure steps are stripped. Results come back through `thegn env pull`.

## Approach

- A session has a hard timeout, so only a live `vercel-exec` bridge extends it.
  Nothing at idle, in the UI loop, or on a timer elsewhere.
- Reads (capture mode) never resume a stopped sandbox.
- Lifetime ceiling stops the session first and deletes after a grace period, so
  un-pulled lane work survives while compute spend goes to zero.
- Capabilities: no checkpoints, no host_cache, no reverse tunnels, no ssh.

## Impact

- THE-2223 (register the kind and spine stubs) and the batch 154 lanes.
- Affected specs: `sandbox` (remote providers).
