---
id: environments
title: Environments
parent: sandboxing
order: 5
actions: [warm-pool-increment, warm-pool-decrement, toolchain-install]
contexts: [panel:environments]
---

# Environments

Part of [[sandboxing]]. Named environments, worktree toolchains, lifecycle hooks and remote worktrees.

## Environments

Named `[env.<name>]` execution environments (local, container, or a cloud
provider) appear in the panel's **system → environments** section — `↵`
(re)binds the cursor env to the active worktree, `t` tests its provider
token (the outcome pops as a toast and is logged), `n` opens the add
wizard, and `x`
removes it after a confirm (removal also forgets the stored token). On a
[[vercel-sandbox]] env, `p` runs `thegn env pull` for the worktree (it may resume
the sandbox) and the row shows `results not pulled` until a pull succeeds. An
environment can keep a **warm spare pool** so new worktrees start
instantly; raise or lower the active project's pool target with the
warm-pool actions (`Ctrl-Alt-=` / `Ctrl-Alt--`, also palette-runnable). The
target update runs in the background: `warm pool: updating target…` appears
while it is queued, followed by the result, and raising the target stops at
eight spares for the workspace's environment.
If the spare ledger cannot be read, reconciliation warns and makes no pool
changes rather than treating the pool as empty.
When a worktree opens and its spare ledger cannot be read, a warning appears on
the status line and in a toast, and the worktree is provisioned fresh instead
of claiming a spare.
On the dev channel, before a spare is handed to a worktree, the worktree's
network allow/block policy is verified on that spare and re-applied if it
drifted. An unverifiable spare is quarantined (listed in `thegn doctor`) and
the worktree is provisioned fresh. Empty policies skip this check. A provider
without an egress policy API (every provider except Sprites and Vercel) also skips it, and
thegn warns that the env's network allow/block lists are NOT enforced: once
when a worktree opens or claims a spare, in `thegn env provision` and
`thegn env restore` output, and in `thegn doctor` (`network rules [env.<name>]`).
If the current warm-pool target cannot be read, reconciliation makes no pool
changes rather than falling back to the configured size. The warm-pool keys
report `warm pool: cannot read the current target: ...` and save nothing.
Reconciliation halts, such as lease, ledger, target, or spare-read failures,
appear as a status-line toast; the same halt is shown once until it clears or
its message changes. When deleting a worktree cannot recycle a claimed spare,
its ledger row remains until the provider confirms destruction, so a failed
destroy leaves it listed for a later retry.
A stale or released spare is recycled from its base checkpoint only when the
provider's checkpoint list confirms that checkpoint exists; if the list cannot
be read, the spare is destroyed instead. On the dev channel, after a
checkpoint restore, `thegn env restore` and warm-pool recycle verify an
environment's non-empty network allow/block policy when the provider supports
egress control. If needed, the policy is re-applied and read back. If
verification fails, `thegn env restore` fails and reports that the sandbox is
running without verified egress; warm-pool recycle destroys the spare. A
successful `env restore` reports the verified counts. On the dev channel,
opening a worktree against an existing provider sandbox with egress control
also verifies its non-empty network policy and reapplies it if it drifted. If
the policy cannot be verified, opening is refused and the sandbox is left in
place; a newly created sandbox that fails verification is still destroyed.

If a worktree's environment selection cannot be read from the state database,
thegn never falls back to the default environment. `thegn env` verbs report an
error; worktree deletion refuses before removing anything; closing a tab does
path-only cleanup and warns that an SSH sandbox container may remain. On the
dev channel, provider SSH connect, warm-pool operations, hibernation,
provisioning and host placement are refused or skipped with the reason. The
close auto-checkpoint is also skipped with a warning on the dev channel. In
the panel's system → environments section, an unreadable selection marks
no environment bound, adds a red `binding unreadable: <reason>` row and a
`binding?` badge, and `↵` refuses to rebind until the state database is
readable.

When the state database exists but the worktree's recorded location cannot be
read or is unrecognised, writes and code runs refuse instead of using the host
checkout. This covers panel git actions, Tests and Jobs, `thegn land`, mutating
`thegn pr` actions, `thegn ci rerun`, `thegn ci trigger` and `thegn ci cancel`,
worktree delete, Sandbox container stop or restart, clipboard image paste and
toolchain install. With no state database, worktrees remain local as before.

If an environment selection cannot be read and a non-local `[env.*]` is
configured (dev channel), the tab shows `env?` and the loading splash shows
`env: unknown`. `thegn doctor` reports an unknown environment; a failed pane
connection is not blamed on a guessed provider. Provisioning skips trust-gated
devcontainer steps and launch skips devshell injection, with a warning. On the
dev channel, an unreadable account selection refuses a provider pane; failed
SSH sandbox teardown identifies the container that may still be running, and
hibernation refuses a restore when its record cannot be read while hibernation
is enabled. Teardown and close auto-checkpoint warnings appear on the status
line and in a toast while the TUI runs; the CLI prints them to stderr.

An unreadable recorded worktree location makes the control API's commit, review handoff,
autopilot push or worker sandbox, `thegn issue create` and `thegn issue comment`, PR creation,
worktree-resolving `thegn env` commands, `thegn repo trust` and submodule init refuse. Background
fetch and merged-worktree cleanup skip it. On the dev channel, hibernation refuses and its
bridge-key drop warns and skips; provider provisioning and attach, provider and host worktree
pipelines, and host placement refuse too. A plain local worktree is unaffected unless a provider
or host placement is configured. Also on the dev channel, failed agent-login uploads mark “Sync
agent logins” Failed with one line per account but do not stop provisioning. The resident bridge
uses the pinned environment or workspace default; if that selection is unreadable, it skips
connecting and logs a warning.
On the dev channel, if the worktree location cannot be read, pane launches and
provider connections are refused when a remote placement is configured
(`[placement]`, `[sandbox.remote]`, or a non-local or host-pinned `[env.*]`);
without one, panes use the local path.
On the dev channel, if the environment selection itself cannot be read while a
remote placement is configured, the launch stops in the halt modal with the
read error instead of using the default environment. Without a remote
placement, a local shell is not refused for that read error. For host
placement, if another provisioning run for the same container is active, this
open skips provisioning; when `THEGN_LOG` is set, the warning is written to
`thegn.log`. The next open checks again.

On any channel, closing a tab whose location cannot be read tears down by path
only and warns on the status line that a remote sandbox may remain. Idle
hibernation also keeps the sandbox, logs a warning and retries later when it
cannot tell whether a shell is attached.

Pool state shows on the sidebar's project chip, not in this section.
`thegn doctor` lists claimed warm-pool spares without a bound worktree and
quarantined spares under `Warm pool (provider spares needing review)`; it will
not destroy them on its own. `thegn doctor --json` includes the same report in
`warm_pool`.
`[sandbox] default_env` picks the environment new worktrees use.
A repo's `env =` setting is only a hint; if that environment would weaken
user policy, it is ignored with a warning.

### Worktree toolchains

When a worktree declares a mise or asdf toolchain, thegn adds its safe shims to
the pane `PATH`. The optional mise environment layer is lower precedence than
an environment bundle or devshell and is applied only after the current repo
config set is approved. Use the command-palette action `toolchain-install` to
run the bounded, explicit install operation; opening a pane never downloads
tools. A `(mise ~)` token means approval is pending, while `(mise !)` means
the provider or shims are unavailable. Remote worktrees remain responsible
for installing their tools on the remote target.

Cached flake devShell injection (`[sandbox] inject_devshell`) is off by
default. Turn it on in your own config (globally, per project or per
environment), or approve a repository's `.thegn.toml` request with
`thegn repo trust . --approve <id>`; an unapproved request is listed as pending
and changes nothing.

## Lifecycle hooks and pane setup

`[hooks]` commands are host-side worktree/session lifecycle jobs. They run
outside the sandbox with a curated environment, capture stdout/stderr in the
worktree state log, and use the event-specific repository/worktree directory;
see [[configuration]] for the six events, ordering, trust gate, timeout, and
failure policy. The legacy `[sandbox].prepare` list is the compatibility
alias for `post_create`, not a second fire-and-forget path.

`[sandbox].init_script` remains separate: it is a POSIX shell snippet executed
inside the sandbox before each agent or shell pane starts. Use it for
per-pane/container setup; use lifecycle hooks for host-side worktree setup,
teardown, or session edges.

## Sprites

A sprites env (`provider = "sprites"`, `placement = "provider"`) is paid, so
thegn is strict about what it creates and deletes. Nothing in this section has
been verified live against the real Sprites API yet.

- **Ledger before create.** thegn writes a `creating` record to its local
  ledger before it asks Sprites to create a sprite, and `ready` after. thegn
  deletes a sprite only when this host recorded it. Sprites that existed
  before this release, or that were made outside thegn, are never deleted by
  thegn; remove those with the Sprites CLI or dashboard.
- **Unrecorded sprites.** `thegn env reap --provider sprites [--apply] [--yes] [--json]`
  (or `--unrecorded`, below) lists orphans, sprites past `max_lifetime_secs` (age from the API
  `created_at`) and sprites "not recorded by this host". It is a dry run by
  default. `--apply` destroys only recorded sprites past their lifetime, and
  removes ledger records whose sprite is gone. It spares records younger than
  60 seconds, skips unrecorded sprites, and refuses on a partial listing. There is
  no background reaper unless you opt in with `lifetime_reap` (below).
  `thegn env down --all` keeps its prompt, destroys only recorded sprites, and
  says how many it skipped.
- **Fleet states.** A recorded sandbox the provider does not list is `missing`
  only when this host still holds its local provisioned marker. Otherwise it is
  `down` (never provisioned, or `env down` already ran; the worktree keeps its
  env pin). A sprite in the account listing with no worktree and no ledger
  record on this host is `unrecorded`, not an orphan. Neither `down` nor
  `unrecorded` turns the masthead badge amber, because neither is a fault:
  one is a sandbox you stopped, the other belongs to someone or something else.
  Budget admission still counts every listed row.
- **Listing unrecorded sprites.** `thegn env reap --provider sprites --unrecorded [--json]`
  is read-only: it refuses `--apply`, takes no lease and destroys nothing. It
  lists unrecorded sprites oldest first with status, age and 30-day CPU and
  reserved-memory GB-hours (one usage request per sprite, at most 50 per
  account), and prints one `thegn env adopt <name> --env <env>` line for each
  sprite whose name is safe to paste. The Sprites list API reports no size, so
  none is shown. Run `env adopt` to take a sprite under management.
- **Git audit file.** Seed, pull and push-lane events are appended as JSON lines
  to `$XDG_STATE_HOME/thegn/provider/git-audit.jsonl`, rotated at 1 MiB with one
  previous file kept. A failed write only warns.
- **No background creates.** `eager = "workspace"` or `"all"` never creates a
  sprite for an unfocused worktree. The focused worktree still provisions when
  you focus it.
- **Keep-awake.** Headless work on a sprite (`thegn agent run`, a headless
  `session open`, stage launches, or `thegn sprite-exec --hold <dur>`) runs
  inside a small `sh` heartbeat that holds the sprite awake. It registers a task
  `thegn-hold-<16 hex>` with a 5 minute expiry, refreshes it every 60 seconds,
  deletes it when the command exits, and stops at a ceiling: `max_lifetime_secs`
  when set, else 4 hours (`--hold` is capped at 24 hours). Ctrl-C stops the
  command too. Interactive panes get no hold, so an idle open pane does not
  bill. After a crash the worst case is the ceiling plus 5 minutes. If `curl` or
  the socket is missing in the VM, one line is printed and the command runs
  unheld.
- **Size.** Sprites have a fixed 8 vCPU; `size`, `min_vcpu`, `gpu` and `nvme`
  are ignored and validate warns. `min_ram_gb > 0` sets the sprite's memory
  limit to `min_ram_gb * 1024` MB (no autoscale) after create and again at each
  provision, with a readback.
- **Instance cap.** `max_instances` applies through the ledger; `0` (the
  default for sprites) means no cap.
- **Cost.** `thegn fleet cost` shows a Sprites block with CPU hours and
  reserved GB-hours per recorded sprite, read from the Sprites usage API only
  when you run that verb. It is priced only when `[placement.price]` has a
  `sprites` entry; otherwise it is labelled "usage from the Sprites API;
  unpriced".
- **Results.** A sprite pulls its commits back on exit. `thegn agent run --wait --pull`
  also runs `env pull` after the agent exits and reports it in text and `--json`.

### Live script

`THEGN_CHANNEL=dev test/sprites-live.sh --env <name> --i-accept-spend [--keep-going]`
drives a real sprite and spends money. By default it stops at the first failed
step. With `--keep-going` a failed step prints `FAIL <id>`, the run continues and
the exit status is non-zero if anything failed. It proves pull with a real commit
made inside the sprite (not a stub), and prints the `fleet cost` JSON. A pass shows
those steps worked on that run; it does not verify the paths it does not touch.
It also checks the fleet `down` and `unrecorded` states and the read-only
`env reap --unrecorded` listing, so it needs a build with those changes. A paged
listing test (`SpritesProvider::with_list_page_size`, `max_results=n` on every
list page, `#[ignore]`d and read-only) compares paged and unpaged results.

### Adopt, the daily loop and release

Everything below is dev channel (`THEGN_CHANNEL=dev`) and unverified live.

- **Adopt.** `thegn env adopt <sprite> --env <S> [--yes] [--json]` records a
  sprite you made elsewhere so thegn may manage it. It reads the sprite (a 404
  refuses) and prints its name, status, API `created_at`, this host's
  controller id and the account fingerprint, then says thegn will be able to
  destroy it on `env down`, `env reap`, `land`, worktree delete and the opt-in
  reaper, and that it counts toward `max_instances`. On a tty you type the
  sprite name back; with no tty you need `--yes`. It writes a `ready` record
  with `created_at` set to now, so lifetime counts from adoption. It refuses a
  record owned by another controller, account or provider, refuses when
  `max_instances` is reached, and says "already recorded" if the sprite is
  already yours. It never touches the sprite itself.
- **Daily loop.** `thegn agent run --wait --pull` runs the agent, pulls the
  commits back and, on a Fly or machine0 env that bills while started,
  stops the machine and prints `stopped <name> (it bills while started)`.
  `--keep-running` (needs `--wait`) skips the stop. Sprites and Vercel
  self-suspend and are untouched. A stop is skipped while another live session
  is bound to the worktree. Pulls of one worktree are serialised across
  processes by a lock; a second caller waits up to 120 seconds, then reports
  `up to date` or "another pull of <worktree> is running".
- **Land.** `thegn land` on a provider worktree first pulls the sandbox's
  branch, then lands the local branch. A diverged or dirty pull refuses and
  names the kept ref; a missing sandbox lands the local branch. The sandbox is
  kept, because it holds gitignored state that exists nowhere else:
  `sandbox <name> kept (thegn land --release-sandbox, or env reap, removes it)`.
  `--release-sandbox` destroys the proven sandbox after a successful land and
  prints `released sandbox <name>`; a failure there is a warning, not an error.
  `merge drain` is unchanged.
- **`lifetime_reap`.** `[env.<n>.provider] lifetime_reap = true` (sprites only,
  default false, needs `max_lifetime_secs > 0`) lets a running client destroy
  recorded sprites past their lifetime, checked every 300 seconds. It lists the
  org once per account and deletes nothing on a partial or unknown listing. A
  sprite whose API status is `running` is skipped and reported once. It never
  creates, resumes or execs. With no env opting in it does no I/O at idle.
- **`login_homes`.** `[env.<n>.provider] login_homes = { claude = "<dir>", codex = "<dir>" }`
  (your own config only, like `model_credentials`) makes the sprite login copy
  and resync read that directory instead of your live host login, so a
  dedicated account can be used. Auth files are written with mode 0600 where the
  file API supports a mode. The codex list is `.codex/auth.json` and
  `.codex/config.toml`, plus the two older json paths.

Unverified: whether a Claude refresh token shared between host and sprite is
invalidated when one side refreshes; whether a Sprites checkpoint can be cloned
to a new sprite (checkpoints restore in place only, so there is no warm
golden image); the Fly IPv4 cost; and which login files pi needs.

The live scripts `test/sprites-live.sh` and `test/vercel-live.sh` spend real
money. They need `THEGN_CHANNEL=dev` and `--i-accept-spend`, and touch only
sandboxes recorded in the run's own temporary ledger.

### Removing and releasing provider sandboxes

- **Sprites `model_credentials`.** `[env.<n>.provider] model_credentials` is
  valid for `provider = "sprites"` with `none` or `upload`; `proxy` and
  `inject` are refused. `upload` forwards model credentials and agent login
  files into the sandbox. `none` withholds the fixed list of model credential
  variables from provisioning, the pane environment and repo exec, and skips the
  agent login upload and resync; provisioning prints one line naming what was
  withheld and how to grant it. An env that does not set `model_credentials`
  behaves as before (credentials are forwarded), and `thegn doctor` and config
  validation say so. Set `model_credentials = "none"` to withhold explicitly.
  Names you add to `env_passthrough` yourself are not affected. Not yet
  verified live.
- **Sprites destroy needs proof.** After the provider accepts the DELETE,
  thegn polls the sprite every 2 seconds for up to 30 seconds until it gets a
  definite 404. Only then does it revoke custody and forget the record.
  Otherwise the record is kept and destroy fails, so a second `wt rm` or
  `env reap` can finish the job. A 404 on the DELETE itself counts as done.
- **Closing keeps the environment.** Closing a worktree tab or pane never
  destroys its sandbox. The close resolves the worktree's location, repo root
  and env first and then forgets the worktree, so the background close and
  auto-checkpoint act on that snapshot. Only `wt rm`, `env down`, `env reap`
  and `land --release-sandbox` destroy.
- **Exec deadline.** A single provider exec has an overall 31 minute deadline,
  above the 30 minute provisioning step ceiling, so shorter caller timeouts fire
  first. On expiry the Sprites session is closed.

- **Pull-back survives restarts.** Every successful `data = "sync"` upload
  writes a journal entry, so closing a worktree or `thegn wt rm` in any process
  (a restarted UI, or the CLI) pulls the sandbox's changes back first, using the
  in-memory record when there is one and the journal otherwise. The pull marks
  the entry `pulling`; success removes it. A failed pull marks it
  `failed {reason}` and **keeps the sandbox**. A `pulling` entry younger than 15
  minutes means another process is pulling, and the command refuses and keeps the
  sandbox. A journal whose target (provider kind, sandbox id, workdir) no longer
  matches the current env, or an entry that cannot be read, also refuses, keeps
  the entry and names the file (remove it by hand once you have checked it).
- **`thegn doctor`** lists a pending pull-back as its own row per sandbox.
- **`sprite-exec` routes to the owning account.** It reads the sprite's ledger
  record and uses the Sprites env whose provider account matches. With no record
  and exactly one Sprites env it uses that env; otherwise it refuses and names
  the candidates. `vercel-exec` capture mode picks its env the same way as the
  terminal path and refuses when several Vercel envs are configured and none
  resolves.
- **Vercel failures show stderr.** On a nonzero exit the last 8 KiB of stderr is
  appended to the error with control characters removed. Raw stdout used for
  file transfer is unchanged.
- **Failed one-shot exec shows stderr.** When a Sprites one-shot exec exits
  non-zero, the last 8 KiB of stderr, control characters removed, follows the
  stdout. A zero exit prints exactly what it did before.

A destroy counts as done only when a complete provider inventory no longer
lists the sandbox. A partial listing proves nothing.

- `thegn wt rm` refuses when the sandbox cannot be confirmed gone: the provider
  token is unset, the destroy is not confirmed, or a provider location is
  stored under a non-provider env. The message names the sandbox and the two
  ways out: set the token and retry, or `thegn env set --worktree <wt> <local env>`,
  which clears the location and warns which sandbox is still running.
  `thegn env reap --provider <p>` keeps listing that sandbox.
- `thegn land --release-sandbox` clears the stored location only after the
  destroy is confirmed. Otherwise it prints `! sandbox <name> kept: <why>`
  after "landed" and keeps the location; the exit code is unchanged. A pool
  spare prints `returned sandbox <name> to the warm pool`.
- Closing a provider worktree keeps its sandbox and says so in one notice that
  names the sandbox and how to remove it.
- Keep-awake tells the truth: if the first hold call fails, the script runs
  unheld with a line on stderr, and on exit it does not release a hold it
  never got.
- `thegn doctor` has a "Remote live readiness" block per sprites and vercel
  env (offline; names only, never values): whether the token resolves and the
  controller id is readable, ledger counts, ledger sandboxes no worktree or
  spare is bound to, and which env var names, git token and agent logins go
  into the sandbox. Sprites credential forwarding itself is unchanged.

## Remote worktrees

`[sandbox.remote] host` runs worktrees on another machine. Control-plane
work (git reads, container lifecycle) always uses ssh; the interactive
pane uses mosh by default, so it survives network changes. `[remote]`
tunes keepalives, timeouts, and retry backoff for flaky links.

> Remote worktrees and cloud providers are **dev-channel only** — see
> [[release-channels]].

Daytona is in maintenance: thegn drives its REST API; interactive panes need
the ssh gateway (planned). `thegn doctor` marks a Daytona env experimental and
`live_verified: false`, like Hetzner.

The bridge helper is uploaded to a staged file and checked by SHA-256 before
it is moved into place. If the install cannot be verified, the bridge
connection is skipped.
The helper lives at `/var/tmp/thegn-bridge/<sha256>/thegn`, outside the
worktree. Its digest is re-checked before thegn starts it through a provider's
native exec API or for a reverse tunnel.
A provider launch through its control command, such as a vendor CLI or SSH,
checks the digest again before running the helper; changed bytes are refused
with `thegn: resident bridge digest mismatch`.
When a provider's `exec_command` uses `ssh` with a host, its checked bridge
launch is passed to the remote shell as one quoted word.
The resident bridge scrubs secret-shaped text, including tokens, credentials in
URLs, and private-key blocks, from command error output before returning it;
standard output is returned unchanged.

A remote checkout with commits or changes made since thegn last synced it is kept for recovery, and thegn shows how to recover it.

On provider placements, an explicit isolation request thegn cannot enforce is
refused before provisioning, credential access or upload. Without such a
request, the placement runs as `provider-managed (unverified)`; `thegn doctor`
shows that floor.

A thegn-derived VPS or Fly sandbox name is bound to the worktree that created
it. Another worktree resolving to the same name is refused before a provider
call.
VPS and Fly ledger records carry a credential-free fingerprint of the provider
kind, API authority, and configured (or default) token variable name, never the
token itself. A name recorded under another account is refused; the VPS reaper
quarantines that record and does not delete it.
The Fly reaper also quarantines records for another account without revoking or
destroying them. If it cannot determine the current account, it quarantines
every record before making provider calls.
If the UI is running, a change to the non-empty set of resources a VPS or
Fly reaper keeps for review raises one alert toast for that provider, naming
the first and counting the rest. An unchanged set does not repeat it.
A VPS or Fly `max_instances` cap
counts sandboxes for the same provider account; records without an account or
that cannot be read count against every account.
On the dev channel, `max_lifetime_secs` is enforced by the Fly and VPS reapers
(Hetzner and DigitalOcean). Providers without a lifetime reaper, such as
machine0 and Sprites, do not enforce it; `thegn config validate` warns when it
is set. Destroy those instances yourself or set the value to `0`.

A provider env with no remote transport fails closed: it halts before
provisioning instead of running the session on the host. Fly sessions run on
the Fly machine over thegn's ssh bridge. Sprites headless sessions use native
exec and start in the provisioned checkout; `interactive_command` is only used
with `exec = "cli"`: with `exec = "auto"` or `"api"` the `exec_command` and
`interactive_command` keys are ignored (`thegn doctor` warns), and a worktree
whose saved transport prefix is stale is repaired on its next launch. Fly
provisioning installs Nix even when running as root without sudo. A plain ssh
host with `data = "in_env"` has the repo seeded at `env up` or session open into
`$HOME/thegn-worktrees/<repo>/<worktree>`. The provider "provisioned" marker and cached sandbox home
live in the state dir, so the CLI and daemon agree, and `.thegn-provisioned` is
excluded from git in remote checkouts. Provisioning fails clearly when Nix is
required but did not install, and interrupted Nix builds no longer leave the
store broken.

The Hetzner provider is experimental: its lifecycle has not been verified on a
live account, and `thegn doctor` says so.

On the dev channel, an unreadable or malformed VPS or Fly ledger record is
refused rather than treated as absent. Destroy, retarget, address lookup,
`vps-ssh` and managed-key selection refuse it; a refused destroy keeps SSH
custody and tells you to fix or remove the record before retrying. SSH keys
registered with Hetzner, DigitalOcean and machine0 use `thegn-managed-`
followed by the first 32 hexadecimal characters of the SHA-256 digest of the
decoded public key. An invalid OpenSSH public key is refused.

On the dev channel, the Fly backend strictly parses an app's machine list and
rejects malformed lists or duplicate machine IDs. If more than one live
thegn-managed machine is present, it refuses to choose one.

Machine0 creates and restores are serialized across thegn processes by an
owner-only lock in the VPS registry. A create that cannot get the lock within
the six-minute readiness budget is refused with a busy message.
Machine0 image, SSH-key and VM lists are parsed strictly: an unrecognised
response shape, an unidentifiable row or a duplicate entry fails the operation
instead of being treated as an empty or shorter list. Machine0 readiness,
status and SSH-auth waits, and Fly's sshd probe, count each in-flight request
against their total time budget.
Machine0 records a pending reservation before creating a VM, and reservations
count toward `max_instances`. If the create outcome is unknown, such as after a
lost response, the reservation remains; retrying the create, or reopening the
worktree once machine0 lists the VM, adopts it (waits for it, applies
provisioning, and records its SSH key) instead of creating a duplicate. The
reservation is retired only after adoption is ready. The error says to retry
later or destroy the sandbox to retire the reservation. A VM with that name
and no pending create from this machine is refused. The Sprites list follows
every page and refuses results over its page or row limit, or an ambiguous
continuation, rather than treating a partial list as complete. Configured
provider ids at `env.<name>.provider.id` must be lowercase for Sprites,
Hetzner, DigitalOcean, Fly and machine0; validation refuses uppercase ids.

VPS and machine0 sandboxes pin their SSH host key on first contact with
`accept-new` in a per-instance file under `$XDG_STATE_HOME/thegn/vps/known_hosts.d/`, moved once
from `~/.thegn/vps` (the VPS and Fly machine ledger lives beside it).
`thegn env show` warns about risky sandbox mounts such as the runtime dir or
profile trees. Thegn creates that directory with owner-only
permissions before managed SSH connections, refuses a symlinked or
non-private directory, requires the same key on later connections, and
removes the pin when it destroys a VPS or machine0 sandbox. Creating a machine0
VM also clears a stale pin left by an earlier VM with the same name.

If local parity fails during provider provisioning, autonomous launches on
that worktree are refused until a later provision applies parity cleanly;
interactive panes can still open. Host-parity closure pushes use a
per-installation signing key and are skipped with a warning rather than sent
unsigned when signing is unavailable. The iroh call-home reach is available
only in builds with the `iroh` feature; `thegn doctor` reports whether it is
built in.

For remote and provider worktrees, thegn reads `.gitmodules` from the remote
commit and rechecks approved submodules before initializing them. It validates
the remote worktree sync plan before uploading anything. A provider sandbox's
network policy is applied and read back before its first command; a mismatch
stops provisioning. If an existing sandbox's policy cannot be verified, thegn
refuses to provision it and leaves it in place.
For provider sandboxes, network allow and block entries are trimmed,
lowercased, stripped of a trailing dot, and deduplicated before the provider
policy request; non-ASCII entries (use `xn--` punycode), IP literals, and
malformed domain patterns are refused with the entry named in the error. Any
allow entry keeps the final deny-everything-else rule, even when blocks shadow
all allow entries.
A provider launch using `sealed-tunnel` with a VPN configured is refused before
provisioning; if a refused launch created its provider sandbox, thegn destroys
that sandbox.

With `data = "sync"`, closing the worktree copies sandbox changes back by
comparing each file with what the upload sent: a file the sandbox did not
change never overwrites a local edit. A file changed in both places (or deleted
locally and changed in the sandbox) stops the sync with a list of files; nothing
is written locally and the sandbox is kept. Make each listed file match one
copy and close again. Files and directories excluded by `.gitignore`, `.ignore`,
`.git/info/exclude` or your global git excludes file are not compared or copied
back (the upload skips them too), so build output never blocks the sync. Files
deleted in the sandbox are not deleted locally.
