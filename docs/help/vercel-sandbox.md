---
id: vercel-sandbox
title: Vercel sandboxes
parent: sandboxing
---

# Vercel sandboxes

An environment with `provider = "vercel"` runs a worktree in a Vercel Sandbox
microVM. thegn drives it through Vercel's HTTP API and a hidden `thegn
vercel-exec` bridge; there is no SSH.

## Setup

```toml
[env.cloud.provider]
provider = "vercel"
team_id = "team_abc123"
project_id = "prj_abc123"
api_key_env = "env:VERCEL_TOKEN"
size = "2"
```

- `api_key_env` is a secret reference (keyring, file or env). It is resolved at
  call time and sent only to Vercel's API. It never reaches the VM.
- `team_id` and `project_id` are required and are not secrets.
- `max_instances` defaults to 2; `max_lifetime_secs` defaults to 24 hours.

## What works

- Terminal panes (an interactive PTY) and headless agent lanes.
- The repo is copied in as a git bundle. Nothing is cloned inside the VM.
- A stopped sandbox keeps its filesystem and resumes when you open a pane (resume behaviour is unverified).

## What does not

- SSH, mosh and `connect = "ssh"`.
- `host_cache` and reverse tunnels.
- Checkpoints.
- Credentials: API keys, agent configs, dotfiles and git tokens are never sent
  to the VM. No credential of any kind is sent into the sandbox, so a
  model-backed agent cannot log in there yet; only scripted or non-model work
  runs until the owner decides how to supply a model credential.

If the interactive PTY is refused, panes fall back to a stream mode without
resize or full-screen programs.

## What a live run needs

- A Vercel access token scoped to the team, kept as a keyring or 0600 file
  SecretRef, never in config.
- The team id.
- The project id. Sandboxes bill to it; Hobby caps sessions at 45 minutes and
  4 vCPU, so Pro is preferred.
- An explicit spend authorization.

## Unverified

Not yet checked on a live account: whether the interactive PTY WebSocket works as implemented (hence the stream fallback), the sandbox-name and tag charsets, upload and output size limits, 429 behaviour, whether deleting a sandbox also removes its snapshots, whether Nix works in the image, and the token scopes required. The kind is experimental (`thegn doctor` says so).

## Cost

Vercel bills usage. A stopped sandbox costs no compute or memory, but its snapshot storage still bills (thegn does not estimate that or data transfer). A session is created
with a 30 minute timeout and is extended only while a thegn bridge is running,
never at idle. Cost shown in thegn is an upper-bound estimate.

## Teardown and results

At the lifetime ceiling thegn stops the sandbox and deletes it seven days later,
or at once when its worktree is gone. Pull work back first:

```sh
thegn env pull
```

This fetches the bundle into `refs/thegn/pull/<worktree>`; only the lane's own
branch fast-forwards. See [[sandboxing]].
