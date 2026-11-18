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
- Credentials by default: API keys, agent configs, dotfiles and git tokens are
  never sent to the VM. See "Model credentials" below for the one opt-in.

If the interactive PTY is refused, panes fall back to a stream mode without
resize or full-screen programs.

## What a live run needs

- A Vercel access token scoped to the team, kept as a keyring or 0600 file
  SecretRef, never in config.
- The team id.
- The project id. Sandboxes bill to it; Hobby caps sessions at 45 minutes and
  4 vCPU, so Pro is preferred.
- An explicit spend authorization.

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

## Model credentials

One key, per environment, default `none`:

```toml
[env.cloud.provider]
model_credentials = "none"   # "none" | "upload" | "proxy" | "inject"
```

Only your own config file can set it. The env wizard never writes a value other
than `none`, `env create` has no flag for it, and a repo-local overlay cannot
set it. It applies only to `provider = "vercel"`; on other providers it is
refused (use `[sandbox.home] agents` there).

| Mode     | What leaves the machine                                                                           |
| -------- | ------------------------------------------------------------------------------------------------- |
| `none`   | nothing: no model credential is sent; model-backed agents cannot log in                           |
| `upload` | your login file(s) are copied into the sandbox's memory (tmpfs) at each terminal open             |
| `proxy`  | refused: not implemented                                                                          |
| `inject` | refused: would hand the model key to Vercel's firewall (the key is stored by Vercel); not offered |

`thegn doctor` prints the mode in effect and what leaves the machine; the env wizard only notes the default `none`.

### Upload

- Only claude and codex. claude: the host `.credentials.json` plus a
  synthesized `.claude.json` holding only `{"hasCompletedOnboarding":true}`.
  codex: `auth.json` only. Nothing else is sent: no account trees, settings,
  hooks, MCP config, git tokens, ssh keys or dotfiles.
- Files go to `/dev/shm/thegn-login-<nonce>/<agent>/` (dir 0700, files 0600,
  checked with `stat`). If `/dev/shm` is not tmpfs, thegn refuses; there is no
  disk fallback. Agents find it through `CLAUDE_CONFIG_DIR` or `CODEX_HOME`.
- Memory only: a stop, suspend or delete destroys it, and every terminal open
  re-uploads if it is missing (unverified that the persistent auto snapshot excludes it; see Unverified).
- Only in a terminal pane or a headless session, which are explicit user
  actions. Never in capture mode, the reaper, provisioning, doctor, fleet or
  any read.
- Before the first byte is written, the sandbox egress policy is narrowed to
  the model hosts plus the env's own `[sandbox.network]` allow domains (plus
  `registry.npmjs.org` when agents are installed). If the policy cannot be
  applied, nothing is uploaded. An allow-all policy is replaced in upload mode.
- The env is resolved from the sandbox's owner worktree and its bound env; if
  that fails and exactly one vercel env exists, that one is used; otherwise
  upload is skipped with one warning (mode `none` behaviour).

### Threat model

A hostile process inside the sandbox can:

- Read the files (same VM, root is available) and use the login for model
  calls, spending your subscription or API budget while the sandbox runs.
- Exfiltrate it to any allowed host. With the narrowed policy that is the
  model hosts and your own allowed domains; domain fronting through an
  allowlisted SNI is possible where the CDN accepts it.
- Use the OAuth refresh token to mint new access tokens after the sandbox is
  gone, until you revoke it (`claude /logout` on the host, or the provider
  console). If the in-VM agent refreshes first, the host's rotating refresh
  token may be invalidated.

It cannot reach the Vercel token, git tokens or any other host secret: none are
uploaded. Recommended: use a dedicated account for Vercel lanes.

### Why proxy and inject are refused

`proxy` is not a small addition to tgproxy. Missing: a public HTTPS endpoint
(tgproxy binds plain HTTP); Vercel OIDC verification (no JWT/JWKS code exists);
an "OIDC instead of virtual key" auth mode; per-sandbox scoping to this host's
ledger; revocation on teardown; and subscription logins cannot be fronted, so
it would work only with an API key. It is queued as its own batch. `inject`
would store your model key with Vercel's firewall, which contradicts the rule
that no key leaves your machine; it works only for API keys and stays refused
unless the owner overrides.

## Connection and limits

- **Host pin.** The interactive WebSocket ticket host (and the health host)
  must equal a suffix or end in `.<suffix>`. The key
  `interactive_host_suffixes` in `[env.<name>.provider]` (default empty, which
  means the built-in `vercel.run`, `vercel.sh`, `vercel.com`) overrides it.
  `vercel.app` is never a default: any Vercel user can deploy there. A refusal
  degrades to the stream mode and never breaks the lane.
- **OIDC has no refresh.** thegn reads the token's `exp` locally without
  verifying it. `thegn doctor` shows "OIDC, expires in Xh". With under 5
  minutes left, requests are refused: use a Vercel access token instead.
- **429.** A `Retry-After` of 20 s or less is honoured on retried operations;
  longer waits are refused with "rate limited by Vercel; retry after Ns".
  Create and run-wait are never retried after a 429.
- **Delete** sends `deleteOrphanSnapshots=true` and also removes each snapshot
  itself, after the ownership proof.

## Reaping orphans

```sh
thegn env reap --provider vercel            # dry run: names, ages, tags, estimate
thegn env reap --provider vercel --apply    # typed yes on a tty, or --yes
```

Only sandboxes tagged for this controller, with no ledger record and passing
the tag proof, are deleted. A partial or unknown listing refuses `--apply`. It
never runs on its own.

## Unpulled results

When a terminal session ends, thegn writes a marker under
`$XDG_STATE_HOME/thegn/vercel/unpulled/<name>`. The Environments panel row and `thegn doctor` (a count) show it. Press `p` on the row (or run
`thegn env pull <worktree>`) to pull; a successful pull clears the marker.
`p` is an explicit key, so it may resume the sandbox. There is no background
resume and no timer.

## Cost estimate

`thegn fleet cost` has a Vercel section for the current session only: active
CPU time, memory and run time, plus snapshot sizes, priced at Pro iad1 rates
($0.128 per vCPU-hour active CPU, $0.0212 per GB-hour memory, $0.08 per
GB-month snapshots). It is an estimate. Earlier sessions are not included.

## Unverified

Not yet checked on a live account; each is shown as unverified:

- Whether the interactive PTY WebSocket works as implemented (hence the stream
  fallback), and the real WebSocket host (the default suffix list may narrow).
- Sandbox-name and tag charsets, upload and output size limits, and the token
  scopes required.
- Whether resume behaves as described, and whether Nix works in the image.
- 429 behaviour in practice, and whether deleting a sandbox also removes its
  snapshots.
- Upload: that claude needs nothing beyond `.credentials.json` and
  `{"hasCompletedOnboarding":true}`; that the tmpfs directory never reaches
  the persistent auto snapshot (live step: stop, resume, assert gone); and the
  model host lists (claude `api.anthropic.com`, `console.anthropic.com`,
  `platform.claude.com`, `statsig.anthropic.com`; codex `api.openai.com`,
  `auth.openai.com`, `chatgpt.com`).
- Whether the host's rotating refresh token is invalidated if the in-VM agent
  refreshes first.
- OIDC token algorithm and `iss` (relevant to the refused `proxy` design).
- Whether session history can be listed per sandbox (so the cost estimate is
  current-session only).

The kind is experimental (`thegn doctor` says so).
