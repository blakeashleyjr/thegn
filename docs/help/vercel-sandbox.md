---
id: vercel-sandbox
title: Vercel sandboxes
parent: sandboxing
---

# Vercel sandboxes

> **Dev channel only.** Provider environments are removed on the stable channel;
> set `THEGN_CHANNEL=dev` to use them. See [[release-channels]].

An environment with `provider = "vercel"` runs a worktree in a Vercel Sandbox
microVM. thegn drives it through Vercel's HTTP API and a hidden `thegn
vercel-exec` bridge; there is no SSH. Vercel environments are supported on the
stable channel, including `thegn job run` with a Vercel runner (see
[[release-channels]]).

## Setup

```toml
[env.cloud]
placement = "provider"

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
- `placement = "provider"` is required. A `provider` with any other
  `placement` is ignored, and `thegn config validate` warns about it.
- When a terminal pane opens, thegn writes the admitted interactive host (and the
  `/health` host, or a refused host) once to stderr and at `info` in the log.
  Only the host is written, never the URL query or the ticket.
- Network egress is **denied by default**: a new sandbox cannot reach any
  host. `egress = "registries"` allows only package registries and git hosts;
  `egress = "allow-all"` opens it. See [Network egress](#network-egress).
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

`thegn env down --stop` parks the sandbox without deleting it. A stop and the
lifetime ceiling both run before any resume.

## Stop, resume and expired snapshots

- A one-shot command that Vercel refused because the session had stopped is
  retried once after the sandbox is resumed, and only when waking is allowed.
  The same single retry applies to streamed commands, interactive shells and
  file transfers (uploads, downloads and the login upload). It happens only
  when Vercel refused the request before starting anything, so nothing runs or
  is written twice; a second refusal is reported as the error.
- If Vercel reports that a stopped sandbox's saved snapshot has expired, thegn
  refuses to resume it and says to destroy it (`thegn env down`) and create a
  new one. thegn never deletes it on its own.
- Stopping a session that is already stopped counts as success.

This fetches the bundle into `refs/thegn/pull/<worktree>`; only the lane's own
branch fast-forwards. See [[sandboxing]].

## Destroy proof, exec ceiling and live shape checks

- **Destroy keeps the record until proven gone.** After the DELETE returns
  2xx, thegn polls `GET` (never resuming the sandbox) every poll interval for
  up to 30 s. A 404 removes the ledger record and deletes the snapshots. A
  sandbox that is still present keeps the record and returns an error, which
  matches the 60 s listing proof `env down` already requires.
- **A snapshot listing that did not finish** deletes what it listed, reports
  an error and keeps the record. Run the destroy again to continue.
- **Exec ceiling.** One exec request may run for up to 31 minutes; the
  facade's outer deadline governs. A stream that ends with no command exit is
  an error: "vercel: run command ended without an exit status".
- **Exec keep-alive.** While a command runs, the exec checks the session once
  at the start and then every 5 minutes (`EXTEND_CHECK_SECS`) and extends it
  when it is due, so a long command is not cut off by the 30 minute session
  timeout. The lifetime ceiling still bounds it. A failed check is logged at
  debug level and never fails the exec.
- **Live sentinels.** `test/vercel-live.sh` step `3s` checks that the
  `fleet list --json` response shapes parse, and `7s` does the same for
  `fleet cost --json`. Step `4b` checks the exec exit code. Both sentinels
  WARN rather than fail when a field is missing.
  Steps `8s` and `8r` prove that a stop and a resume reached Vercel.
- **Fixtures.** `crates/thegn-svc/tests/fixtures/vercel/README.md` records
  that those JSON files are the shapes thegn parses, not captured bodies.
  After the first live run (THE-2234), replace each file with a captured body
  (secrets removed) so a parse change fails CI instead of the next paid run.
  Until then the destroy proof is UNVERIFIED live.

## Model credentials

One key, per environment, default `none`:

```toml
[env.cloud.provider]
model_credentials = "none"   # "none" | "upload" | "proxy" | "inject"
```

Only your own config file can set it. The env wizard never writes a value other
than `none`, `env create` has no flag for it, and a repo-local overlay cannot
set it. On Vercel it defaults to `none`. Sprites accepts the same `none` and
`upload` values; leaving it unset there asks once per project whether to
forward, and a headless launch withholds with a hint instead of asking (see
[[environments]]). Other providers refuse the key (use `[sandbox.home] agents`
there).

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

## Network egress

A Vercel sandbox gets **no network access by default**: `curl example.com`
fails inside it. thegn sends the policy when it creates the sandbox and sends it
again every time a stopped sandbox resumes, so a resumed sandbox never runs on a
stale policy. If the policy cannot be applied on resume, the sandbox is not
handed out.

```toml
[env.cloud.provider]
egress = "registries"            # deny (default) | registries | allow-all
egress_allow = ["example.com"]   # extra hosts for deny and registries
```

| `egress`     | Reachable from the sandbox                                                                                                                                                                                 |
| ------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `deny`       | Nothing, plus any `egress_allow` hosts. The default.                                                                                                                                                       |
| `registries` | `crates.io`, `static.crates.io`, `index.crates.io`, `registry.npmjs.org`, `pypi.org`, `files.pythonhosted.org`, `github.com`, `codeload.github.com`, `objects.githubusercontent.com`, plus `egress_allow`. |
| `allow-all`  | Everything. Code in the sandbox can reach any host. An explicit choice.                                                                                                                                    |

`egress_allow` uses the host-pattern grammar from [[sandbox-network]]: `x.com`
is the apex and every subdomain, `*.x.com` subdomains only, `=x.com` exactly
that host. `*` and port suffixes are refused; choose `allow-all` to open
egress. Set it at creation with `thegn env create <name> --provider vercel
--egress registries`, or pick it in the new-environment wizard under
**network**. Only your own config can set these keys; a repo `.thegn.toml`
cannot. Changes take effect on the next create or resume.

### Egress policy

The model-login narrowing below is a separate, tighter step. An allow list from `[sandbox.network]` is sent to Vercel as a custom policy;
`*` means allow everything. An egress change is applied to the sandbox and to
its running or starting session, and a failed session update is an error.

### Recipe: a dedicated login

The shipped default stays `none`. To use a login that is not your daily one,
change only config (your own config file):

```toml
[env.cloud.provider]
model_credentials = "upload"
login_homes = { claude = "~/.thegn-logins/claude", codex = "~/.thegn-logins/codex" }
```

`login_homes` makes `upload` read those directories instead of the live host
login, so a hostile sandbox can only spend the dedicated account. Like
`model_credentials` it cannot come from a repo overlay.

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

Active CPU time is already summed across vCPUs. Vercel bills memory in
one-minute minimum increments; the estimate does not round up to them.

A report built from a snapshot listing that did not finish is marked
incomplete. Snapshots left behind by a sandbox that is gone are priced too.

## Selected build and test Jobs

A project can select a Vercel team and project as a runner for pinned build and
test Jobs (runner alias `vercel-<project>`). Only the closed `cargo build` and
`cargo test` recipes run, for the deadline the route forwards. The agent, its
model login and the Vercel token stay on your machine.

The default Vercel Sandbox image has no Rust toolchain. When `cargo` is not on
the sandbox `PATH`, the launcher installs a minimal rustup toolchain (stable)
into the attempt's own home before the recipe starts, so the first run pays the
download. If the image cannot fetch one (no `curl`, or no network), the attempt
stops with a `thegn-job-launch-failed` message on stderr naming the missing
toolchain and reports as indeterminate rather than exit 127.

Rust links through a C compiler, and the default image ships none. When `cc` is
missing the launcher installs one before the recipe starts, with the image's
package manager (`apt-get`: `gcc libc6-dev`; `dnf`/`microdnf`: `gcc`; `apk`:
`gcc musl-dev`), using `sudo -n` when it is not root. If that fails the attempt
stops with `thegn-job-launch-failed: no C linker (...)`. Both installs run in
every attempt's fresh sandbox, so each attempt pays the download and install
time (typically under a minute) before the recipe's own clock matters.

A job sandbox is created with your env's egress list plus these hosts, so a
deny-by-default env still works for cargo recipes: `sh.rustup.rs`,
`static.rust-lang.org`, `index.crates.io`, `static.crates.io`, `crates.io`, and
the OS mirrors `archive.ubuntu.com`, `security.ubuntu.com`, `ports.ubuntu.com`,
`deb.debian.org`, `security.debian.org`, `cdn.amazonlinux.com` and
`dl-cdn.alpinelinux.org`. Add any other host through `egress_allow`; an
`allow-all` env stays allow-all. Interactive sandboxes are not widened.

Each attempt gets its own fresh sandbox, so a fresh sandbox is enough to
finish. The steps, in order:

- The provider's team and project must equal the binding's. A mismatch is
  refused before the ledger is written or any request is sent. The ledger
  directory is pinned to the first team and project it served, and recovery and
  cancel check the recorded pair again before touching a sandbox.
- The attempt number and spend cap are written to
  `$XDG_STATE_HOME/thegn/vercel-jobs/<project>/` first. An attempt that would
  exceed the spend envelope is refused.
- The sandbox name is written before the create request, so an ambiguous
  create can be found by name. The sandbox uses the configured `size` as its
  vCPU ceiling and lives for the transfer budget plus the deadline plus five
  minutes. It is never extended. An attempt whose lifetime does not fit the
  smaller of the env's `max_lifetime_secs` and the plan's session maximum (five
  hours by default; the Hobby plan allows 45 minutes) is refused before
  anything is created.
- Every claim re-reads the sandbox and checks this controller's tag and the
  vCPU ceiling. Protected files (`.git`, `.thegn`, credential trees, `.env*`,
  agent homes) are never part of the snapshot.
- The snapshot, including tracked edits, untracked inputs and executable bits,
  is written into a per-attempt checkout. Every file is read back and hashed,
  and the root directory digest must equal the pinned source digest. A deleted
  file is absent because the checkout starts empty.
- The command starts detached, under `env -i`, with no environment in the
  request. The sandbox id, session and command id are written to the ledger
  before a result is trusted; if that write fails the attempt is
  indeterminate, never a success.
- An unclear start (a timeout or a server error) is never sent again. The
  attempt reads back as indeterminate with its sandbox deleted or pending.
- At the deadline the command is killed, the session is stopped and the sandbox
  is deleted. The attempt reports a timeout and its outputs are not read.

Cleanup is reported `done` only when the sandbox delete is confirmed.
Otherwise it stays `pending` with the sandbox name in the record. The job
ledger's reconcile settles it (there is no command for it yet); only a row past
its deadline, transfer budget and a safety margin is touched, and a row that
changed in the meantime is left alone. The env reaper does not delete job
sandboxes early: it stops them at `max_lifetime_secs` and deletes them seven
days later. Spend is an estimate from elapsed time at a configured rate, also
for an attempt rejected after its sandbox was created; an attempt whose sandbox
may still exist, or an unpriced one, keeps its whole cap committed.

This is the executor only. Wiring a project's runner choice to it, live runs
and the usage report for job sandboxes are not done yet.

## Unverified

The client was checked against Vercel's published API document on 2026-11-19
and corrected where it differed. It has still not been run against a live
account. Not yet checked on a live account; each is shown as unverified:

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

## Builds on small sandboxes

A Hobby sandbox has 4 vCPU and 8 GB. One `rustc` can use about 3 GB, so cargo's
default of one job per CPU can get the compiler OOM-killed (clippy and nextest
fail with `Out of memory: Killed process (rustc)`). For a `cargo` recipe, the
selected-job launcher sets `CARGO_BUILD_JOBS` to floor(MemTotal / 3 GiB), at
least 1 and at most the CPU count (2 on the Hobby size). Set `CARGO_BUILD_JOBS`
in the sandbox environment to override it. In an interactive shell in the
sandbox, export `CARGO_BUILD_JOBS` yourself the same way.

The security suite (`just security-e2e`) needs bwrap user namespaces, which a
Vercel sandbox does not provide. That is expected, not a thegn bug: run it on a
host with user namespaces.

The kind is experimental (`thegn doctor` says so).

## Unverified

- The live casing of Vercel session statuses.
- The cause of the live exit 127 for the agent CLI in a sandbox.
- Whether Fly and VPS keep their records until the machine is absent.
