---
id: environments
title: Environments
parent: sandboxing
order: 5
actions:
  [warm-pool-increment, warm-pool-decrement, toolchain-install, retry-pullbacks]
contexts: [panel:environments]
---

# Environments

Part of [[sandboxing]]. Named environments, worktree toolchains, lifecycle hooks and remote worktrees.

Sprites and Vercel panes push a static `thegn-musl` bridge into the sandbox. Release
archives and Homebrew ship it next to `thegn` and the host finds it automatically;
set `THEGN_BRIDGE_BINARY` to override, and run `thegn doctor` if it reports
`artifact none`.

Sprites and [[vercel-sandbox]] environments are supported on the stable
channel, including `thegn fleet list`, `cost` and `inspect` for them and the
**New environment…** wizard. SSH and mosh remotes are stable too (see
Remote worktrees below). Kubernetes, every other cloud provider and
`[host.*]` are dev channel only (see [[release-channels]]); on the stable
channel `thegn fleet` shows Sprites and Vercel environments only (an ssh env
is not a provider env and is not listed), and the wizard offers only `local`,
`ssh`, `sprites` and `vercel`. Where this page
says "on the dev channel" about provider behaviour, the same behaviour applies
to Sprites and Vercel environments on the stable channel.

## Environments

Named `[env.<name>]` execution environments (local, container, or a cloud
provider) appear in the panel's **system → environments** section — `↵`
(re)binds the cursor env to the active worktree, `t` tests its provider
token (the outcome pops as a toast and is logged), `n` opens the add
wizard (on the stable channel it offers `local`, `ssh`, `sprites` and `vercel`;
the other kinds appear on the dev channel), and `x`
removes it after a confirm (removal also forgets the stored token). On a
[[vercel-sandbox]] env, `p` runs `thegn env pull` for the worktree (it may resume
the sandbox) and the row shows `results not pulled` until a pull succeeds. An
environment can keep a **warm spare pool** so new worktrees start
instantly; raise or lower the active project's pool target with the
warm-pool actions (`Ctrl-Alt-=` / `Ctrl-Alt--`, also palette-runnable). The
target update runs in the background: `warm pool: updating target…` appears
while it is queued, followed by the result, and raising the target stops at
eight spares for the workspace's environment.
To fill the pool by hand, run `thegn env pool-fill` (see
[the CLI reference](../cli.md)). A short-lived process (a CLI command or the
daemon) never refills the pool after a claim: it prints `warm pool for <worktree>
is one spare short; the thegn TUI refills it, or run: thegn env pool-fill
--worktree <worktree>`. Only the TUI refills in the background. `thegn doctor`
shows a `warm pool: N ready, M provisioning` line per provider env (Vercel
shows `not used for vercel`).

If the spare ledger cannot be read, reconciliation warns and makes no pool
changes rather than treating the pool as empty.
When a worktree opens and its spare ledger cannot be read, a warning appears on
the status line and in a toast, and the worktree is provisioned fresh instead
of claiming a spare.
On the dev channel, and for Sprites and Vercel environments on stable, before a spare is handed to a worktree, the worktree's
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
be read, the spare is destroyed instead. On the dev channel, and for Sprites and
Vercel environments on stable, after a checkpoint restore, `thegn env restore` and warm-pool recycle verify an
environment's non-empty network allow/block policy when the provider supports
egress control. If needed, the policy is re-applied and read back. If
verification fails, `thegn env restore` fails and reports that the sandbox is
running without verified egress; warm-pool recycle destroys the spare. A
successful `env restore` reports the verified counts. Before it restores a
Sprites checkpoint, thegn reads the checkpoint: a missing one (404) refuses with
"no checkpoint", a non-empty `health` refuses as unhealthy, and any other failed
read refuses too, so nothing is rolled back to a checkpoint that is gone or
broken. On the dev channel, and for Sprites and Vercel environments on stable,
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
close auto-checkpoint is also skipped with a warning on the dev channel, and
for Sprites and Vercel environments on stable. In
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
dev channel, and for Sprites and Vercel environments on stable, an unreadable
account selection refuses a provider pane; failed
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

`thegn sandbox warm-devshell [--json]` fills the cache explicitly. It runs
`nix print-dev-env --json` in the current repository's main checkout, which
evaluates the repo's flake as you, so running it is your consent. It writes the
cache (0600 file in a private 0700 directory) that the injection reads, and
refuses a directory with no `flake.nix`. Nothing runs it automatically.

### Nix daemon access in local NixOS panes

When `[sandbox] nix_daemon` is omitted, a standalone local interactive terminal
pane on NixOS uses the host Nix daemon only after resolving a native local
`bwrap` or `systemd` backend, when a compatible socket exists, the effective
network mode is NAT, no network allow/block/audit or VPN policy is configured,
and the profile is not sealed. OCI backends are excluded, whether their engine
is local or remotely addressed. Set `nix_daemon = false` to opt out, or `true`
to request the existing explicit behavior. This setting is separate from
`inject_devshell`: it does not evaluate the repository's `.envrc` or devShell
on the host.

Automatic access is not enabled for agent or unattended runs, SSH worktrees,
provider placements, sealed profiles, or panes with `network = "none"` or other
explicit network/VPN policy. A mounted daemon socket lets sandbox processes ask
the host to build arbitrary derivations and access paths in the host Nix store;
use it only for work you trust with that authority.

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

- **First `env up` timing.** A first create on a fresh account takes about
  one to two minutes before everything is done, and your shell is usable after
  about 20 seconds. `thegn env up` prints each phase with its elapsed time
  (for example `Install coding agents — done (51.2 s)`) and a total at the end;
  the loading card shows the same phases with a hint under the slow ones.
  Typical phases: boot 0.4 s, workspace and repository seed 3 s, setup 12 s,
  coding-agent install 45 to 60 s (the agent CLIs download in parallel), agent
  logins 10 s, snapshot 4 s. Later opens are fast: a sprite that is asleep wakes
  in under a second, and `thegn env snapshot` keeps a checkpoint you can
  `thegn env restore` instead of installing again. Use `env up --shell-ready`
  to return as soon as a shell opens and let the installs finish in the
  background.
- **Ledger before create.** thegn writes a `creating` record to its local
  ledger before it asks Sprites to create a sprite, and `ready` after. thegn
  deletes a sprite only when this host recorded it. Sprites that existed
  before this release, or that were made outside thegn, are never deleted by
  thegn; remove those with the Sprites CLI or dashboard.
- **Reuse needs proof.** When a sprite with the wanted name already exists,
  thegn reuses it only if this instance's ledger holds a record of creating it
  under its own scope. Without that proof it refuses: `sprites: a sprite named
X exists in this Sprites account but this thegn instance has no record of
creating it; refusing to use it (it may belong to another thegn instance or
another person). If it is yours: thegn env adopt X --env <env>`. Run that
  `env adopt` line if the sprite is yours. This also applies when a record was
  lost, and to the dev instance for a sprite the stable instance created for the
  same worktree path: the two instances do not share records.
- **Automatic recovery across instances.** thegn stamps a creation marker
  (`/workspace/.thegn/sprite-owner`: sprite name, account fingerprint,
  controller id, 30 minute lease) on a sprite it creates or reattaches. When a
  listed sprite has no record here, reattach, `env snapshot` and `env restore`
  read that marker and record the sprite for you if it names this sprite and
  account and its lease is ours or has lapsed, so stable can pick up a sprite
  dev made (and the reverse) without `env adopt`. A live lease held by another
  instance, or a missing marker (sprites made before this release), still
  refuses with the `env adopt` line. The check reads the sprite and, on
  reattach, writes the marker, which wakes a cold sprite.
- **Checkpoint and restore need a record too.** `thegn env snapshot` and
  `thegn env restore` run the same ledger proof first. Without a record they
  refuse with the `thegn env adopt` line. A sprite made before the ledger
  existed needs `env adopt` once before it can be checkpointed or restored.
- **Create pins URL auth.** Create sends `"url_settings": {"auth": "sprite"}`,
  the documented default, so the sprite's URL never depends on that default.
  If create answers with a different name than the one asked for, thegn errors
  and keeps no record, because a kept record would prove ownership of a
  sprite it did not name. A sprite that answer made shows under
  `thegn env reap --provider sprites --unrecorded`; a 409 names that verb and
  `env adopt`.
- **Unrecorded sprites.** `thegn env reap --provider sprites [--apply] [--yes] [--json]`
  (or `--unrecorded`, below) lists orphans, sprites past `max_lifetime_secs` (age from the API
  `created_at`) and sprites "not recorded by this host". It is a dry run by
  default. `--apply` destroys only recorded sprites past their lifetime, and
  removes ledger records whose sprite is gone. It spares records younger than
  60 seconds, skips unrecorded sprites, and refuses on a partial listing. There is
  no background reaper unless you opt in with `lifetime_reap` (below).
  `thegn env deprovision --all` keeps its prompt, destroys only recorded sprites, and
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
  limit to `min_ram_gb * 1024` MB (no autoscale) once, at create. The policy is
  a per-sprite document the API stores, so reuse, resume and a warm-spare claim
  do not re-apply it. Read it back with
  `thegn fleet inspect <sprite> --env <S> --policy`.
- **Exec protocol errors.** A command stream that ends without a valid exit
  status is a failure, never a success, on any remote. On Sprites, a stream that
  breaks what the client relies on (a malformed exit, two exits with different
  codes, output after the exit, an unknown stream) is reported as
  `sprites exec: protocol error: <what>` and the command fails. The server
  repeats the exit in two forms and sends control messages the protocol document
  does not list; a repeated exit with the same code and a control message of an
  unknown type are accepted.
- **Instance cap.** `max_instances` applies through the ledger; `0` (the
  default for sprites) means no cap.
- **Cost.** `thegn fleet cost` shows a Sprites block with CPU hours and
  reserved GB-hours per recorded sprite, read from the Sprites usage API only
  when you run that verb. It is priced only when `[placement.price]` has a
  `sprites` entry; otherwise it is labelled "usage from the Sprites API;
  unpriced". The usage window starts at the ledger `created_at` floored to its
  UTC hour. A sprite whose usage rows carry no `last_recorded_at` shows "no
  usage reported yet" (JSON `reported: false`) rather than 0.00, and a failed
  usage request marks that row only. Below the per-sprite rows `fleet cost`
  adds one **account** row per distinct Sprites account: month-to-date CPU and
  GB-hours for all sprites in the account (`GET /v1/usage` from the start of the
  UTC month), deduplicated by account fingerprint (a second env on the same
  account says "same account as <env>") and marked `(partial)` when the API says
  the figures are incomplete. A sprite younger than an hour (by its ledger
  `created_at`) is marked `recent` (JSON `recent: true`) in the partial column,
  because the usage API may not have reported it yet. If that young sprite's
  usage reads zero, the row shows `-` with `partial: true` (JSON `partial: true`,
  `usd: null`) instead of a confident 0.00, and a note says the usage is not
  recorded yet; run `thegn fleet cost` again later. A failed account read shows
  "account usage unavailable" and never hides the per-sprite rows.
  On Sprites, reading costs calls the provider's usage endpoints, and a sprite
  has been seen to read `running` once right after such a read; thegn makes no
  command or wake call for a cost read.
- **Results.** A sprite pulls its commits back on exit. `thegn agent run --wait --pull`
  also runs `env pull` after the agent exits and reports it in text and `--json`.

### Interactive output quirk (THE-2450)

Interactive exec can print `/dev/pts/<n>` and the next output on one line: the
Sprites server sends the tty line without its line ending, and thegn forwards
bytes as received. Live step 6c reports this as a WARN, not a failure. To make an
upstream report, run `python3 -B test/sprites_exec_repro.py <sprite> --run`.

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

Newer steps (dev channel, unverified live until a paid run passes them):

- `4b` checks that `sprite-exec` returns the command's exit status (3) and its
  stderr to the caller, accepting the remote command's error text on either
  output stream.
- `10b` reads the sprite's state up to four times, ten seconds apart, after the
  cost read. It fails only if the sprite reads running every time it can be
  read, and warns on a single running reading or an unreadable listing.
- `10w` wakes a cold sprite with `sprite-exec -- true` and prints how long the
  wake took (it must finish within 150 s).
- `--with-agent <name>` is opt-in. It adds the agent steps (7g-7i), which run
  `agent run --wait --pull --json` and so spend model usage. Without it those
  steps are skipped and the plan prints no spend.
  A failed agent step prints the agent's status, exit code, error and last
  output lines, never secrets.
- The teardown removes a sandbox through its worktree when `env deprovision`
  refuses it.
- The state checks are specific to the row they exercise, not to the whole
  fleet.

`test/remote-daily.sh` checks the `agent run` JSON and adds three steps for
sprites and vercel: `11m` commits inside the worktree through the exec bridge,
`11k` lands it without `--release-sandbox` (the sandbox is kept), and `11j`
confirms the record is still `ready` and still listed. Step 8 requires the land
to report releasing the sandbox; `11k` requires the land to report pulling the
sandbox's branch and the commit to be in main. Both lands pass a gate command
that always succeeds, because the scratch repository has no build. All of these
stay plan-only without the spend flags.

### Warm claim, agent preflight and progress

- **Warm claim.** A fresh worktree whose stored location is local but whose
  resolved env is provider-placed may claim a warm spare. The other gates are
  unchanged: the worktree is not live, the env is poolable, nothing is bound
  yet, provisioning is pending and the sandbox floor holds. The pool stays
  opt-in (`[lifecycle.pool] size`). The TUI create path claims the same way.
- **Agent CLI preflight.** After provisioning and before the open,
  `thegn agent run` runs `command -v <prog>` in the sandbox (60 s deadline). A
  missing CLI prints one warning that names the fix; it never blocks the run.
  Network errors are only debug-logged.
- **Progress lines and exit hints.** While `--wait` observes a sandbox run on
  a terminal, stderr gets one progress line every 60 s. Exit 75 and exit 127
  print a hint line, and `--json` carries an `exit_hint` key when there is one.
- **Wake notice.** On a terminal, when opening the exec on a Sprite (or the
  resume, upload and open on Vercel) takes more than 3 s, one line says the
  sandbox is waking. Capture mode stays silent because callers own its
  stdout and stderr.
- **Doctor.** Each remote-live row also shows `placement`, `placement_warning`,
  `sandbox_agents`, `max_instances` and `max_lifetime_secs`. When providers
  are channel-clamped and no rows remain, doctor adds a `warning` row. Doctor
  makes no network calls.

### Retry, inspect, drain and provision

Unverified live. All of this works on both channels for Sprites and Vercel
environments.

- **Retry failed pull-backs.** The palette action "Retry failed pull-backs"
  (`retry-pullbacks`, no default chord) runs the same code as
  `thegn env sync-retry`, off the loop, and posts one toast with the counts. It
  never destroys a sandbox.
- **Inspect.** `thegn fleet inspect <sprite> --env <S> [--policy] [--probe] [--json]`
  reads one sprite from the org-level API: name, status, `created_at` and URL
  auth, with a warning when URL auth is public. It never runs or resumes the
  sprite. `--policy` also reads the memory policy and your configured
  `min_ram_gb`; that read is separate because it may wake a cold sprite. Its
  errors never print the request URL.
- **Live health.** `--probe` runs one short read-only command inside the
  sprite (this wakes a cold sprite) and adds its health: CPU load and cores,
  memory used against the limit (the cgroup limit when one is set) with memory
  pressure, disk use of `/`, uptime and boot id, and the time left before
  `max_lifetime_secs`. The health is `ok`, `warn` (memory or disk at 90% or
  more, or the kernel killed a process for memory this boot) or `unknown` (the
  probe could not run; the reason is printed). Warnings name the cause, for
  example `sprite out of memory: kernel killed rustc 3.7 GB`, and a lifetime
  under an hour. With `--json` the same report is the `health` object.
- **Provision field.** `thegn agent run --json` and a headless `session open`
  carry `provision`: `none` (not a provider env, or already provisioned),
  `claimed_spare` or `provisioned`.
- **Drain warning.** `thegn merge drain` prints one line, and adds it to the
  `--json` `warnings`, for each queued worktree whose effective env is a
  provider env: commits made inside the sandbox are not in the drain until
  pulled (`thegn env pull`, or `thegn land`, which pulls first). It still
  drains them.

### Adopt, the daily loop and release

Everything below is unverified live. It works on both channels for Sprites and
Vercel environments; the Fly and machine0 parts need the dev channel
(`THEGN_CHANNEL=dev`).

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
  already yours. It never touches the sprite itself. Stable and dev instances
  keep separate ledgers, so a sprite one made is refused by the other; the
  refusal names this command, so run it from the instance that needs the sprite.
- **Daily loop.** `thegn agent run --wait --pull` runs the agent, pulls the
  commits back and, on a Fly or machine0 env that bills while started,
  stops the machine and prints `stopped <name> (it bills while started)`.
  `--keep-running` (needs `--wait`) skips the stop. Sprites and Vercel
  self-suspend and are untouched. A stop is skipped while another live session
  is bound to the worktree. Pulls of one worktree are serialised across
  processes by a lock; a second caller waits up to 120 seconds, then reports
  `up to date` or "another pull of <worktree> is running".
- **Land.** A worktree has a sandbox when its stored location is a provider
  location, or its environment is a provider environment and the sandbox exists
  (the local provisioned marker, or a complete provider listing shows it). So a
  worktree created from the command line (`thegn wt new --env`, then
  `thegn env up`) is handled the same as one opened in a pane. `thegn land` on
  such a worktree first pulls the sandbox's branch, then lands the local
  branch, then prints `released sandbox <name>` (with `--release-sandbox`) or
  `sandbox <name> kept`. When the provider's listing cannot say whether the
  sandbox exists, land is refused and names the sandbox: retry when the provider
  answers, or move the worktree to a local environment with `thegn env set`.
  `--release-sandbox` on a worktree with no provider sandbox prints
  `! --release-sandbox ignored: this worktree has no provider sandbox (its env is not provider-placed, or a complete provider listing lacks it); nothing was released`
  and still exits 0. A diverged or dirty pull refuses and
  names the kept ref; a missing sandbox lands the local branch. The sandbox is
  kept, because it holds gitignored state that exists nowhere else:
  `sandbox <name> kept (thegn land --release-sandbox, or env reap, removes it)`.
  `--release-sandbox` destroys the proven sandbox after a successful land and
  prints `released sandbox <name>`; a failure there is a warning, not an error.
  `merge drain` is unchanged.
- **Crash recovery.** After a crash during cleanup, a merge-queue row whose
  worktree has no registry record is kept and retried when the repository's
  environment is remote; it is retired only when the environment is local.
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

Unverified: how long the Sprites usage API lags behind a new sprite (the one-hour `recent` window is a guess); whether Sprites reports `health` on a fresh checkpoint; which file-write parameter the Sprites server honours (file write and delete send both the documented and the previous form); whether the policy GET wakes a cold sprite; Fly's 404 timing after an app DELETE; whether a Claude refresh token shared between host and sprite is
invalidated when one side refreshes; whether a Sprites checkpoint can be cloned
to a new sprite (checkpoints restore in place only, so there is no warm
golden image); the Fly IPv4 cost; and which login files pi needs.

The live scripts `test/sprites-live.sh` and `test/vercel-live.sh` spend real
money. They need `THEGN_CHANNEL=dev` and `--i-accept-spend`, and touch only
sandboxes recorded in the run's own temporary ledger.

With `--min-ram-gb N`, step 5d proves the memory limit: a process inside the
sprite allocates past it. The step passes when the process is stopped at the
limit. It prints a WARN, not a failure, when the provider accepts the limit but
does not enforce it inside the guest.

Step 5d also reports the guest's available memory next to the policy and
classifies the allocation as enforced, not enforced, stopped above the limit, or
inconclusive.

The live scripts print `TIME <step> <seconds> s` after each step and
`STARTUP <label>: <seconds> s` lines at the end: cold create, first interactive
output and wake for Sprites; create, first command, stop and resume for Vercel.
With `--with-agent` the Sprites check starts and stops its own pane daemon for
the agent step.

### Removing and releasing provider sandboxes

- **Sprites `model_credentials`.** `[env.<n>.provider] model_credentials` is
  valid for `provider = "sprites"` with `none` or `upload`; `proxy` and
  `inject` are refused. `upload` forwards model credentials and agent login
  files into the sandbox. `none` withholds the fixed list of model credential
  variables from provisioning, the pane environment and repo exec, and skips the
  agent login upload and resync; provisioning prints one line naming what was
  withheld and how to grant it. Leaving `model_credentials` unset asks once per
  project and provider on the first interactive launch: forward and remember,
  not now (this launch only), or never. The answer is stored per project, so
  later launches do not ask. A headless launch with no stored answer withholds
  and prints the hint instead of blocking. `thegn doctor` and config validation
  report an unset Sprites value as waiting for that answer. Names you add to
  `env_passthrough` yourself are not affected. Not yet verified live.
- **Sprites destroy needs proof.** After the provider accepts the DELETE,
  thegn polls the sprite every 2 seconds for up to 30 seconds until it gets a
  definite 404. Only then does it revoke custody and forget the record.
  Otherwise the record is kept and destroy fails, so a second `wt rm` or
  `env reap` can finish the job. A 404 on the DELETE itself counts as done.
- **Fly destroy needs proof.** After the DELETE answers 2xx, thegn polls
  `GET /v1/apps/{app}` every 2 seconds for up to 30 seconds. Only a 404
  retires the record. Otherwise destroy errors, naming the app being
  destroyed, and keeps the record so `wt rm` or `env reap` can finish.
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

### Build and test jobs on a Sprite

A project can bind one Sprite as the runner for pinned `cargo build` and
`cargo test` jobs. Set `[jobs] runner = "<env name>"` and run
`thegn job run cargo-test` (both channels; see [[cli]]). Each attempt runs in a
scrubbed environment with a private home, one dispatch per attempt. A deadline
kills the remote process group, and secrets stay out of the uploaded snapshot.
Ctrl-C cancels the attempt. If thegn dies mid-job, the daemon reattaches the
attempt on its next start and never re-runs a command whose dispatch it cannot
prove. One project may use one Sprite. For Vercel, see [[vercel-sandbox]].

## Interactive remote panes

This section applies to every remote provider that opens panes through its own
exec API. Where only Sprites behaves a given way, it says so.

### Terminal identity

A remote pane is thegn's own terminal, so the remote shell gets what a local
pane gets, never the host terminal's own type.

- **Native panes** get `TERM=xterm-256color` and `COLORTERM=truecolor`.
- **`sprite-exec`** sends the host `TERM` when the remote is sure to have its
  terminfo (`xterm-256color`, `screen-256color`, `tmux`, `vt100`…), and
  `xterm-256color` for anything exotic such as `xterm-ghostty`. It adds
  `COLORTERM` when the host's is `truecolor` or `24bit`.
- **Locale.** The first non-empty of the host's `LC_ALL`, `LC_CTYPE` and `LANG`
  is read. When it names a UTF-8 codeset, the remote gets `LANG=C.UTF-8`.
  Otherwise no locale is sent. Any `LC_ALL` or `LC_CTYPE` from the host is
  dropped, so a stale value cannot override it.

### Close versus quit

Closing a remote pane (its close key, or `thegn session close`) ends its remote
shell: on Sprites thegn asks the server to kill the exec session with SIGHUP and
waits up to 5 s. Quitting thegn, or a dropped connection, only detaches: the
shell keeps running and the next open reattaches to it.

### Connection liveness

Interactive remote transports share one rule, and a new one should adopt it.

- **Armed only by input.** Typing starts a 5 s timer. Any message from the
  server, data or control, cancels it. Nothing runs while you are idle.
- **Two strikes.** At the first expiry thegn sends a WebSocket ping and waits
  5 s more. If still nothing arrives, thegn treats the connection as dropped,
  and the pane reattaches.
- **Self-calibrating.** It acts only on a connection that has already answered
  a ping, and only for interactive panes. A server that never answers pings is
  left alone, so a quiet but healthy session is never cut.
- **`TCP_NODELAY`** is set on the exec and proxy sockets, so a keystroke is not
  held back to be batched.

### Local echo

Local echo prediction draws a burst only after the server has echoed an earlier
key of the same line, on any row, and never past the right edge. It is shown
only when the echoed cell holds the typed character, so nothing is predicted at
a password prompt.

Output retires only the keys the server actually echoed: output confirming k
predicted keys drops exactly those k and keeps the rest. Output that does not
move the cursor keeps the overlay, which is dropped at the next event once the
oldest unconfirmed key is over 2 s old (checked on events, not by a timer). A
mismatch, or pending non-ASCII text, clears everything. The first key of a line
is never predicted.

### Size, input batching and pastes

- **A tty exec states its size after connect.** The Sprites driver queues one
  resize on the fresh control channel right after an open or a reattach, so the
  remote never has to assume it from the open request (the URL still carries
  cols and rows, harmlessly). Before this, the first `stty size` in a remote
  pane was `0 0` until the first resize.
- **A pump that forwards decoded input coalesces queued input.** The
  `sprite-exec` bridge joins the keys already queued behind one event into a
  single stdin write, up to 32 KiB per message; one larger event, such as a
  bracketed paste, is sent whole. A lone keystroke and a lone Escape are sent
  at once: an Escape always travels alone so it cannot merge with the next key.
  A resize ends a batch and is sent after the bytes. `TCP_NODELAY` stays on.
  A 1 MiB paste took about 17 s before batching; the figure after it is not yet
  measured.
- **A paste sent just after Enter can arrive bracketed**, as it does over
  `ssh`: the remote shell enables bracketed paste once its prompt redraws, so a
  paste landing after that point is wrapped in the paste markers.

Measured on a live Sprites sandbox (alpha.113): 19 of 23 parity cases matched a
local pane; keystroke echo was about 123 ms slower than local; an output flood
was about 15 times slower, with the cause not yet established (no fix is
claimed); a new worktree's shell was ready about 21 s after the request, with
the agent install continuing in the background.

### Background setup (prompt first)

On every remote provider, a worktree's shell opens as soon as its workspace is
ready. Coding-agent install, agent logins, shell history and the snapshot finish
in the background, with progress on the status line (`background setup: <step>…`).
On Vercel the agent install and snapshot also run after the prompt now.

- An agent launched meanwhile waits for its CLI and login before it starts.
- `thegn env up` in a terminal returns as soon as a shell can open and finishes
  the background steps in a detached `thegn env up --wait` (log:
  `$XDG_STATE_HOME/thegn/provider/deferred/<sandbox>.up.log`); a failure shows in
  `thegn env show`. Scripts (no terminal) and `--wait` wait for everything;
  `--shell-ready` returns without starting the background run.
- Each phase of a cold create (sandbox create, boot wait, workspace, clone, …)
  prints its duration on stderr (`provision: <step> — done (N.N s)`), and the
  create, boot and home-probe phases are also logged with `elapsed_ms` under the
  `thegn::startup` tracing target.
- A background phase that was interrupted or failed resumes on the next open and
  shows in `thegn env show`. A failure also raises a notice.
- `thegn env down` and deleting the worktree cancel it.

### Agent login sync

Login sync copies an agent's config into the sandbox in two phases.

- **Phase 1** always runs and carries only the small set the agent needs to be
  logged in. For hermes that includes its auth files.
- **Phase 2** is an allowlist of top-level entries per agent: claude, codex, pi
  (`settings.json` and the `agent/` tree's settings, extensions, skills, agents,
  prompts and themes) and hermes (`config.yaml`, `SOUL.md`, skills, hooks,
  prompts, memories, scripts, plugins without their cache; no `.db` files and no
  `cache` directories). Telemetry, transcripts and runtime state never ship.
- **Dependency trees** such as `node_modules` are skipped for every agent.
- **An agent thegn does not know**, whose tree has more than 2,000 allowlisted
  files, ships its top-level files only, with one warning.

### Agent install log

The agent install step never fails the environment. It runs one `npm install -g`
for every missing npm agent (retrying per package if that fails) while hermes's
curl installer runs in parallel. Output and per-agent seconds go to
`~/.cache/thegn-agent-install.log` in the sandbox; read it when an agent
launched in the pane exits 127.

### Parity script

`test/remote-parity.sh` checks that a remote pane behaves like a local one:
identity and locale, size, colours, UTF-8, Ctrl-C, job control, paste, a
full-screen program, arrow keys, mouse, the alternate screen, an output flood,
the title, cwd, exit status, echo latency and a leak check after close. It takes
the env as an argument:

```
THEGN_CHANNEL=dev test/remote-parity.sh --env <name> --i-accept-spend [--keep-going] [--samples N] [--idle-probe] [--startup-only [--fill-spare]] [--tui-probe]
```

Without flags it only prints its plan and creates nothing. `--self-test` runs
the same cases with a local pane on both sides, using no provider and no
network. `just remote-parity` runs it. Each case prints PASS, WARN or FAIL;
THE-2450 is reported as a WARN.

- `--startup-only` measures only the cold and warm time to a first prompt.
- `--fill-spare` (needs `--startup-only`) also measures a warm-spare claim and
  spends one more sandbox; without it that row is skipped.
- `--tui-probe` turns on the full-screen-program probe
  (`test/remote_parity_tui.py`) for the run. The probe gives the TUI a
  throwaway home and config, copies only your `[env.<name>]` table into it
  (read-only, so the pane is remote), keeps the run's own state dir so the TUI
  still finds its daemon, and types the ready marker only once the pane shows
  a shell prompt. It fails when the pane reports this machine's hostname (a
  silent local fallback) and refuses to drive a TUI whose config resolves to
  yours.

Latency and throughput are measured and printed, not held to fixed budgets.

## Sandbox safety on delete, down, deprovision, rm, set, restore, land and reap

Every command that would destroy, recycle or release a provider sandbox first
pulls its commits home, and refuses when the pull fails or the sandbox holds
uncommitted or untracked paths (any `git status --porcelain` line). Force skips
only the refusal and says what is lost.

| command                         | behaviour                                                                                                                                                                                              | instead                                                                      |
| ------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------- |
| `wt rm`, TUI delete             | pulls first; refuses on a failed pull or a dirty sandbox. `--force` (and the TUI force delete) destroys anyway. With `--delete-branch`, a branch that received sandbox commits is kept, with a warning | commit inside the sandbox and retry, or `--force`                            |
| `env down`                      | refuses a dirty sandbox or a failed pull: `could not pull back from sandbox <id>: ...; nothing was destroyed. Re-run with --force to destroy anyway`. `--stop` never refuses                           | `env down --force`, or `env down --stop` to park it                          |
| `env deprovision <id>`          | never destroys a sandbox bound to a registered worktree; `--all` skips those and refuses when a binding cannot be resolved                                                                             | `env down --worktree <wt>` or `wt rm <wt>` first                             |
| `env rm`                        | refuses while a registered worktree resolves to that env with a provider sandbox                                                                                                                       | `env down --worktree <wt>`, `wt rm <wt>`, or `env set <env> --worktree <wt>` |
| `env set`                       | away from a provider env, pulls best-effort and names the sandbox it leaves, for headless worktrees too                                                                                                |                                                                              |
| `env restore`                   | pulls the sandbox's commits first and says what uncommitted work it discards                                                                                                                           |                                                                              |
| `land --release-sandbox`        | re-checks right before releasing; a dirty sandbox is kept (exit 0, `!` line)                                                                                                                           |                                                                              |
| `merge land`, TUI land          | pull first, as `thegn land` does. A pull that fast-forwards the branch makes the queued candidate stale: refused                                                                                       | re-queue with `thegn merge add`, or `thegn land`                             |
| `wt prune`                      | never removes a worktree bound to a provider sandbox (keep reason `provider-sandbox`)                                                                                                                  |                                                                              |
| closing (forgetting) a worktree | names the sandbox it leaves, for headless worktrees too                                                                                                                                                |                                                                              |

Lifetime reapers (Sprites) keep a sandbox bound to a registered worktree and
warn once; when any worktree's binding cannot be resolved they keep every
sandbox. Fly and VPS reaping is unchanged. Background passes decide "bound"
offline and never wake a sandbox; the hibernator only execs a sandbox of a
sleeping kind (Sprites, Fly, machine0) when a listing shows it awake.

## Exec streaming

Non-tty `sprite-exec` streams output without a cap, sends remote stderr to
stderr (stdout no longer carries the stderr tail or the 1 MiB marker) and
forwards a non-terminal stdin. TTY mode runs only when stdin and stdout are
both terminals. Provisioning (`run_exec`) is unchanged.

## Parity run summary

`test/remote-parity.sh` ends with a `RUN SUMMARY` block listing each step's
result and timing.

## Unverified

Not confirmed against a live provider:

- the guest has the `C.UTF-8` locale;
- the shape of the server's replay on reattach;
- the largest WebSocket message the server accepts;
- whether an open, idle exec keeps a sprite awake;
- whether Sprites applies the cols and rows in the exec URL itself (the explicit
  resize makes it moot, and the live log will say);
- where the limit on a very large paste is;
- whether the hermes installer uses npm;
- the cause of the parity P15 failure (pending the live run);
- the P15C exit 1.

## Remote worktrees

`[sandbox.remote] host` runs worktrees on another machine. Control-plane
work (git reads, container lifecycle) always uses ssh; the interactive
pane uses mosh by default, so it survives network changes. `[remote]`
tunes keepalives, timeouts, and retry backoff for flaky links.

> Remote worktrees over SSH/mosh are supported on the stable channel. Kubernetes
> and cloud providers other than Sprites and Vercel are **dev-channel only** —
> see [[release-channels]].

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
