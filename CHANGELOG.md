# Changelog

All notable changes to **thegn** are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[SemVer](https://semver.org) with pre-release tags (`0.1.0-alpha.N` →
`0.1.0-beta.N` → `0.1.0`). Release tags are `v<version>` on `main`.

## [Unreleased]

## [0.1.0-alpha.137] — 2026-11-25

Remote environments report their health, Sprite and Vercel jobs survive dropped
connections, and coding agents can be wired to thegn in one step.

### Added

- A remote health model: a sidebar dot and popup per remote environment, `thegn env status --watch`,
  `thegn fleet inspect --probe`, and `env.status` on the control API and MCP.
- `thegn sprite-exec --detach`, `--attach` and `--status`, for server-side runs that outlive the client.
- Agent integrations: a thegn skill, `mcp wire` support for thegn's MCP server with Codex and Hermes
  adapters, Claude and Codex lifecycle hooks through `thegn agent hook`, and the same wiring inside remote
  environments. The tour is offered with one key.
- Tabs move left and right with Shift-Alt-←/→ or by dragging on the tab strip; a pane can break out into
  its own tab (Ctrl-Alt-Shift-b) or move to the previous or next tab (Ctrl-Alt-Shift-←/→) without
  restarting.
- The right panel shows layout keys and the focused zone's keys from the live keymap
  (`[panel] layout_hints`, `[panel] context_hints`).

### Changed

- The masthead fleet cost per hour includes metered AI spend and hides at zero unless
  `[bars] fleet_always_show` is set.
- Narrow panel tab strips shorten labels before hiding tabs, and never hide the active tab or `work`.
- Sandbox caches are private or read-only per sandbox.
- Remote provisioning runs independent steps at the same time.
- Vercel cargo jobs are bounded by the sandbox's memory.

### Fixed

- Short-lived database connections are capped process-wide, so a startup burst no longer parks many
  open file descriptors.
- Sprite exec streams stay alive when idle and reattach after a drop.
- A Sprites job's `CARGO_HOME` links to the real toolchain; a missing sprite is named plainly.
- A Vercel job records its exit status and error output even when the attempt is indeterminate.
- Leftover `/homeless-shelter` paths on Vercel sandboxes are cleaned up; stale pipeline lanes are dropped.

## [0.1.0-alpha.136] — 2026-11-25

The standard sandbox hides more of home, background behaviour that reaches the
network or changes your repositories now asks first, and remote `thegn job run`
works on fresh Sprites and Vercel sandboxes.

### Added

- A help page for installing and running thegn under WSL2.

### Changed

- The standard sandbox profile hides credential files, cloud CLI logins and sibling repositories, and
  mounts `~/.claude` and `~/.codex` settings read-only.
- Usage polling is opt-in. thegn asks before scanning the default repository roots and before installing
  the merge-guard hook.
- Copied text is no longer stored in the database unless clipboard register persistence is turned on.
- Removing a clean worktree keeps its branch by default and says so.
- Vercel job sandboxes may reach the Rust toolchain hosts, and a cargo job installs a minimal toolchain
  and C linker when the image has none.

### Fixed

- A daemon that inherited a named profile's state root no longer nests `profiles/<name>` twice, which left
  the UI rejecting the daemon and no shell starting.
- Wrapper exit notices (systemd-run, bwrap, ssh) carry the real last error line, the log path and a recovery
  step; repeated failures collapse into one row, and onboarding no longer reports success while panes fail.
- Pasting into a terminal advances by the raw byte count.
- Typing in the new-worktree wizard's program list no longer goes to the branch name.
- Closing the tour remembers the default keymap; test and agent failure badges clear on the next success.
- Sprites jobs create the marker directory and retry when the first write 404s on a fresh sprite; a failure
  before dispatch reports "job not started".

## [0.1.0-alpha.135] — 2026-11-25

A new remote environment opens its shell sooner: coding-agent install and the
other slow setup steps finish in the background.

### Changed

- Remote environment readiness is polled from 250 ms with backoff instead of every 2 s, and the repository
  is seeded with a blobless clone. `env up` logs per-phase timings.
- `thegn env up` in a terminal now returns once a shell can open and finishes coding-agent install, logins and the snapshot in a detached background run; `--wait` (and any non-terminal run) still waits for everything.

### Fixed

- At daemon start, job ledgers of every Sprites and Vercel environment are reconciled and
  reattached once, not only the configured runner's; an environment that cannot be resolved
  is skipped with a warning.
- Background prefetch and pull-request refresh workers share two database slots, so a
  startup burst no longer holds many SQLite connections open at once.

## [0.1.0-alpha.134] — 2026-11-25

Agent builds and tests can run on a Sprite or Vercel sandbox. With
`[project.<slug>.jobs] route = "auto"`, a terminal agent's `cargo build` and
`cargo test` go to the project's job runner instead of the local machine.

### Added

- `thegn job shim`, `thegn job shim-install` and `thegn job match`. A
  `route = "auto"` agent pane gets the shim directory on its `PATH`, and
  `[jobs] admission_timeout_secs` (default 30) bounds the wait for a runner.
- A sprite created by another thegn instance is adopted from its creation
  marker (`/workspace/.thegn/sprite-owner`, 30-minute lease) instead of being
  refused.
- `thegn doctor --full` shows tailnet identity; the default output redacts it.

### Changed

- The Linux GNU release archive is built against a glibc 2.28 floor, so it
  runs on Debian 12 and other older distributions. Asset names are unchanged.
- The environments, privacy and launch docs no longer call Sprites and Vercel
  dev-only, and record the first-run network trace.

### Fixed

- A cancelled worktree create no longer leaves a ghost row in the sidebar.
- A recorded daemon session stays out of the relaunch attach pool, so restore
  brings back the saved layout.
- The idle benchmark's warm-up launch skips the first-run tour; the database
  writer closes an idle connection after 5 seconds.

### Known limitations

- A Sprites interactive (pty) exec can drop the line break after its first
  line of output. The loss is upstream of thegn.

## [0.1.0-alpha.133] — 2026-11-25

Model credentials are no longer sent to a remote sandbox without asking. If
`model_credentials` is unset, the first agent launch on a Sprite asks once per
project and provider; set `upload` or `none` to skip the question.

### Changed

- Forwarding your model login and API keys to a Sprite now asks once per
  project and provider: forward and remember, not now, or never for this
  project. Answers are kept in thegn's database, never in a repository file.
  Runs with no screen to answer on (`thegn job run`, pipelines) do not forward
  and print a one-line hint naming the setting.
- The Alt+W agent picker lists only agents installed on this machine, with an
  install hint for the rest.
- The welcome tour shows a visible "Skip tour (Esc)" control on every screen.

### Added

- `[project.<name>.jobs] runner` binds a job runner to one project, and
  `thegn job route` prints where this worktree's jobs will run and why.
- The provider fleet report is available over the control socket and MCP.
- `nix run github:blakeashleyjr/thegn` starts thegn (the flake declares
  `apps.default`).
- Performance harnesses: `just perf-compare` (tmux and zellij comparison),
  `just perf-echo` (quiet-session keystroke echo), cat-file and many-pane modes
  for `just perf-flood`, and host and daemon RSS in the soak report.
- Tests replay database snapshots from shipped releases through the migrator,
  and a SIGKILL test checks that a session survives its client being killed.
- Discussion templates and a launch-day operations plan under `docs/launch/`.

### Fixed

- `thegn config validate` warns that network allow and block lists are
  advisory on local sandbox backends.
- Provider inventories that are unstable or too large are refused instead of
  half-read, and an empty offline read is labelled incomplete.
- A lost Vercel job reconcile race is reported as superseded, not failed.
- Refusing to destroy a sprite another thegn instance created now names
  `thegn env adopt`.
- `thegn agent run --track --wait` closes its dispatch row when the agent
  exits, so the next tracked run on that issue is not refused.

## [0.1.0-alpha.132] — 2026-11-24

Restart thegn after upgrading. If an older daemon still holds the database,
thegn now says so and offers to restart it; the database is backed up before
any schema migration.

### Fixed

- Upgrading no longer fails with a bare "refusing database schema migration":
  thegn explains that an older daemon is still running, offers a safe restart
  from the terminal, and keeps the last three timestamped database backups.
- A first run makes no TCP connections: the music-player (MPD) probe only runs
  when you configure an MPD socket or set `MPD_HOST`. The idle database writer
  closes its connection when unused.
- Startup moves help-registry and merge-guard work off the first-frame path and
  logs per-phase timings.
- The first `env up` on Sprites installs agent CLIs in parallel and reports a
  total time; the shell is usable after about 20 seconds.

- Stable-channel messages for remotes and client/server: `thegn doctor` shows the
  remote row as partly stable (ssh/mosh remotes and client/server stable;
  Kubernetes and direct iroh dev-only), a missing `mosh-server` gets an install
  hint, `serve --iroh-direct`/`pair direct` name `thegn serve` + `thegn pair new`,
  and a dropped Kubernetes env says it is dev-channel only.

### Added

- Vercel sandboxes now deny network egress by default. `[env.<n>.provider]
egress = "registries"` allows only package registries and git hosts,
  `"allow-all"` opens it, and `egress_allow` adds hosts. The policy is applied
  on create and on every resume; the new-environment wizard and
  `thegn env create --egress` offer the choice.
- Every release archive (Linux gnu and musl, macOS) and the Homebrew, AUR, deb
  and rpm packages now ship the static x86_64-linux-musl resident bridge as
  `thegn-musl` beside `thegn`, so Sprites and Vercel panes work from a release
  install without a Nix build. The host finds it next to its own executable
  (`THEGN_BRIDGE_BINARY` still overrides), and `thegn doctor` explains how to
  fix a missing one.
- The first launch opens a full-screen welcome tour: the workspace model on a
  small live diagram, a theme you see applied as you scroll, an honest table of
  what each sandbox tier protects with the backends that work on this machine,
  a default coding agent picked from the ones installed, and a drill on your
  real key chords. Every screen can be skipped, `Esc` leaves at once, and
  nothing is saved until the last screen shows the exact changes. Reopen it
  from the palette with **Welcome tour…**; `thegn setup` still opens the
  classic setup wizard.

### Changed

- Sprites and Vercel Sandbox environments are supported on the stable channel:
  their `[env.*]` tables survive the stable config clamp, worktrees can open on
  them, the panel shows the environments section, and `thegn job run` with a
  Sprites or Vercel runner is no longer refused; `thegn fleet` lists, costs and
  inspects Sprites and Vercel instances, and the New-environment wizard offers
  only providers your channel supports. Every other cloud provider, the managed
  `[host.*]` pool, Kubernetes placement, `thegn host` and `thegn placement` stay
  dev-channel only.
- SSH/mosh remote workspaces (`[sandbox.remote]`, `[env.*] placement = "ssh"`)
  and client/server mode (`thegn serve`, `attach`, `pair`, `session`, `events`)
  are supported on the stable channel. Kubernetes placement and the direct-iroh
  listener (`serve --iroh-direct`, `pair direct`) stay dev-channel only. The
  New-environment wizard offers `ssh` on stable.

## [0.1.0-alpha.131] — 2026-11-24

### Added

- Each pane gets a memory high-water mark at 85% of its cap
  (`[sandbox.limits] memory_high`) so the kernel reclaims before it kills, and
  a 4G swap cap (`memory_swap`) so one pane cannot fill the machine's swap;
  `memory_swap_total` optionally caps swap for all of thegn. Background and
  gate jobs keep their own limits.
- thegn watches each pane's memory: a `mem NN%` badge while it is near its cap,
  inbox and desktop notices near the cap and when a process is OOM-killed, and
  a pane whose shell or agent was killed for memory reopens in place, resuming
  Claude or pi (at most once per 10 minutes, never after you typed).
- `just update [stable|dev]` installs a verified release (or a Mac-built
  branch) and restarts that instance's daemon so the new schema can migrate;
  `docs/just-recipes.md` catalogs every recipe.
- `thegn job run` can cancel a running Vercel job, and the daemon reattaches
  Sprites job attempts whose owner died.

## [0.1.0-alpha.130] — 2026-11-24

### Fixed

- A process that exceeds the per-pane memory cap (`[sandbox.limits] memory`)
  is killed on its own; the pane and its shell stay open. Before, systemd
  stopped the whole pane unit and the pane closed.

## [0.1.0-alpha.129] — 2026-11-24

### Added

- `thegn job run [--runner ENV] [--deadline SECS] <cargo-build|cargo-test>`
  (dev channel) runs a pinned build or test of the current worktree on the
  project's selected Sprites or Vercel runner, set with `[jobs] runner`. The
  provider token stays on this host; Ctrl-C cancels the attempt, a second
  Ctrl-C exits at once, and the command exits with the job's own code.
- The daemon reconciles interrupted job attempts on startup, only when a
  ledger has unsettled rows.

### Changed

- Selected-job attempts carry an owner lease (pid, boot id, heartbeat), so
  recovery never touches an attempt whose owner is still running, including
  after the laptop sleeps.
- A cancel reaches the selected runner and reads back as cancelled, before
  or during dispatch (Vercel honours it before dispatch only).
- Job snapshots skip files your global gitignore excludes and any key,
  certificate, `.envrc`, Terraform state or credential file.

## [0.1.0-alpha.128] — 2026-11-24

### Added

- `wt disk --lanes --apply` reclaims the `target` directory of an idle lane
  checkout whose ownership is verified, holding Cargo's lock across the removal
  and failing the command if anything is left behind. Bare target lanes and
  lanes with sources stay untouched until they carry an ownership proof.
- Terminal search shows when results were capped and keeps the Tab and Esc
  hints visible.

### Changed

- Library code imports names explicitly instead of through globs, and builds
  formatted strings in place with `write!` instead of temporary strings.

### Fixed

- A Sprites conformance test no longer flakes when the mock closes its socket
  before the client reads the exit frame.

## [0.1.0-alpha.127] — 2026-11-24

Restart the daemon after upgrading: schema v117 adds durable CI-autofix and
PR-agent refusal reasons.

### Added

- Sprites selected-job attempts can be cancelled, interrupted attempts are
  reconciled without ever re-running a command whose dispatch is unknown, and
  a running attempt can be reattached.
- A fixture-tested Vercel Sandbox selected-job executor for pinned build and
  test attempts, with team/project identity checks, a lifetime ceiling and a
  spend envelope.
- The MCP server offers `agent_dispatch` for write-scoped native agent launches
  with a redacted audit record. It is never offered to, or run for, a caller
  using a scoped control token (the Lead).
- `wt disk --lanes` reports build-lane sizes, owners and whether each lane can
  be reclaimed; lanes with unresolvable ownership stay explicit.
- Terminal search rows say which tab and program a line came from and how old
  it is; Enter jumps to the matched line and `n`/`N` step through matches.
- PR queue and CI-autofix refusals are stored with their reason.
- Preview can open the selected development app with its own hot reload.
- A Windows sandbox wire-protocol crate (`thegn-sandbox-proto`) and the
  programme release-channel feature flags.

### Changed

- Terminal history search scores matches on a worker thread, streams results
  as they are found, and keeps earlier results when you press Enter before the
  search finishes; the event loop no longer stalls on long histories.
- `oci_runtime` is passed as `--runtime` only to podman and docker; other OCI
  engines say the value is ignored, and doctor still probes the runtime for an
  `auto` backend.
- The libkrun runtime probe checks read-write access to `/dev/kvm` on the host
  that runs the engine, and only for local Linux placements.
- Remote bwrap, systemd and provider OCI panes no longer put host-sourced
  secrets on the command line; ssh and Kubernetes OCI creates send them over
  stdin.
- macOS no longer offers the rootful podman sandbox backend.
- The seven crates without a package description now have one.

### Fixed

- Closing the last pane of a nested stack removes the emptied stack from its
  parent split instead of leaving a blank region.
- The GPU fallback sparkline keeps its selected interval.
- Broken intra-doc links that failed the CI doc job.
- Two load-sensitive host tests wait on observable conditions instead of
  fixed sleeps.
- The Daytona config example no longer names an unverified CLI command, and
  `config validate` network-audit warnings are covered by tests.

### Documentation

- Contributor notes on unconventional host test assumptions, scoped
  implementation and history searches for agents, and corrected optional-CI
  guidance in the PR template.

## [0.1.0-alpha.126] — 2026-11-23

### Changed

- Closing a pane focuses the pane you used most recently in that tab instead
  of whichever pane the layout happened to leave. Closing a pane you are not
  in leaves focus where it is.
- Egress host patterns share one grammar: exact (`=host`), subdomain and
  wildcard forms, ports, and IP literals, matched on label boundaries.
  Percent-encoded or non-hostname characters are refused, and a block entry
  without a port blocks every port.
- The egress proxy control protocol and audit record shapes are frozen for
  v1 (server challenge, lease reply, additive-only enums); credentials are
  never serialised and audit paths drop query strings.

### Fixed

- Apple container sandboxes refuse `network = "host"` as well as
  `network = "none"` instead of passing a keyword the runtime does not honour.
- A Sprites job test no longer hangs for 45 s under load: the test server
  reads the client's close before dropping the connection.

## [0.1.0-alpha.125] — 2026-11-23

### Added

- A new pane that is still waiting for its shell prompt shows a small,
  dim progress card after a short grace period: thegn's own stage
  (resolving environment, starting sandbox, starting shell, waiting for the
  prompt) and the processes the shell is running, each with elapsed time, plus
  a hint when a known slow step (direnv, a version manager, prompt setup,
  an agent waiting for a passphrase) is recognised. Arguments are redacted.
- Selected build and test jobs can run on a project-bound Sprite
  (fixture-backed executor): one dispatch per attempt, a durable attempt
  ledger, a scrubbed environment and private home per attempt, a deadline that
  kills the remote process group, secrets kept out of the transferred
  snapshot, and one project per sprite. Not wired to terminals yet.

### Fixed

- `thegn env down --stop` on a Vercel sandbox sends the JSON body Vercel
  requires instead of failing with HTTP 415.
- `thegn fleet cost` marks a fresh sprite's zero usage as pending instead of
  reporting a confident 0.0.
- Opening a pane no longer reads the state database on the event loop, which
  removes frame stalls of up to ~150 ms during spawns.
- The remote-parity TUI probe runs against a throwaway config and can no
  longer write its ready marker into the owner's real config.

## [0.1.0-alpha.124] — 2026-11-23

### Fixed

- Clicking the Work or Observe tab in the masthead switches views again. A
  click on a tab used to start a sidebar resize instead, because the resize
  edge was matched by column only and sits under the tabs at the usual sidebar
  width.
- Docker and Podman are declined on Windows only for local placements, so a
  Windows host can use them on an ssh, connector or provider placement.
- Sandbox status words recognise Windows paths and `.exe` names (for example
  `C:\...\docker.exe`, `wsl.exe`).
- The hermetic Sprites exec replay no longer flakes with "exec stream ended
  without an exit status": the test server reads the client's close before
  dropping the connection.
- Vercel session usage comes from the session's own figures.
- A sandbox test that read `$HOME` while other tests changed it is hermetic,
  so the coverage job passes again.

### Changed

- Help pages, the README and the config example no longer over-claim what the
  sandbox enforces. A new **Sandbox security model** help page describes the
  current protections and their gaps per profile and backend.
- Builder-style methods that return `Self` are `#[must_use]`, and the
  workspace lint denies new ones without it.
- `ControlClient` methods document their error cases.
- `.gitignore` ignores `.env` files, private keys, credential files and
  editor/tool caches.

### Added

- The job spec gains a typed selected-runner route (the contract for sending a
  project's build or test to its chosen runner); it is not wired to terminals
  yet.

## [0.1.0-alpha.123] — 2026-11-23

### Fixed

- A text selection in a pane disappears when that pane is resized (a new
  split, a closed pane, zoom, the drawer, or a window resize) or leaves the
  screen. It used to stay highlighted over whatever text had reflowed under it.
  The selected text is still copied when you release the mouse.

## [0.1.0-alpha.122] — 2026-11-23

### Fixed

- The release package step accepts the upload step's generation record among
  the downloaded assets, so packaging no longer stops on an "unexpected" file.
  alpha.120 and alpha.121 were tagged but never published; alpha.122 carries
  their changes.

## [0.1.0-alpha.121] — 2026-11-23

### Fixed

- Release uploads resolve the draft release by listing releases instead of the
  by-tag endpoint, which never returns a draft, so a release run no longer
  stops at the upload step. alpha.120 was tagged but never published because
  of this; alpha.121 carries the same changes.

## [0.1.0-alpha.120] — 2026-11-23

### Added

- Development builds support direct Iroh pairing using explicit peer addresses
  and scoped invitations.

### Changed

- Pipeline-owned worktrees appear once under Pipelines by default. Explicit
  folder assignments take precedence, and the flat layout remains available.
- Commands that now refuse, and what to run instead:
  - `wt rm` and the TUI delete pull the sandbox's commits first and refuse on a failed pull or a dirty sandbox; use `--force` (or the TUI force delete) to destroy anyway.
  - `env down` refuses a dirty sandbox or failed pull; use `--force`, or `--stop` to park it.
  - `env deprovision` refuses sandboxes bound to a registered worktree (`--all` skips them); run `env down --worktree <wt>` or `wt rm <wt>` first.
  - `env rm` refuses while a registered worktree uses the env; tear the worktrees down or `env set <env> --worktree <wt>` first.
  - `merge land` and the TUI land pull first and refuse a queued candidate the pull made stale; re-queue with `thegn merge add` or run `thegn land`.
  - `wt prune` keeps worktrees bound to a provider sandbox.
- `env set` and `env restore` pull first and name what they leave or discard; closing a worktree names the sandbox it leaves.
- `land --release-sandbox` re-checks right before releasing and keeps a dirty sandbox.
- Sprites lifetime reapers keep sandboxes bound to a registered worktree; background passes never wake a sandbox, and the hibernator never execs a sleeping sandbox unless a listing shows it awake.
- Non-tty `sprite-exec` streams without a cap, sends remote stderr to stderr and forwards a non-terminal stdin.
- `test/remote-parity.sh` prints a `RUN SUMMARY` (THE-2477).
- Native local interactive sandboxes on NixOS now get host Nix-daemon access when safety checks pass. Agent, SSH, provider, and OCI launches stay off by default; explicit `nix_daemon = false` opts out (THE-2478).

### Fixed

- Work and Observe tabs respond to mouse clicks and keyboard navigation.
- Native macOS checkout-hook inspection handles an exited process-group leader
  while preserving checks for live descendants.
- Native CI shells include the packaging and release smoke test dependencies.
  Linux-only process diagnostics and feature-only helpers are compiled where
  they are used.
- Native macOS builds no longer fail on the per-process activity test or dead-code lints for Linux-only media and process-sampling helpers (THE-2479, THE-2482, THE-2483).

## [0.1.0-alpha.119] — 2026-11-22

### Fixed

- A published release's manifest binds `SHA256SUMS` like every other asset, so verifying a download no longer fails at the manifest step.

## [0.1.0-alpha.118] — 2026-11-22

### Fixed

- The documentation build passes again: doc links that could not be resolved are plain code text (THE-2389).

### Security

- `url` is updated to 2.5.9, which closes a stack overflow in `Url::origin()` on nested `blob:` URLs (RUSTSEC-2026-0592).

## [0.1.0-alpha.117] — 2026-11-20

### Changed

- The Sprites live check's wake check after a cost read (step 10b) fails only if the sprite stays running, and warns on a single running reading or an unreadable provider listing (THE-2476).
- A failed agent step in the Sprites live check says why: the agent's status, exit code, error and last output lines, never secrets.

### Fixed

- Four doc comments that broke the documentation build.

## [0.1.0-alpha.116] — 2026-11-20

### Changed

- `thegn land` refuses, naming the sandbox, when the provider cannot say whether a worktree's sandbox exists.
- The daily remote live check requires land to report releasing and pulling, and no longer depends on the owner's merge gate command.

### Fixed

- `thegn land` on a remote worktree that was never opened in a pane now pulls the sandbox's commits first and honours `--release-sandbox`; before, it landed without pulling, left the sandbox running and said nothing (THE-2477).
- `--release-sandbox` is never dropped silently: when it cannot act, land says why.

## [0.1.0-alpha.115] — 2026-11-20

### Added

- `test/remote-parity.sh` gains `--startup-only`, `--fill-spare` and `--tui-probe` for startup timing, a warm-spare claim and a full-screen-program probe.
- The remote agent install logs per-agent seconds to `~/.cache/thegn-agent-install.log` in the sandbox.

### Changed

- Remote panes on Sprites state their size right after connect, so the first `stty size` is no longer `0 0`.
- The `sprite-exec` bridge batches queued input into one write (up to 32 KiB); a lone keystroke and a lone Escape still travel alone.
- Local echo prediction retires only the keys the server echoed and drops stale ones after 2 s.
- The agent install runs one `npm install -g` for all missing npm agents.

### Fixed

- `session open` prints an id only for a session the daemon holds, a stage-launch replay of a vanished session is refused, and `session attach` exits non-zero when the session vanished.

## [0.1.0-alpha.114] — 2026-11-20

### Added

- The shared provider conformance suite gains four opt-in cases any remote provider can back: a command stream without an exit status fails; a call refused because the session stopped is retried exactly once; an unfinished listing is never reported complete; an egress allow list never widens to allow-all (THE-2351).
- A contributor page, "Add a remote sandbox provider", lists each rule with the test or live step that enforces it.

### Changed

- The live checks for Sprites, Vercel and the daily remote run share one step runner and print per-step times and start-up times (THE-2351, THE-2223).
- The Sprites memory step reports the guest's available memory and gives a clear verdict on whether the limit is enforced.

### Fixed

- The Sprites live check's agent step starts its own pane daemon (it failed at once before).

## [0.1.0-alpha.113] — 2026-11-20

### Added

- Remote worktrees on every provider: the shell opens as soon as the workspace is ready; agent install, agent logins, shell history and the snapshot finish in the background with progress on the status line. An agent launched meanwhile waits for its CLI and login. `thegn env up` still waits for everything, and `--shell-ready` returns early. An interrupted or failed background phase resumes on the next open and shows in `thegn env show`; `env down` and deleting the worktree cancel it. On Vercel the agent install and snapshot now also run after the prompt.
- `test/remote-parity.sh` (`just remote-parity`) checks a remote pane against a local one; it takes `--env`, has a `--self-test` mode with a local pane on both sides, and only prints its plan without flags.
- `thegn session snapshot --json` and MCP `sessions_snapshot` report `mouse`, `sgr_mouse` and `title`.

### Changed

- Remote panes get `TERM=xterm-256color`, `COLORTERM` and, when the host locale is UTF-8, `LANG=C.UTF-8`; `sprite-exec` keeps a host `TERM` the remote is sure to know.
- Closing a remote pane ends its remote shell; quitting thegn or a dropped connection still detaches.
- Interactive remote connections check liveness after input: a ping after 5 s, a reattach after 5 s more. It only acts on a connection that has answered a ping. `TCP_NODELAY` is set on the exec and proxy sockets.
- Local echo prediction on a remote pane is drawn only after the server echoed an earlier key of the same line and only when the echoed cell holds the typed character, so nothing shows at a password prompt.
- Agent login sync has phase-2 allowlists for pi and hermes, skips dependency trees for every agent, and ships only top-level files for an unknown agent with more than 2,000 allowlisted files, with one warning. Hermes auth files move into phase 1.
- The Sprites quirk that prints the tty line without its line ending is documented (THE-2450).

## [0.1.0-alpha.112] — 2026-11-20

### Changed

- Sprites command streams are checked against the frame protocol: a malformed exit, two exits that disagree, output after the exit or an unknown stream is a typed protocol error and the command fails; a repeated exit with the same code and control messages of an unknown type are accepted, as the live server sends both (THE-533).
- The Sprites live check proves whether the memory limit is enforced by allocating past it (step 5d).
- The VPS provider's destroy is now covered by the shared conformance case that keeps the record until the server is confirmed gone (THE-530).

### Fixed

- After a crash during cleanup, the merge-queue row of a remote worktree with no registry record is kept and retried instead of retired (THE-236).

## [0.1.0-alpha.111] — 2026-11-20

### Added

- Right panel: with `[lead]` configured (dev channel), a pinned Lead footer shows the Lead's state, pending approvals across all repos (click to answer them), lane and spawn budgets and the last action; the sidebar LEAD row gains a state suffix.

## [0.1.0-alpha.110] — 2026-11-19

### Fixed

- Vercel: streamed commands, interactive shells and file transfers refused because the session had stopped are retried once after resume, like one-shot commands.

## [0.1.0-alpha.109] — 2026-11-19

### Added

- `thegn session send --key` presses named keys (`up`, `esc`, `f5`, `ctrl+c`…) and `--paste` sends bracketed paste; `session wait --until` takes `screen:<regex>` and `stable:<ms>`; MCP `sessions_input` takes `keys` and `paste`, and `sessions_snapshot` reports the cursor and screen modes. Who may type is unchanged (THE-2475).
- Sidebar row menu: "Info…" opens the Info screen for the row.

### Changed

- Sprites restore reads the checkpoint first and refuses a missing or unhealthy one. `fleet cost` marks a sprite younger than an hour `recent`, as its usage may not be reported yet (THE-2451).
- Observe: a partial window says how much it covers, e.g. `partial: last 3m of 15m`.

### Fixed

- The reaper no longer retires worktrees whose location is remote or whose effective environment is ssh, k8s or a provider (THE-2472).
- A deleted worktree no longer reappears in the sidebar from an older refresh (THE-2470).
- Info explains against the `--config` path the TUI started with, and shows a failed state-database read as "unknown" instead of unset.

## [0.1.0-alpha.108] — 2026-11-19

### Changed

- The Vercel live check proves that a stop and a resume reached the provider.

### Fixed

- Vercel: a one-shot command refused because its session had stopped is retried once after resume.
- Vercel: stopping or killing something already stopped is success; an expired saved snapshot gives a clear refusal with the remedy, and nothing is deleted automatically.
- Vercel: a snapshot listing that does not finish is never treated as complete: destroy keeps its record and can be retried, and cost reports are marked incomplete; snapshots of a sandbox that is gone are priced.

## [0.1.0-alpha.107] — 2026-11-19

### Added

- Info screen: `thegn info` (with `--workspace`, `--worktree`, `--json`), the sidebar key `I` and the palette action "Info screen" show what is in force for a workspace or worktree, which layer set each value, and which sandbox backend would launch, using the same rule as a real launch. Remote and provider placements are never probed.

## [0.1.0-alpha.106] — 2026-11-19

- Docs: help pages for Sprites checkpoint and restore ledger proof, URL auth pinning, the create-time memory policy, `fleet inspect`, the `provision` field of `agent run --json`, the drain warning, the Fly destroy check, `sandbox warm-devshell`, the masthead instance tag and the "Retry failed pull-backs" action. `env snapshot` and `env restore` now refuse a sprite another instance made for the same worktree path.
- Sidebar: a worktree deleted from outside the running TUI (wt rm, wt prune, a landing, plain git) no longer stays in the left bar; a leftover directory with no checkout in it is no longer treated as a live worktree.
- Observe: the host dashboard now uses the masthead's samples, so it has history from launch and no gaps while the tab is hidden, and no longer runs a second sampler.

## [0.1.0-alpha.105] — 2026-11-19

### Changed

- New worktree checkout directories end in a 16-character digest instead of 64. Existing checkouts are not moved.

### Fixed

- Vercel sandboxes: the egress policy is sent in the shape the API accepts; one-shot commands request their output; streamed commands can receive stdin; the interactive shell sends its environment in the documented form.
- Vercel: an egress change now also reaches the running or starting session; snapshot listings read every page; a pending or failed session is no longer shown as running; extending a session that is gone is an error, not "at the plan ceiling"; CPU usage is shown while a session runs.
- An issue plugin's update is checked against what was asked; oversized listings are reported as partial; duplicate issue ids in a reply are refused.
- A config file rewritten with the same length during a capture is detected by comparing bytes.

### Security

- A plugin manifest is opened without following symlinks, and a manifest listing a capability twice is refused.

## [0.1.0-alpha.104] — 2026-11-19

### Added

- `thegn env pool-fill [--worktree W] [--size N] [--json]` fills the warm pool in the foreground and can persist the per-project size (0 to 8). It never destroys, and refuses Vercel and non-provider envs. A claim from a CLI or daemon process no longer refills in the background; it prints a hint, and only the TUI refills.
- `thegn fleet cost` adds one month-to-date account row per distinct Sprites account, deduplicated by account fingerprint and marked partial when the API says so. `thegn doctor` shows a warm pool line per provider env.
- Terminal search adds `Alt+c` (exact case), `Alt+w` (whole word), `Alt+r` (regex) and `Ctrl+r` (recent queries); `Alt+f` stays fuzzy.

### Changed

- Sprites reuse is fail-closed: a sprite that already exists is reused only when this instance's ledger proves it created it, otherwise thegn refuses and names `thegn env adopt`. After this change the dev instance refuses sprites the stable instance created for the same worktree path.
- The Vercel exec keeps its session alive while a command runs (a check at the start and every 5 minutes); a failed check never fails the exec.
- The live scripts and the net trace stop overriding `XDG_RUNTIME_DIR`. `devel-launch` puts the dev binary's directory first on `PATH`, and `thegn doctor` names a `PATH` mismatch.
- The `harden-live-upgrades` change is retired as superseded by `just live`.

## [0.1.0-alpha.103] — 2026-11-19

### Changed

- `just live` no longer touches the real installation: it builds if changed, installs to `~/.thegn-devel/bin/thegn` and starts the dev instance, keeping the dev daemon unless `--restart-daemon` is given. `--relink-launchers`, `--install-dir`, `--yes` and `--no-scope` are refused with a pointer at home-manager. The first dev launch seeds `~/.config/thegn-dev` from stable's `config.toml` with the serve and model-proxy ports shifted by +1.
- Stable is installed from the published gnu release archive through `legacyPackages.<system>.releasePackage`; the home-manager module gains `manageConfig` (default `true`). The README, `RELEASING.md` and `thegn help two-instances` describe the new flow; `docs/live-upgrades.md` is now a short pointer.

## [0.1.0-alpha.102] — 2026-11-19

### Changed

- The dev instance no longer overrides `XDG_RUNTIME_DIR`: its daemon socket moves beside the stable one under the login runtime dir and both mask it, and rootless podman shares the login runroot. `scripts/devel-launch.sh` scrubs pane identity from its environment and gains `--daemon` to start the dev daemon detached. New `just pipeline-tools` and `pipeline-tools-check` install and show the lint tools for pipeline workers (`.#lint-tools`). `thegn doctor` adds "runtime exposure" and "share teardowns pending" rows.
- Fresh provider-placed worktrees can claim a warm spare (opt-in pool). `agent run` warns once when the agent CLI is missing in the sandbox, prints a progress line every 60 s under `--wait` on a terminal, adds exit hints for 75 and 127 (`exit_hint` in `--json`), and prints a wake notice when opening a sandbox takes over 3 s.
- Vercel `env down` keeps the record until `GET` returns 404 (30 s proof); one exec may run up to 31 minutes, and a stream with no exit status is an error. `thegn doctor` remote rows add `placement`, `placement_warning`, `sandbox_agents`, `max_instances` and `max_lifetime_secs`.
- Live scripts: sprites-live adds 4b, 10w and opt-in `--with-agent`; remote-daily adds 11m, 11k and 11j; vercel-live adds 3s, 7s and 4b. All stay plan-only without spend flags. Vercel live remains UNVERIFIED (THE-2234).

### Fixed

- A newly added project opens and its terminal is focused at once, instead of waiting for the next background refresh.

## [0.1.0-alpha.101] — 2026-11-19

### Changed

- `just devel`, `just devel-env` and the new `just devel-desktop` share `scripts/devel-launch.sh`, which keeps a relative `WAYLAND_DISPLAY` and the session D-Bus address working under the dev runtime dir. `devel-desktop` prints a desktop entry and writes nothing.
- `thegn repo forget --copy-from <DB>` seeds an empty instance from another instance's database through a verified backup, then forgets. `just live` refuses from a shell that carries another instance's config, state or `THEGN_DIR`, and names the path and the fix. `thegn doctor` reports the runtime dir, controller id, bind addresses and a relative-`WAYLAND_DISPLAY` note.
- `thegn env sync-retry [--json]` retries failed provider pull-backs without destroying a sandbox; a failed pull-back on close now warns and names it.
- `fleet cost` starts the Sprites usage window at the ledger `created_at` floored to its UTC hour and shows "no usage reported yet" instead of 0.00 when nothing was reported. Live step 6c is a WARN that names THE-2450 (upstream line-ending quirk); `test/sprites_exec_repro.py` reproduces it.
- Search output is unchanged but faster: results are grouped before sorting, only the top `max_results` are fully sorted, and growing literal queries re-scan only the previous hits.

## [0.1.0-alpha.100] — 2026-11-19

### Changed

- Fleet reads a recorded sandbox the provider does not list as `down` unless this host still holds its provisioned marker (then `missing`), and a sprite with no worktree and no ledger record as `unrecorded`. Neither turns the masthead badge amber. Budget admission still counts every listed row. Not yet verified live.
- `thegn env reap --provider sprites --unrecorded` is a read-only listing (oldest first, status, age, 30-day usage) with one `env adopt` line per paste-safe name; it refuses `--apply`. The Sprites list API reports no size.
- Seed, pull and push-lane events append to `$XDG_STATE_HOME/thegn/provider/git-audit.jsonl` (rotated at 1 MiB); a failed Vercel exec appends the last 8 KiB of stderr; `SpritesProvider::with_list_page_size` adds `max_results` to list pages for a read-only live paging test. The live script gains steps for these.
- With `THEGN_LOG` set, `THEGN_TRACE_FILE=<absolute path>` sends stdio bridge verb traces to that file only, never stdout or stderr.
- `test/sprites-live.sh` takes `--keep-going` (a failed step prints `FAIL <id>` and the run continues, with a summary at the end), proves pull with a real commit made inside the sprite, and prints the cost JSON. Not yet verified live.
- Sprites usage is read from the top-level `usage` array (`buckets` stays as a fallback). A non-tty exec exit frame may carry the exit code as one binary byte or as ASCII; both are accepted.
- `env down` on a sandbox that is already gone says so and clears local state instead of warning that commits are lost.
- Interactive exec output sometimes lost a line break. The cause is not yet known: this adds a trace to find it and a defensive fix to the one client-side place bytes can vanish. It is not claimed as a fix.

## [0.1.0-alpha.99] — 2026-11-19

### Changed

- Provider pull-back is journaled: a restart, `wt rm` or another process pulls a `data = "sync"` sandbox back using the journal, a failed pull keeps the sandbox, and `thegn doctor` shows a pending row. `sprite-exec` routes to the account that owns the sprite and refuses when ambiguous; `vercel-exec` capture mode refuses likewise. A failed Sprites one-shot exec appends the last 8 KiB of stderr. Not yet verified live.
- Search rows are labelled worktree · tab · program, the overlay lists recent lines on open, identical lines from one pane are grouped, and Enter scrolls to the exact line or points to replay when it has left the grid.
- Merge integrate and land record provenance (source tip and target base for integrate, the admitted candidate for a ready land, none for a manual land). Remote and macOS placements keep the requested `oci_runtime`; krun needs read-write `/dev/kvm` on a local Linux host.
- Search is literal smart-case by default, ranked by match quality then recency; `Alt+f` toggles fuzzy. The `profile` scope leaves the `Tab` cycle.
- `[env.<n>.provider] model_credentials` is now valid for Sprites (`none` or `upload`). `none` withholds model credentials and agent logins; unset behaves as before. Not yet verified live.
- Sprites destroy now counts only after a definite 404 from polling the sprite; otherwise the record is kept and destroy fails so a retry can finish. Provider exec has an overall 31 minute deadline. Closing a worktree resolves its location and env before forgetting it.

## [0.1.0-alpha.98] — 2026-11-19

### Added

- `thegn env adopt <sprite> --env <S>` records an existing sprite after you confirm it, so thegn can manage it. `[env.<n>.provider] lifetime_reap = true` lets a running client destroy recorded sprites past `max_lifetime_secs`. `thegn land` on a provider worktree now pulls and lands the local branch and keeps the sandbox; `--release-sandbox` destroys it. `agent run --wait` stops a Fly or machine0 machine afterwards unless `--keep-running`. `login_homes` selects a dedicated login for Sprites and Vercel `upload`. Not yet verified live.
- The daemon has a native inbox: `[inbox]` and `[[inbox.accounts]]` poll Gmail or any IMAP server (app password), Todoist, or any command (himalaya, a Jira JQL script) and record new items as `inbox_item` notifications. Credentials are secret refs resolved only in the daemon, item text is sanitized and untrusted, and the first poll of an account is a baseline that fires nothing. `thegn inbox list|test|status`, `inbox_item` automation rules (capped at `review`, no `{message}` in spawn templates) and the `producers.status` verb are new; the Lead reads items through `notifications.list`. Requires `[daemon] headless_producers`. Schema v116: restart the daemon. Not yet verified live.
- `[autopilot]` now runs as a synthesized `autopilot:<repo>` rule through the new config-only `autopilot.run` action, and automation budgets count autopilot runs and the Lead's manual spawns. Pausing automations also stops inbox polling. Not yet verified live.
- The sidebar shows at most 7 entries per group (`[ui] sidebar_group_row_limit`, was 15). The active worktree, the cursor row, pinned rows and rows that need you are never folded. Worktrees sharing a batch token such as `b158` fold into one collapsed batch row (`[ui] sidebar_batch_group_min`, default 2, `0` = off) with a rollup of needs-you, active and done counts. A row drops a second line that repeats its title, worktree names are toned by state, and the tree guide is continuous through two-line rows and closes at the show-more row. Not yet verified live.

### Changed

- `thegn wt rm` now refuses to delete a worktree whose provider sandbox it cannot confirm gone (no token, destroy not confirmed by a complete inventory, or a provider location under a non-provider env), naming the sandbox and the two ways out. `land --release-sandbox` clears the location only after confirmation and otherwise prints `! sandbox <name> kept: <why>`. Closing a provider worktree now says its sandbox is kept. Keep-awake no longer releases a hold it never got. `thegn doctor` gains a "Remote live readiness" block.
- `[ui] confirm_quit` (default true): `Ctrl-q` asks first; press it again to quit. Daemon panes survive either way.
- The background LOC and size scanners now go idle. A background worktree's line count is redone only when its git state moved (worktree root, `HEAD` or `index`), not every 15 minutes; the worktree you are in keeps the 900 s TTL, is recounted at most every 60 s while you edit, and is recounted at once when you switch to it with an older count. A file-watch event now counts only the worktree you are in (it used to plan every stale worktree), and a switch or create counts only that worktree plus never-counted ones. Between sweeps neither scanner opens the database. The size of the worktree you are in is `du`d at most every 120 s. Config keys and defaults are unchanged: `max_scan_per_round` still bounds walks, while the number of worktrees a round may examine (a few `stat`s each) adapts to the registry. Steady state with defaults: `loc` at 50 worktrees is 1 round per 15 min (about 300 `stat`s), at 222 two rounds (about 1,300 `stat`s) then idle, at 1,000 two rounds (about 6,000 `stat`s); `disk` at 50 worktrees is 1 round per 45 s (about 9 `stat`s/s), at 222 about 40 `stat`s/s, at 1,000 two rounds per 90 s (about 90 `stat`s/s). A machine with many worktrees went from about 8 tokei walks a minute, continuously, to one walk per worktree whose git state changed plus the one you are in. The tokei pool shrinks from up to 4 threads to 2, and the per-profile "skipped"/"not_due" prune lines drop to DEBUG. The "lane can never go idle — raise `max_scan_per_round`" warning is gone.
- The state database now maintains itself. The UI (never the daemon) converts a legacy database of up to 256 MiB to incremental auto-vacuum, gives free pages back in small steps, applies bounded retention and sweeps stale state files (`pty-shutdown/` keeps at most 500), at most every 10 minutes behind a lock so two UIs never both maintain. A larger legacy database gets one toast asking for `thegn db vacuum`, which is now an escape hatch that takes the same lock and refuses with `LowDisk` when space is short. `thegn doctor`'s db space line ends `; auto-maintained`.

## [0.1.0-alpha.97] — 2026-11-18

### Added

- Sprites environments are now safer to run: thegn records a sprite in its ledger before creating it and deletes only sprites it recorded. `thegn env reap --provider sprites [--apply]` lists orphans, sprites past `max_lifetime_secs` and sprites "not recorded by this host" (older sprites, or ones made outside thegn, are never deleted by thegn; remove them with the Sprites CLI or dashboard). Headless work on a sprite holds it awake up to a ceiling (4 hours by default), `eager` no longer creates sprites in the background, `min_ram_gb` sets the memory limit, `fleet cost` gains a Sprites usage block, and `thegn agent run --wait --pull` pulls results back. Not yet verified against the live Sprites API.
- `thegn repo forget` removes projects from a stopped instance's registry, for splitting one instance into two (see the `two-instances` help page). Name the projects to forget, or pass `--keep <PATH>` to forget every other one. It is a dry run unless you pass `--apply`, which backs the database up first. It refuses while the instance is running and never touches files, git worktrees or branches.

- The daemon can run the tracker, PR, mention and PR queue producers with no window open: `[daemon] headless_producers = true` (default off). Each armed kind holds a lease so the UI yields while the daemon polls; the PR poll covers at most 8 worktrees per tick, and `thegn lead pause` stops daemon PR queue work too. `thegn doctor` shows producer status. Not yet verified live.
- `thegn lead login [--mcp <server>]` logs the Lead's agent, or one of its MCP servers, in from a fresh temporary directory and copies back only the credentials or token files. The Lead cannot influence it, and your `~/.mcp-auth` and `~/.npmrc` are masked inside the Lead.
- `[merge_queue] gate_sandbox = "sealed"` runs the merge gate in a sealed sandbox (no network, only the gate checkout and its target dir writable), so a `merge` autonomy land can proceed unattended. It is off by default and unattended merge stays off unless you set it. A gate that needs the network or the Nix daemon cannot run sealed and still asks for approval. Not yet verified live.

### Fixed

- The UI's mention check now runs every 5 minutes per repo instead of on every refresh.

### Changed

- The local database migrates forward to schema version 114 (producer leases, THE-2208); restart the daemon after upgrading.

## [0.1.0-alpha.96] — 2026-11-18

### Added

- `just devel` starts a separate dev instance from the checkout (own state and runtime under `~/.thegn-devel`, `THEGN_DIR`, and `~/.config/thegn-dev`), and `just devel-env` prints its environment. The new help page `two-instances` explains the setup and what the two instances still share.

## [0.1.0-alpha.95] — 2026-11-17

### Added

- The Lead: a configured agent CLI in a global tab, pinned as the `LEAD` section at the top of the sidebar. Set `[lead] agent` (dev channel only; restart the daemon after adding it) and press `Alt i` (`jump-lead`) to open it. Each tab is its own conversation; the group survives workspace switches and detach. `[lead] account` is refused for now, and only claude is supported as the Lead's agent.
- The Lead's tool surface over `thegn mcp serve`: `overview_get`, `notifications_list`, `notifications_ack`, `automations_put`, `automations_rm`, `automations_enable`, `automations_runs`, `automations_pause`, `approvals_list`, `approvals_request` and `merge_drain`, each gated by its scope, plus opt-in notice event frames. `automations_resume` is never a tool: only you can resume. The Lead never grants an approval.
- Lead confinement: a Lead token may open only a configured agent in a registered, sandboxed worktree; type plain text into workers it opened; and read, work the tracker and dispatch roster, and ask for approvals. It may not call `merge.add`, `merge.drain` or `worktrees.create`, run host git, or change a sandbox profile, whatever its scopes. Outward calls (`issues.comment`, `issues.update`) ask you first unless listed in `[lead] outward_allow`.
- The Lead's sandbox: repositories read-only, a private agent home, thegn state, config and credentials masked, and an isolated network through `pasta` (`[lead] network = "isolated"`, the default; `"host"` warns on every launch). It refuses to open without bwrap, or without pasta in isolated mode. `[mcp_proxy.profiles]` gives the Lead its own MCP servers, whose secrets only the Lead's token can fetch. The Lead's token expires after `[lead] token_ttl_secs` (default 12 hours) and slides while in use. While `[lead]` is set, every pairing code needs `thegn pair approve`.
- Automation triggers: `schedule_due` (cron or interval, daemon only), `issue_matched` and `session_exited`; rule actions `pipeline.start` and `lead.wake`; a per-rule `autonomy` ceiling of `propose`, `review` or `merge`. `thegn automations add|rm|runs|pause|resume` and a richer `list`.
- The stage executor drives an automation-started lane through its stages, runs validation in the lane worktree's sealed sandbox, then advances or lands it. A worktree without a sealed sandbox records an error and runs nothing.
- Approvals appear in a group at the top of the Notifications panel: `y` approves and `n` rejects, each asking for a second press. Each new request is pushed to your phone with a single-use code; the signed push inbox answers it with `approvals.redeem` (24 hours, one use). Agents and the Lead cannot redeem.
- Budgets: `[automations] max_concurrent_lanes` and `max_spawns_per_day` bound what automation starts on its own; `0` means nothing may spawn. `thegn lead pause` (kill switch: pauses rules, kills the sessions automation and the Lead opened, cancels executor validation; `--keep-sessions` keeps them), `thegn lead resume` and `thegn lead status [--json]`.
- Vercel Sandbox: `provider = "vercel"` with `team_id`, `project_id` and `api_key_env`. thegn drives it over Vercel's HTTP API and a hidden `thegn vercel-exec` bridge (no SSH): terminal panes, headless agent lanes, repo copied in as a git bundle, work pulled out with `thegn env pull`. `max_instances` (default 2) and `max_lifetime_secs` (default 24 hours) cap it and a reaper stops and later deletes sandboxes; `thegn env reap --provider vercel` lists orphans (dry run) and deletes them with `--apply`. `thegn fleet cost` shows an estimated Vercel cost for the current session.
- Vercel `model_credentials = "none" | "upload"` (default `none`; `proxy` and `inject` are refused). `upload` copies claude or codex login files into the sandbox's memory-backed storage after the egress policy is narrowed to the model hosts. `interactive_host_suffixes` pins the interactive host. When a session ends with results not pulled, a marker shows on the Environments row and in `thegn doctor`; `p` on the row pulls.
- `thegn doctor` lists nine more release-channel features, all experimental and not yet implemented: `sandbox_backends`, `linux_confine`, `egress_proxy`, `seatbelt`, `win_isolation`, `plugin_backends`, `agent_presets`, `private_copy` and `escalation`.

### Changed

- The sandboxing help page is split into topic pages (backends, network, profiles, resources, devcontainers) and a new environments page; the panel environments help moved there.
- Egress and capability reporting says only what the code does: provider capabilities derive from per-kind facts, the sandbox matrix labels the network cell advisory (Apple unverified), `network_audit` documents that it needs podman and warns without it, and a provider placement warns that hostname rules are not enforced.
- Apple `network = "none"` is refused as not expressible. The `smol` and `appcontainer` backends are reserved: config accepts them, they are never selected. The VPN `filter-front` DNS mode is reserved and behaves as the default resolver.
- On the stable channel, Kubernetes environments are removed.
- `thegn doctor` marks Daytona as maintenance and unverified against a live account; Fly reach is described as plain ssh to the dedicated IPv4. ssh sharing (ControlMaster) is off on Windows and for long socket paths.
- bubblewrap and passt ship in the Nix package as fallback runtime tools, so the Lead's sandbox and isolated network work without a system install.
- Known limit: in this release `merge` autonomy behaves like `review`, so every land stops for your approval. The merge gate runs the lane's own code on the host, outside any sandbox, and is not sandboxed yet. Unattended validation needs a sealed sandbox.
- Known limit: the Vercel provider has never been run against Vercel; it was built against a mock. Unverified until a live run: the interactive PTY WebSocket (panes fall back to a stream mode), sandbox resume, name and tag limits, token scopes, whether upload files stay out of the auto snapshot, 429 behaviour and snapshot deletion. The kind is experimental. Cost is an upper-bound estimate for the current session only.
- The local database migrates forward to schema versions 112 and 113 (111 is listed above); restart the daemon after upgrading.

### Fixed

- Sandboxed agents could intermittently fail to commit with "cannot lock ref: unable to create directory": git removed the ref directory a running sandbox had mounted. Each writable ref carve-out now holds a hidden `.thegn-keep` directory.

### Removed

- The unwired DNS filter server and its gate.

### Security

- Lead confinement is enforced in the daemon for every call made with a Lead token, not in the Lead's sandbox alone.
- Vercel API tokens are sent only to Vercel and never enter the VM; the interactive host is pinned to Vercel suffixes and OIDC tokens near expiry are refused.

## [0.1.0-alpha.94] — 2026-11-17

### Added

- From the cpu/mem popup, Enter on a process opens the system monitor filtered to it, and `p` shows a worktree's or pane's processes; the monitor's process filter also applies `wt:` and `wt=` (`user`, `s` and `thr` are still not available). (THE-2025)
- Drag a file from the Files panel onto a terminal pane to insert its quoted path; drops are refused while the center shows a splash or is dormant. (THE-135)
- The theme builder's element view keeps the selected part of the workspace lit behind the dimmed modal. (THE-2023)
- `thegn usage [--json] [--tokens]`. (THE-138)
- `/` filters the git panel's Files view. (THE-787)
- In the git panel's Files view, `S` stashes only staged changes (including partially staged files; it refuses only when nothing is staged) and `U` only unstaged changes. (THE-787)
- `[metrics]` edits apply live: targets restart on save, with no thegn restart. (THE-2024)
- `[monitor]` `window_ladder`, `default_window`, `default_style` and `default_scale` apply to the next monitor open after a save. (THE-2024)
- Home Manager options for `[weather]` (except the API key) and `limits.test_nice`. (THE-428)
- The Lead can write its own automation rules with `automations.put`, `automations.rm`, `automations.enable` and `automations.runs`. These rules may only notify or open a session; `merge.add` and `tools.run` are refused when the rule is written and again when it is evaluated. Config rules stay read-only. The local database migrates forward to schema version 111; restart the daemon after upgrading. (THE-2181)

### Changed

- Building from source needs Rust 1.96 or newer; the declared minimum now matches the dependencies. (THE-1989)
- Fewer allocations when painting the system monitor and when typing in the workspace picker. (THE-898, THE-367)

### Fixed

- A failed `git ls-files` shows "file list unavailable" instead of a tree of only the changed files. (THE-375)
- The system-monitor help lists the right filter fields as ignored. (THE-2025)
- With a `/` filter in the git Files view at Half width, the diff preview and Enter follow the highlighted row. (THE-2063)
- A landed worktree whose checkout was already removed no longer leaves its merge-queue row behind; this applies to local worktrees only, and remote (ssh/provider) rows keep the existing refusal. (THE-236)

### Security

- Paths containing control characters are refused for drag insertion. (THE-135)
- The Lead can no longer run arbitrary commands through the daemon. A Lead token may open only a configured agent in a registered worktree whose launch is sandboxed, and may not type into, split or fork sessions, run host git, or change a sandbox profile; Lead-authored rules that open a session are held to the same rule. Refusals are audited as `lead_confined`. (THE-2187)

## [0.1.0-alpha.93] — 2026-11-17

### Added

- The file preview shows the text of .docx documents (bounded, sanitized; tab-stop definitions are not rendered as tabs); other zip-based files still list their parts. (THE-135)
- The system monitor's process filter accepts `pane:<id>`. (THE-2025)
- The panel header shows the worktree's zone. (THE-139)
- `y` in the git panel's Commits view copies the selected commit sha(s) to the system clipboard. (THE-787)
- Sidebar workspace headers show their zone (`zone ?` when unreadable, `!` without a `[zone.<name>]` policy). (THE-139)
- Palette action "Assign workspace to zone…"; unassigning or moving zones confirms with cancel as the default, and the zone is resolved inside the write transaction. (THE-139)
- The fleet popup lists providers over their hourly budget, and worktree paths keep their tail. (THE-2020)
- `/` search with `n`/`N` in the in-app diff viewer. (THE-787)
- `thegn agent result --row|--session [--json]`. (THE-2005)

### Changed

- neo_frizbee moved to 0.11 (one copy in the build); chrono-tz stays at 0.9 under a reasoned pin. (THE-680)

### Fixed

- The panel header no longer shows stale ahead/behind counts as current; a stale count is dimmed and an unknown one shows `?`. (THE-375)

### Security

- Hibernation capture writes its artifacts into a private, randomly named staging directory and removes it afterwards, instead of fixed `/tmp/tg-hib.*` paths. (THE-446)
- `thegn secret ssh rotate` removes rotation key pairs (base36 sibling ids) orphaned by an interrupted rotation, on the next rotation of that key. (THE-1993)

## [0.1.0-alpha.92] — 2026-11-17

### Added

- The panel's current width shows as a clickable chip at the right of the tab row; clicking cycles the width like `e`.
- Sidebar two-line rows continue the tree guide on the detail line.
- The cpu/mem popup rows show a short CPU trend sparkline.
- `thegn agent run --env <name>` pins the worktree to that env first; without it, a worktree with no env selection and no provider location is pinned to `pipeline` when `[env.pipeline]` is defined. `--json` includes `env`.
- Theme catalog row.
- The file preview lists the entries of zip, jar, whl, tar and tar.gz archives (names sanitized, bounded scan) instead of a hex dump. (THE-135)

### Changed

- The sidebar mode chip shrinks to the word `full`, then disappears (no `▌`).
- YAML parsing moved from serde_yaml to serde-saphyr: merge keys (`<<:`) are now applied, duplicate keys still error, and overlay syntax errors are multi-line with a source excerpt.
- `thegn secret` store now needs the secrets dir to be creatable even when the keyring would accept the value. (THE-522)
- Plain-session tracking reads only the rows it needs instead of the whole dispatch roster. (THE-259)
- `thegn dispatch note` is limited to 120 notes per row per minute; the error says when to retry, and `--json` reports `"kind":"rate"`. (THE-243)
- termwiz vendoring decision recorded in docs/ARCHITECTURE.md. (THE-681)

### Fixed

- A one-shot plugin that detaches a setsid helper and exits successfully no longer leaves the helper running. (THE-929)

### Security

- rusqlite/SQLite bumped to address a SQLite CVE.
- Concurrent `thegn secret` writes from two processes no longer interleave keyring and file-fallback steps; a keyring write that timed out holds the lock until it really finishes. (THE-522)
- Hook-log pruning works through held directory handles and can no longer be redirected by a swapped directory. (THE-518)

## [0.1.0-alpha.91] — 2026-11-16

### Added

- The perf rollup's `git_spawn_top` names the top (thread, git subcommand) pairs behind idle git spawns. (THE-1995)
- `thegn session list` shows `output:<age>`, the time since the session last produced output. (THE-2010)

### Changed

- Agent-login sync no longer copies `mcp-needs-auth-cache.json` (THE-2051); it uploads the allowlisted files as one archive and skips the upload when nothing changed. (THE-2056)
- The VPS/fly machine ledger and managed known_hosts live in `$XDG_STATE_HOME/thegn/vps`, moved once from `~/.thegn/vps`. (THE-2050)
- A failed pane connect opens the database once. (THE-2001)
- `thegn env test`/`deprovision` name the token reference and the paths tried when a provider token cannot be resolved; `thegn env show` warns about risky sandbox mounts (runtime dir, profile trees). (THE-2016, THE-2009)
- Daemon launches pick up `[env.*]` and env-selection edits without a daemon restart; `[host.*]`, `[sandbox.profiles.*]` and other `[sandbox]`/`[daemon]` keys still need a restart. (THE-2024)
- The VPS controller id and machine0 attestations live in `$XDG_STATE_HOME/thegn`, moved once from `~/.thegn`; a symlinked or foreign legacy id is refused, not replaced. (THE-2057)
- Process-group cleanup reaps the leader and probes the group instead of scanning all of /proc. (THE-1996)

### Fixed

- Sprites envs with `exec = "auto"` or `"api"` ignore `exec_command`/`interactive_command` (used only with `exec = "cli"`); a worktree with a stale saved transport prefix is repaired on its next launch; `thegn doctor` warns when those keys are set but ignored. (THE-2054)
- Fly provisioning installs Nix when running as root without sudo. (THE-2053)
- Plain ssh hosts with `data = "in_env"` get the repo seeded at `env up` / session open into `$HOME/thegn-worktrees/<repo>/<worktree>`. (THE-2049)
- `wt rm` no longer tries to checkpoint a sandbox it just destroyed. (THE-2055)
- Nightly CI doc link, a flaky ipc test, and RustSec advisories: dependency bumps in Cargo.lock plus documented deny.toml ignores for advisories with no safe upgrade. (THE-1989)

## [0.1.0-alpha.90] — 2026-11-16

### Added

- The fleet popup gains row actions to stop or destroy a VM (with confirmation), and the cpu/mem popup can jump to a row's worktree or pane. (THE-2020, THE-2025)
- `[placement.fleet_budget.<provider>]` sets `warn_hourly`/`max_hourly` caps; `thegn fleet cost` lists breaches, and new provider sandboxes are refused at the cap. (THE-2020)
- `thegn agent run` opens a headless agent in a worktree, optionally waits for it, and prints its output tail. (THE-2005)

### Changed

- Route-to-host merge mode no longer blocks provisioning without `thegn serve`; the in-env `merge add` refuses with the fix instead. (THE-2037)
- Agent-login sync uploads only an allowlist of config files, not telemetry or caches. (THE-2039)
- The monitor containers tab shows minicore rows and a cpu/memory scatter. (THE-842)

### Fixed

- Security: a provider env with no remote transport (e.g. fly before this fix) no longer runs sessions on the host; it halts before provisioning. Fly sessions now run on the Fly machine over thegn's ssh bridge. (THE-2044)
- Headless sessions on sprites no longer need the `sprite` CLI with `exec = "auto"`; `interactive_command` is used only with `exec = "cli"`. They start in the provisioned checkout, reattach after a dropped connection, and `session list` shows `cause:remote-exec-lost` when the sprite suspended. (THE-2041, THE-2042, THE-2047)
- The provider "provisioned" marker and cached sandbox home live in the state dir, so CLI and daemon agree. (THE-2040)
- `.thegn-provisioned` is excluded from git in remote checkouts (THE-2043); provisioning fails clearly when Nix is required but did not install, and the fly image installs xz (THE-2045); interrupted Nix builds no longer leave the store broken. (THE-2046)
- `thegn fleet` lists fly and machine0 inventories and names the real cause when it cannot. (THE-2038)
- `wt rm --delete-branch` works for remote worktrees and never blocks sandbox teardown. (THE-2048)
- Daemon agent launches honour `{prompt_file}`, and agent payloads are capped at 1 MiB. (THE-1986)
- The loading splash no longer reopens the database on every step. (THE-2001)
- Quitting cancels a hung sshfs/rsync projection mount. (THE-251)
- Disk reclaim prunes orphaned build artifacts and stale live-build dirs. (THE-1997)

## [0.1.0-alpha.89] — 2026-11-16

### Added

- Theme Builder gains an element view and a live palette picker. (THE-2023)
- Clicking the masthead `cpu`/`mem` widget shows CPU and memory by project, worktree and pane. (THE-2025)
- The MCP server gains `issues_update` and `issues_comment`. (THE-203)
- A masthead `fleet` widget shows the estimated hourly burn of provider VMs; clicking it opens a fleet popup with per-VM cost estimates. (THE-2020)
- The CPU breakdown attributes merge-queue gate and agent handoff work to the worktree that owns it. (THE-2036)
- The Theme Builder preview highlights the samples a selected color paints, and catalog rows carry palette swatches. (THE-2023)
- `[sandbox.git] credential = "deploy-key"` with `deploy_key = "<path>"` mounts that key read-only into local sandboxes; the default stays no credential. (THE-2021)

### Changed

- Config reloads apply refresh cadences, notification routing, panel/help/media and automation changes through named hooks and say so; cadence keys no longer report "restart required". (THE-2024)
- `thegn env down` pulls commits back first and refuses on a failed pull unless `--force`; `--stop` parks the sandbox instead of destroying it. (THE-2019, THE-2021)
- Provider sessions pull their commits back automatically when the last session exits. (THE-2021)
- `thegn env set` refuses a dev-only env against a stable daemon, and `thegn env show` reports the sandbox, location and git policy. (THE-2007, THE-2009)
- The voice worker runs under thegn-proc supervision. (THE-729)
- `thegn session list` prints `exited(<code>)` and `last:<state>` unless the final state was `done`; scripts that parse it should expect the extra fields. (THE-2012)
- `thegn doctor` lists provider reaper holds, and notification priority/mode warnings and Home Manager options were added. (THE-530, THE-428)
- `thegn fleet` lists typed size, region and state for every provider and finds Fly apps missing from the ledger. (THE-2020)
- `watch.pr_interval_secs` now sets the PR refresh cadence and applies live. (THE-2035)
- The monitor's disk and gpu rows use the minicore layout and stay inside the devices panel. (THE-842)
- Local podman/docker sandbox policy changed, so existing local OCI sandbox containers are recreated once after upgrading.

### Fixed

- Pane output no longer re-parses a 64 KiB tail per chunk for preview port hints. (THE-2000)
- A sandbox bind whose destination is a symlink (such as a HOME-swapped `~/.claude/projects`) no longer stops bwrap; sandboxed commits carry the host's git identity.
- Local podman/docker sandboxes mask the user bus and runtime sockets like bwrap does. (THE-2009)
- Remote OCI sync has time budgets and kills the whole process group when the copier overruns. (THE-1994)

## [0.1.0-alpha.88] — 2026-11-16

### Added

- Provider worktrees are seeded from a git bundle over the provider files API, with no GitHub credential in the VM; `thegn env pull` brings VM commits back into the lane branch. Pull-back refuses main, the default branch and the merge target, and fails closed when the default branch is unknown. (THE-2021)
- `thegn fleet list` and `thegn fleet cost` show the provider inventory, orphans, hourly burn, accrued spend and projections. (THE-2020, THE-2015)
- Config reloads report what applied live and what needs a restart, and every config key carries a reload class. (THE-2024)
- A sidebar mode indicator: a header chip, a rail glyph, and a masthead chip while the sidebar is hidden. Clicking cycles full, rail and hidden.
- Externally created worktrees go in a collapsed per-repo "Other" group. Long groups show 15 rows plus "show N more" (`sidebar_group_row_limit`, per-group `sidebar_group_row_limits`).
- The MCP server gains `sessions_snapshot`, `sessions_split`, `dispatches_put` and `dispatches_set_status`. (THE-203)
- Plugins can set transient row-field values with `ttl_secs` and `until_scan` (plugin API 0.6). (THE-2003)
- Home Manager gains `stats.alerts`, `usage.alerts`, sound pack and chime paths, and `media.mpv.socket` / `media.mpd.socket`. (THE-428)
- `thegn doctor` and the daemon report the daemon channel, and a stable daemon refuses dev-only envs. (THE-2007, THE-2017)
- Landed-cleanup holds and changed reaper kept sets record a notification in the inbox. (THE-236, THE-530)
- Config load warns on unknown notification enums and inverted alert thresholds. (THE-428)

### Changed

- Masthead app chips are clickable buttons, and a failed app shows why. (THE-2022)
- The sidebar detail line shows more by default.
- Task runs, voice capture and process helpers now supervise through the shared process supervisor, which detaches at leader exit and can hand the caller a live stdin pipe. (THE-729)
- `ci.logs` uses one 90 second deadline, provider calls clip to the refresh deadline, the remote refresh prelude is cancellable, the run-detail drill has a 120 second wall and is cancelled on quit, and `ci.runs` listings are capped; dropped requests cancel their provider children. (THE-538)
- Internal: typed git reads, strict git decoding on ssh/exec, a typed Fly machine inventory, and a refreshed host event ingress audit. (THE-368, THE-376, THE-383, THE-530, THE-661)

### Fixed

- Opening a provider worktree no longer shells out to the provider CLI, and provider worktrees are provisioned headlessly with git installed first. (THE-2014, THE-2005)
- `thegn env down` destroys the provider sandbox and verifies it is gone before saying "down". `env set` away from a provider env resets the stored location, and `wt rm --delete-branch` works for provider worktrees with a local checkout. (THE-2019)
- `ui.sidebar_detail_fields` applies on reload, and a weather provider, location or units change reloads live. (THE-2024, THE-2006)
- Halt text names the stable-channel clamp and gives provider-specific remedies. (THE-2008, THE-2018)
- `file:~/` secret refs use the account home, and `thegn wt ...` acts on the cwd's repo rather than an inherited `$THEGN_WORKTREE`. (THE-2016)
- Sandboxes with backend `none` skip startup runtime probes, so they no longer leak paused podman containers. (THE-870)
- Linear and GitHub issue reads follow label pages. (THE-323)
- `integrate --from` fails closed when the branch read fails, and lifecycle refuses on a failed branch read. (THE-368)

### Security

- `[sandbox.git]` policy: bwrap sandboxes default to commit-only. The shared `.git` is read-only except the lane's refs, objects and worktree metadata; a top-level branch, the main checkout or an unknown merge target fall back to a read-only `.git` unless `mode = "full"`. (THE-2021, THE-2009)
- `GH_TOKEN`/`GITHUB_TOKEN` (and GitLab/GitHub Enterprise tokens) are withheld from sandboxes and provider VMs by default; only `mode = "full"` with `credential = "repo-token"` or `"user-token"` passes them. (THE-2021)
- Sandboxes mask runtime-directory escapes and the daemon's control socket. (THE-2009, THE-2011)

## [0.1.0-alpha.87] — 2026-11-15

### Added

- `thegn share list` now shows Tailscale serve ports whose teardown is still pending, and `--json` rows carry `teardown_pending`. (THE-330)
- Plugins that provide a worktree row field can subscribe to `thegn.worktree_scanned`, which is sent when a worktree's git glyphs change. (THE-2003)
- Home Manager gains `sound.min_priority`, `dnd.allow_priority`, `sound.per_kind`, `dnd.windows`, `usage.providers` and `usage.profile_roots`. (THE-428)

### Changed

- The daemon `ci.logs` now runs at most 2 provider log fetches at once; a cache-missing request waits up to 30 seconds, then gets a busy error. (THE-544)
- Internal: workspace clone, setup runs and VPN/preview capture now run under the shared process supervisor, the Fly reaper re-reads its ledger before destroying, and CI log collector bounds are tested. (THE-729, THE-529)

### Fixed

- Invalid `dnd.windows` entries and unknown `usage.providers` now warn at config load. (THE-428)
- Worktree paths with trailing spaces or non-UTF-8 bytes are now read exactly by several commands, including the merge fixer checkout. (THE-368, THE-376)

## [0.1.0-alpha.86] — 2026-11-15

### Added

- The MCP server gains `sessions_resize` and `worktrees_open`, both needing the `write` scope. (THE-203)
- `media.backend`, `notifications.sound.mode`, and `notifications.sound.always_kinds` are now Home Manager options. (THE-428)
- `thegn plugin status` gains `skipped` and `deferred` columns for scheduler ticks that were skipped or pushed back. (THE-537)
- On hybrid Linux CPUs, the CPU Frequency summary shows the mean frequency of the performance and efficiency cores. (THE-844)

### Changed

- The CPU usage panel's `io` and `st` rows now show the latest percent beside the sparkline. (THE-842)
- Internal: a `run_consuming_streams` entry point and an `idle_output` limit on `thegn_proc`; strict and byte-exact git output helpers; control-byte tests for the git list parsers; per-drain budgets for worktree-creation, share, forward, and host UI events; and tests for each. (THE-729, THE-368, THE-383, THE-657)

### Fixed

- PTY open, attach, and resize now refuse sizes over 1000x1000 on every surface, instead of allocating a huge terminal grid.
- When `thegn share` cannot verify a Tailscale teardown, it records the port as pending and tells you to retry with `thegn share stop <local port>`. (THE-330)
- Worktree listings now fail with an error on a non-UTF-8 path instead of showing a replacement-character path. (THE-368, THE-376)
- The VPS reaper re-reads its ledger before destroying an instance and keeps any record that changed in the meantime. (THE-529)

## [0.1.0-alpha.85] — 2026-11-15

### Added

- An opt-in `issue` sidebar detail field shows the newest issue linked to the worktree, with `+N` when more are linked. (THE-2003)
- The MCP server gains `merge_add`, which enqueues a worktree's branch on its repository's merge queue and needs the `git` (or admin) scope, and `notify_push`, which pushes a tray notification and needs the `write` scope. (THE-203)
- Control API, gRPC, and MCP `merge.list` rows now carry `cleanup_retry` (attempts, next attempt time, class) and `cleanup_hold`, so clients no longer have to decode the retry marker. (THE-236)
- On Linux, the CPU Frequency panel shows ` itmt` when the kernel has Intel Turbo Boost Max scheduling enabled, and falls back to ACPI CPPC `highest_perf` when the AMD value is absent. (THE-844)
- `stats.refresh_rates` is now a Home Manager option. (THE-428)

### Changed

- An out-of-range `stats.refresh_rates` entry (below 0.5 seconds, above the duration cap, or not finite) now logs a warning naming the index, and the value still loads. (THE-428)
- Merge-queue and PR-queue messages are now handled in bounded slices per loop turn (64 messages or 4 ms), so a burst cannot stall input. (THE-657)
- The plugin alert outbox now has a 64 KiB budget per drain across all plugins; alerts over it are dropped and counted. (THE-537)
- Internal: Git and capability probes, review-pane preview capture, and VPN subprocesses now run on `thegn_proc` and no longer start their own reader and reaper threads; typed inventory results for Sprites and Daytona listings; a color-depth test matrix over every graph painter; and tests for each. (THE-729, THE-251, THE-530, THE-842)

### Fixed

- GitHub issue detail now reports its label list as incomplete when it reaches the 100-label page limit, instead of claiming it is complete. (THE-323)
- Submodule status reads now fail with an error on non-UTF-8 paths instead of silently substituting replacement characters. (THE-368, THE-383, THE-376)

## [0.1.0-alpha.84] — 2026-11-15

### Added

- An opt-in `stash` sidebar detail field shows the repository's stash count, read from the stash log without a git process and hidden at zero. (THE-2003)
- An opt-in `ci` sidebar detail field shows the worktree's latest cached CI result (`ci pass`, `ci fail`, `ci run`, `ci pend`) with no extra fetch. (THE-2003)
- On hybrid Linux CPUs, the CPU Frequency panel and the Power tab policy rows tag each policy `perf` or `eff`, and the Frequency summary adds `hybrid <n>P/<n>E`. (THE-844)
- The `[activity]` float keys, plus `media.volume_step`, `usage.warn_percent`, `usage.crit_percent`, and `stats.refresh_secs`, are now Home Manager options with range checks. (THE-428)

### Changed

- Git write commands now have a deadline: 1 hour for network commands (fetch, push, pull, clone and similar) and 15 minutes for other writes. Set `THEGN_GIT_WRITE_TIMEOUT_SECS` to override it, or `0` to disable it.
  Background processes a git write leaves in its process group are now stopped when it finishes. (THE-368)
- Git commands that read from stdin now share the write deadline, and Git output beyond 256 MiB on stdout or 8 MiB on stderr stops the command with an error naming the limit. (THE-368)
- Git output larger than 64 MiB, or any single field larger than 1 MiB, is refused as an error instead of being parsed. (THE-383)
- Daemon fallback notices name the newest unacknowledged crash report. (THE-119)
- An out-of-range `media.volume_step`, `usage.warn_percent`, `usage.crit_percent`, `activity.cpu_percent`, or `activity.runaway_core_fraction` now logs a warning naming the key and valid range, and the value still loads. (THE-428)
- Internal: typed VPS inventory results for the server reaper; latest-value slots for metrics, container, provisioning, and live job output; per-drain ingress budgets; a `test/podman-pause-bisect.sh` harness; a minicore graph painter; and tests for each. (THE-530, THE-661, THE-657, THE-870, THE-842)

### Fixed

- Live job output, metrics, container, and provisioning updates now keep only the newest value instead of queueing every update, and chatty container events wake the screen at most once per 250 ms. (THE-661)
- The review-pane structural diff and the workspace clone probes now run under a deadline and output cap, and an incomplete structural diff is reported as an error instead of shown partial. (THE-729)

## [0.1.0-alpha.83] — 2026-11-15

### Added

- Sidebar worktree rows now show commits ahead of the default branch (`5c`, capped at `999+`) and a detail line with the branch, commit count, changed files, diff stat, PR, last commit subject, and age. Configure badges, visibility, and field order with the new `[ui]` keys `sidebar_detail_when`, `sidebar_show_commits`, `sidebar_show_behind_base`, `sidebar_commit_badge`, `sidebar_behind_badge`, and `sidebar_detail_fields`.
- Sidebar detail fields can now be ordered or removed with `sidebar_detail_fields`; the former fixed tail (`env`, `sandbox`, `devcontainer`, `hibernated`, `mq`, `unread`, `disk`) remains the default, while upstream, stage, activity, and last-commit author are opt-in.
- Plugins can add sidebar detail fields and row badges through the `WorktreeField` extension point. API 0.5.0 plugins list them as `plugin:<plugin>/<field>`; API 0.3 and 0.4 manifests still work, plugin text is sanitized and width-capped, and plugin fields are dropped first when a row is narrow. See `docs/extending/row-field.md`.

### Changed

- Sidebar details now show on every row by default, including when the sidebar is unfocused. Set `sidebar_focus_detail = "cursor"` and `sidebar_detail_when = "focused"` to restore the previous behavior.
- `thegn doctor` now labels CI credential rows `configured (not probed)` when it has not checked the connection.
- Internal tests now cover writer backpressure, hidden log panel feedback, tracker paging limits, Git parser truncation, exotic file names, repository discovery budgets, cleanup failure injection, process resource growth, and the Podman pause-process leak investigation; `docs/host-event-ingress.md` records the host event-ingress contract table.

### Security

- Errors from `gh` and `glab`, including JSON decoding errors, no longer expose configured tokens or token-shaped strings.

### Fixed

- Plugin runs that exit cleanly are no longer reported as killed; on Linux, a run is marked killed only when a leftover process in its group had to be stopped.
- Core Git helpers now have a 60-second deadline and bounded output, so a hung or runaway Git command cannot stall the caller.
- The gix Git backend now reports both sides of a staged rename instead of dropping the deleted side.
- If one pane of a tab fails to start, panes already started for it are closed; reattached sessions are detached and keep running.
- Git parsers now reject truncated `merge-tree` conflict output and signed timestamps instead of returning partial results.
- Worktree creation, rename, the creation wizard, and agent dispatch now refuse when the branch list cannot be read, avoiding duplicate names.
- The Git runner now applies its deadline and output cap to commands fed on stdin.
- The screen now updates promptly after container events instead of waiting for another wake-up.
- Calendar diagnostics now share one budget across all pages of a fetch, count truncated output, and report junk output lines from calendar plugins.

## [0.1.0-alpha.82] — 2026-11-14

### Added

- The `THEGN_PERF=1` rollup now reports the three busiest database-open call sites.

### Changed

- Plugin schedule intervals from 100 through 999 ms are raised to 1 second when the plugin loads, with a warning naming the plugin; intervals under 100 ms are rejected.
- One-shot plugin commands now use 5, 20, then 50 ms polling backoff, so quick commands finish sooner and long-running commands wake less often.

### Fixed

- Git reads in worktree, repository, supervision, merge, wizard, integration, hydration, CI, and PR flows now use bounded, typed calls and fail closed on errors; the sandbox's shared Git-directory mount also fails closed.

## [0.1.0-alpha.81] — 2026-11-14

### Changed

- Cargo generation pruning now retains the two newest fingerprints per package
  and unit kind, and reclaims older generations after the shorter of the
  configured age and a three-day safety floor, including in active and dirty
  worktrees.
- Plugin schedules now run at most once per second, and the shipped `hello` example plugin now runs every 60 seconds instead of every 5.

### Fixed

- Idle sessions cache primary-branch checks until branch refs change, so they no longer spawn Git repeatedly; missing worktrees no longer warn on every check.
- Finished subprocess group liveness checks reuse one global `/proc` scan per short window; plugin descendant checks walk the child's own subtree.
- Background memory trimming backs off after slow scans and skips scan trimming when swapped memory is elevated; noisy `tokei`, `ignore`, and `globset` logs are limited to errors.
- Plugin CI log streams now redact retained private-key lines when an END marker arrives without a BEGIN marker.

## [0.1.0-alpha.80] — 2026-11-14

### Changed

- Daemon status updates now keep only the latest snapshot, so rapid updates stay bounded instead of queueing every update.

### Fixed

- Sprites checkpoint lists now follow continuation pages instead of refusing multi-page inventories; partial inventories are still refused and transient list errors are retried.
- Importing a merge bundle no longer drops output still in flight when the producer exits ("bundle produced no data").
- Land and diff-watch Git reads now preserve typed failure reasons and fail closed instead of treating unreadable output as empty.

### Security

- CI log tails that begin inside a private key now redact retained preceding lines when the END marker arrives without a BEGIN.

## [0.1.0-alpha.79] — 2026-11-14

### Added

- For GitHub or GitLab CI, `thegn doctor` adds a value-free `CI credential placement` section with `credential placement: github/local`, `github/ssh`, and `github/provider` (plus matching `gitlab/...` rows); JSON keeps the notes under `providers`.
- The workspace picker adds a `broad root: … (scan limited to 20000 repos)` warning row for roots such as `/` or your home directory without refusing them.

### Changed

- On Linux, PDF previews require usable `bwrap` and Mermaid previews are refused as uncontained, with Mermaid falling back to source text; other platforms mark uncontained previews.

### Fixed

- Duplicate CI detail requests no longer run twice, and provider JSON is capped by total bytes, object and array size, depth, and item count.
- Native provider exec output is chunked and byte-bounded, so a flood cannot grow retained memory without limit.

## [0.1.0-alpha.78] — 2026-11-14

### Added

- `thegn doctor` prints host-cache `scope` and `require_sigs` lines beside the signer;
  JSON adds `scope`, `require_sigs`, and `require_sigs_explicit`.

### Changed

- Container, model-proxy and `worktrees.create` (MCP) capabilities are advertised only on the surfaces that actually serve them.
- Plugin manifests use a canonical bounded grammar for plugin, contribution,
  surface and capability IDs, and invalid IDs are refused with bounded diagnostics.

### Fixed

- Provider executable installs take a per-destination lease, preventing concurrent
  overwrites; failed verification restores the previous verified executable.
- Truncated UI labels keep whole characters and terminal control characters render inert.
- Git failures in integrate, autopilot, session resume and worktree commands identify
  the failed Git operation instead of being treated as missing data.
- Font configuration refuses updates through a user-owned symlinked parent directory.

## [0.1.0-alpha.77] — 2026-11-14

### Changed

- LSP slots now report typed states, making ready, starting, degraded, and backoff states visible.

### Fixed

- Worktrees created outside thegn are discovered without rerunning Git when their metadata is unchanged.
- A failed worktree close now leaves a status message.
- LSP locations preserve exact OS paths, including Windows drive and UNC paths, and mark locations outside the worktree.
- Human-readable CI logs render terminal control characters as visible escapes instead of passing them to the terminal.
- CI provider responses are size-bounded, and only exact owner/repository remotes are recognized as GitHub.
- Clipboard status wording appears only for clipboard operations and no longer overwrites a specific copy message.

## [0.1.0-alpha.76] — 2026-11-14

### Fixed

- Scheduled CI failures without a queued PR now produce one `ci_run_failed` notice per run, and a newer run replaces the previous unread notice.
- Restoring a hibernated snapshot stages artifacts in a private directory and verifies the replay before using it.
- Destroy and reconciliation calls to cloud providers reserve control-gate capacity, so ordinary traffic cannot starve them.
- Forwarded share and VPN diagnostics are capped by byte and rate budgets, and sidecar records include optional placement fields.
- `dispatch put` and `dispatch claim` refuse a parent row from a later pipeline stage.
- Merge-bundle ingest checks free disk space before streaming, falling back to the size cap when free space is unknown.

## [0.1.0-alpha.75] — 2026-11-13

### Added

- gRPC exposes `AgentSessions`, `SessionsRecord` and `WorktreesCreate`; launch
  presets remain CLI-only because their configuration and environment resolve locally.

### Fixed

- A reused gate worktree verifies that tracked files still match the branch snapshot before gating.
- Pipeline supervision no longer treats a worktree in an unknown state as satisfying a parent-artifact requirement.
- Release, worktree-prune, dispatch-commit and PR-authorship Git reads report errors instead of treating unreadable output as empty.

## [0.1.0-alpha.74] — 2026-11-13

### Added

- gRPC exposes `issues.update`, `issues.comment`, `dispatches.list`, `dispatches.put` and `dispatches.set_status`; doctor bundles are CLI-only and
  workspace search is not advertised as a control surface. Secret routes are marked unsafe for remote transports.
- S3 snapshot uploads use conditional creation (`If-None-Match: *`) by default, so existing snapshots are never overwritten; set
  `[lifecycle.snapshot] conditional_create = false` for stores that reject conditional writes.

### Changed

- Share endpoint validation covers URL and ticket boundaries, and bore's environment exposure is documented.

### Fixed

- `thegn diff` and other default-branch reads report an error when the default branch cannot be read instead of silently
  using `HEAD`.
- A failed account claim removes the empty account home it created.
- Release uploads refuse `--clobber` once a release is published, and archives reject vendor PAX headers.
- Theme names are compared after Unicode NFC normalization.

## [0.1.0-alpha.73] — 2026-11-13

### Added

- MCP read tools `calendar_events`, `calendar_clocks`, `issues_list`, `issues_get` and `dispatches_list` expose calendar, tracker issue and dispatch-roster data.

### Changed

- Pipeline rows waiting for a human or an open PR remain protected from redispatch without holding a stage concurrency slot.
- A worktree edited after landing keeps its queue row labelled; `thegn merge sweep --force` retires only the queue row, and `thegn merge list --json` shows `cleanup_class` ids.
- Merge-queue help explains that rebase holds histories containing merge commits or unrelated roots.

### Fixed

- Merge-queue gate commands without shell syntax run directly, so a missing executable is reported as a gate failure instead of an infrastructure error.
- Dispatches can no longer name a superseded parent, except when resuming that parent.
- Diff line counts use NUL-delimited output, so paths containing tabs or newlines are counted correctly.

## [0.1.0-alpha.72] — 2026-11-13

### Added

- `thegn sandbox ls`, `thegn sandbox ctl` and `thegn sandbox logs` list and control sandbox containers from the CLI.
- MCP tools `git_status`, `merge_list` and `pr_status` expose Git status, the merge queue and cached pull-request status; gRPC exposes `mcp_proxy.status`, `mcp_proxy.reload`, `issues.list` and `issues.get`.

### Fixed

- Remote worktrees now read file and worktree lists with NUL delimiters, so paths with newlines and other unusual names are handled safely; Changes and stash views show “unavailable” when a Git read fails instead of an empty list.
- Rebase state is reported as running, not running or unknown instead of guessing when Git state cannot be read.
- Failed patch application now reports rollback errors, and worktree snapshots restore the index after a failure.
- Playerctl and SMTC media metadata is clipped to safe field lengths.

## [0.1.0-alpha.71] — 2026-11-13

### Added

- The Home Manager module exposes `[sandbox.limits]` `heavy_slots` and `max_load`, plus the enum options `network.mode` and `media.default_action`.
- `thegn integrate` records integrations in the session roster; an ordered `--order` fold with `--into` that stalls or fails its gate prints a cross-lane fix-up command.
- The example configuration includes an `integration-fixup` stage recipe.
- In the Forward panel, `a` adds a port forward for a container port entered by hand.

### Changed

- `just test` runs the whole workspace as one nextest run.

### Fixed

- A pane no longer fails to start when the systemd user session bus is unreachable, such as with a stale `DBUS_SESSION_BUS_ADDRESS`; the hard CPU cap is skipped and a soft priority cap is used when available.

## [0.1.0-alpha.70] — 2026-11-12

### Added

- `thegn session open --permissions` grants per-launch permissions to a Claude session; they can only narrow the agent entry's grants unless
  `--widen-permissions` is given.
- `thegn land --trust-gate` skips the gate only when a trusted attestation matches the branch commit and tree and the current target tip is its ancestor;
  otherwise it runs the gate. Lands from a non-root worktree are recorded in the session roster.
- `thegn integrate --order a,b,c --into <branch> [--from <slug>]` folds named branches in that order onto a stack branch and prints a resume command on conflict;
  branches folded into a stack branch are not cleaned up as landed.
- `[sandbox.limits] heavy_slots` and `max_load` (off by default) limit how many heavyweight gates run at once across processes and wait for the load average
  to drop.
- The Home Manager module exposes the `[git]` options `backend`,
  `structural_diff` and `submodules` as enums.
- The sandbox end-to-end test verifies that a stage worker can commit in its
  worktree but cannot write outside it.

### Fixed

- GitHub CLI calls have deadlines (30 seconds for reads, longer for PR diffs and up to 10 minutes for merges and PR creation), so a hung `gh` no longer
  stalls thegn.
- In the Git panel, `P` pushes the selected branch and `p` pulls the checked-out branch or fast-forwards another selected branch; neither shortcut
  forces a push or changes upstream configuration.

## [0.1.0-alpha.69] — 2026-11-12

### Added

- The Home Manager module exposes the `[stats]` thresholds `battery_warn`,
  `disk_free_warn` and `disk_free_critical`.

### Fixed

- `thegn doctor` no longer hangs on a blocked GitHub CLI (such as a locked
  keyring); bounded identity and git probes report an unknown result.

## [0.1.0-alpha.68] — 2026-11-12

### Added

- The Home Manager module exposes the `[semantic]` settings `worktree_index`,
  `index_max_files` and `map_budget_lines`, plus `log.stderr_cap_mb`.

### Security

- Managed SSH key rotation and host provisioning take their lease locks through a pinned private directory handle.
- Plugin run directories for control publishing, plugin stats and the plugin SDK's local runner are created and opened through one pinned private directory handle.
- Devcontainer volume ownership claims are written through a pinned private directory handle.

## [0.1.0-alpha.67] — 2026-11-12

### Added

- The Home Manager module exposes the `[lsp]` settings `enabled`, `hover` and
  `require_containment`; server entries remain config-file-only.

### Security

- Hook log files are finalized through a pinned directory handle instead of
  reopening the directory by path.
- The secrets store and managed-tool staging directories are created and
  opened through one pinned private directory handle.
- The managed SSH ledger lock and machine0 attestation store use pinned private
  directory handles.

### Fixed

- Truncated or malformed `git status` replies now error instead of reading as a
  clean tree; commit-time output must end with a newline.

## [0.1.0-alpha.66] — 2026-11-12

### Added

- The Home Manager module exposes `[preview]` `enabled`, `fetch_timeout_ms` and
  `max_body_bytes`; ports and external URLs remain config-file-only.
- `docs/help/sandboxing.md` documents preserving worker reports with a writable
  `mounts` entry under `[env.*]`; the private-`/tmp` warning skips covered paths.
- The Home Manager module exposes `[usage]` scalars and tailnet host-discovery
  `enabled`/`online_only` toggles; network opt-in stays config-file only.

### Security

- The local `act` guard scans user-level actrc files (XDG/`~/.actrc` and macOS
  Application Support) for credentials using act's own line tokenizer.
- The local `act` guard handles clustered short flags whose last letter takes a
  value (e.g. `-vs`), so the following token is read as that value.
- Drawer state and PTY diagnostics files are written through a pinned private
  directory handle, with no path re-resolution between create and write.
- Hook log files are allocated through a pinned directory handle with
  exclusive, no-follow creation.

### Fixed

- Git log-graph and numstat parsing reject output truncated mid-row instead of
  returning a partial result.

## [0.1.0-alpha.65] — 2026-11-12

### Added

- `session snapshot`, `session attach` and `session wait` accept the session id
  positionally as well as through `--session`.
- `session open --track <issue>` records an ad-hoc worker in the dispatch roster,
  where `dispatch list`, `dispatch wait` and `dispatch verify` can see it.
- `session open --brief-file F --vars k=v` supports file-backed prompt templates
  for lanes without an issue.
- `session open` warns on stderr when its prompt names a `/tmp/` path hidden by
  a private sandbox `/tmp`.
- `wt new --batch FILE` creates multiple worktree lanes from one batch file.

## [0.1.0-alpha.64] — 2026-11-11

## [0.1.0-alpha.63] — 2026-11-11

### Added

- `thegn wt prune --merged [--target <branch>] [--apply] [--json]` removes lane
  worktrees and branches already merged into the target; it is a dry run
  unless `--apply` is given.
- `thegn dispatch commit <row> --msg-file F [--trailer "Key: value"]` commits a
  finished worker's worktree in one step, retrying once after a formatter hook
  rewrites files.
- `thegn release prepare --bump <alpha|beta|rc|patch>` bumps the version in
  `Cargo.toml`, `Cargo.lock`, `README.md` and `CHANGELOG.md` without
  committing, tagging or pushing.
- `thegn wt new <name> --exact-branch` uses the name verbatim as the branch,
  skipping the configured prefix and numbering.

### Fixed

- `thegn session open --stage --issue` now writes
  `.thegn/maintenance/issue.json` into the worktree.
- Live and parked workspaces in the sidebar now come from one registry row
  source, so their rows no longer disagree.

## [0.1.0-alpha.62] — 2026-11-11

### Added

- The Home Manager module exposes the `[daemon]` and `[network]` scalar
  options (`daemonEnabled`, `daemonIdleExitSecs`, `daemonLeaseGraceSecs`,
  `daemonStreamGapWindowSecs`, `networkOfflineAfterFailures` and
  `networkRecoveryProbeSecs`). `daemon.socket` and `network.mode` stay
  unexposed.

### Fixed

- Drawer state files are now written owner-only: the parent directory is
  private (0700) and the file is 0600, a symlinked final entry is replaced and
  a symlinked parent is refused.
- The hook log directory and the reserved per-run log files are now
  owner-only, so a failed finalization no longer leaves a world-readable
  reservation.
- Credentialed `just act` runs refuse a trailing value flag with no value, and
  the guard scans arguments the way `act` parses them across `.actrc`, so a
  credentialed run can no longer pass as anonymous.
- Adding an account claims its managed home with one exclusive directory
  create, so concurrent Add-account calls can no longer share one credential
  directory.
- Workflow triggers refuse more than 64 inputs before any provider request.
- The daemon's idle control-directory prune also reclaims a dead daemon's stale
  socket and lock after confirming the socket refuses connections.
- Branch, tag and bisect ref output must end with a newline, so output
  truncated inside the final record is no longer parsed as complete.
- Pipeline supervision no longer treats an unstable verification snapshot as a
  parent artifact.

## [0.1.0-alpha.61] — 2026-11-11

### Added

- The Home Manager module exposes the `[disk]` scalar options
  (`diskShowSizes`, `diskWarnThresholdGb`, `diskScanIntervalSecs`,
  `diskMaxScanPerRound`, `diskAutoCleanOnMerge`, `diskCleanOnPrClosed`,
  `diskIdleCleanDays`, `diskGenerationMinAgeDays`, `diskReclaimOnLowDisk` and
  `diskSccache`). The two path keys stay unexposed.

### Fixed

- The daemon retries its control-socket bind once when an idle-prune pass
  removes the control directory between the directory check and the bind lock,
  instead of failing to start.
- Pipeline verify refuses a mixed snapshot: if the worktree's HEAD or the
  handoff artifact changes while verification is running, the result is
  reported as unknown rather than as a verdict about two different states.
- Filing a pipeline report is tied to the run it was verified against, so a
  run that was republished in the meantime no longer receives the report.
- The pipeline parent-edge check compares issues by their canonical identity:
  `ABC-74`, `linear:abc-74` and `LINEAR:ABC-74` are one issue, while the same
  key under two different providers stays distinct.
- Removing a workspace now also forgets the drawer panes and remembered drawer
  state of every worktree it closed.
- `make-app.sh` refuses a `--name` containing `..` or ending in `.`, and
  derives a valid macOS bundle identifier from a name containing underscores.
- A media-player payload that reports no playback state is shown as stopped
  rather than playing.
- Git commit and stash listings that arrive truncated (without their final
  terminator) are rejected as a whole instead of silently dropping the last
  record.

## [0.1.0-alpha.60] — 2026-11-11

### Added

- The Home Manager module exposes the `[limits]` options (`limitsTestCpuQuota`,
  `limitsTestMemMax`, `limitsTestMaxParallel`, `limitsTestTimeoutSecs`,
  `limitsDiscoverTimeoutSecs`, `limitsIsolatedTargetDir`) and the
  `[diagnostics]` options (`diagnosticsCrashReports`,
  `diagnosticsCrashRetention`, `diagnosticsRingSize`).
- Pipeline verify output now carries a typed refusal kind for each refusal
  (for example an unreadable, oversized, binary or sensitive artifact, an
  unknown worktree, or an unproven gate), so callers no longer parse prose.
- The calendar agenda header says which rows are shown when a long day is
  windowed (for example `12 · 3-9 shown`).

### Changed

- The daemon prunes idle, abandoned control directories from other state
  scopes when it binds, leaving the emptied directory in place, and re-checks
  the bind lock after taking it so a pruned lock file cannot be held twice.
- The calendar agenda keeps your selection on the same event across a
  refresh instead of jumping to the same row number.
- The log parser reads worktree identity tokens only from the leading run of the
  line, so a token later in a message is no longer taken for one.

### Fixed

- Sealed containers no longer keep host credentials or dotfiles that an
  earlier provisioning copied in. The cleanup removes a file only when it
  still matches the host's copy, resolves the home directory physically, and
  never follows symlinks or crosses mount points.
- `thegn dispatch` completion re-checks its evidence inside the status write
  and refuses a row that changed in between or whose fence row is missing.
- Artifacts reached through a symlinked or `..` parent directory, and
  absolute artifact paths, are refused.
- A drawer launch result that arrives after its worktree was forgotten is
  dropped instead of being applied.
- Dispatch sessions and review tasks imported from older data are recorded
  with their proper dispatch kind.
- The CI `act` guard refuses a runner image that is not pinned by digest, or
  an override that changes the pinned one.
- The macOS app build takes a lock so concurrent runs cannot clobber each
  other; a stale lock is reclaimed safely and the build fails closed if it
  cannot be written.

## [0.1.0-alpha.59] — 2026-11-11

### Added

- The Home Manager module exposes recording (`recordingDir`,
  `recordingMaxBytes`), media (`mediaEnabled`, `mediaShowArt`,
  `mediaOverlayOnBadgeClick`, `mediaSeekStepSecs`, `mediaSeekStepVideoSecs`,
  `mediaPollIntervalSecs`, `mediaPlayersPriority`) and monitor process table
  (`monitorProcRows`, `monitorProcesses`, `monitorProcessIo`) options.
- `thegn doctor --json` carries a `stage_worker` key with the stage worker
  readiness class that the text report already showed.
- The control API reference now has a generated routes table with request and
  response types, a transports and access-modes section, and a page of worked
  request and error examples that are checked against the route registry.

### Changed

- A pipeline stage row now records its kind (plain, stage or held). Completion
  refuses a stage row that has lost its stage or artifact path. The local
  database migrates forward to schema version 110.
- `thegn dispatch` refuses `--artifact` together with `--stage` before doing
  any work, because a staged row's artifact path is always assigned for you.
  Plain rows still accept `--artifact`.

### Fixed

- The calendar agenda scrolls to follow the selected event. Before, moving
  past the visible rows left the selection on an event you could not see, and
  Enter could open the link of that hidden event.
- The Logs section counts only lines for the current worktree, so errors that
  belong to another worktree no longer show in its header.
- Closing a worktree forgets its drawer panes and remembered drawer state, so
  a worktree later created at the same path starts clean.
- The daemon cleans up dead private control directories left under `/tmp` by
  other state locations, without touching live ones.

## [0.1.0-alpha.58] — 2026-11-10

### Added

- Releases now include an aggregate `SHA256SUMS` file covering the Linux
  tarball, the `.deb` and `.rpm` packages and the notices file, plus the
  `THIRD_PARTY_NOTICES.md` licence notices as a release asset.
- The control API can read a worktree's sandbox profile pin
  (`worktrees.sandbox_profile`) and set it to `standard` or `sealed`
  (`worktrees.sandbox_profile_set`, admin only; an explicit `clear` removes the
  pin). The change is not offered to MCP tools or plugins.
- `thegn doctor` lists worktree sandbox pins, showing each pin and the profile
  the worktree would use without it. The JSON report carries the same list as
  `worktree_sandbox_pins`.
- `just net-trace` (`scripts/first-run-net-trace.sh`) is an opt-in Linux script
  that traces a release binary's first frame in a throwaway home and reports
  any network connection other than local Unix sockets. It needs `strace`.

### Changed

- When the state database was written by a newer thegn, the refusal message now
  points to the "Upgrading and uninstalling" help page for upgrade, rollback
  and backup steps.
- Verification refuses a pipeline handoff that contains a known credential
  format, such as a private key, a token or a URL with a password, and names the
  kind without echoing the value.
- The Logs panel's worktree scope reads "this worktree + global". Entries with
  a legacy or unbound worktree tag no longer appear under a single worktree and
  are labelled in the detail view.

### Fixed

- Sealed worktrees on host-backed containers no longer receive your atuin key
  and session or your dotfiles; thegn logs one warning when it drops them.
- A one-shot plugin that times out or is cancelled now has descendants that
  detached into their own session killed as well.
- Spellings of one worktree path that differ only by a trailing slash or a dot
  segment share one drawer state. Drawer state saved under a trailing-slash
  path reopens closed once.

## [0.1.0-alpha.57] — 2026-11-10

### Added

- Each worktree can pin its own sandbox profile, `standard` or `sealed`. Pick
  it in the new-worktree wizard's Isolation row, with the palette action
  "Isolation profile for this worktree", with `thegn wt new --isolation`, or
  with `thegn sandbox profile show|set|clear [--worktree <path>]`. The default
  stays `standard`; see [Worktree isolation profiles](docs/help/sandbox-profiles.md).
- The Home Manager module adds null-default options for the notification
  do-not-disturb and sound settings (`notificationsDndEnabled`,
  `notificationsSoundMute`, `notificationsSoundSuppressFocused`).

### Changed

- A refused `sealed` launch now says how to switch the worktree back to the
  `standard` profile.
- The control API's dispatch status change to `done` applies the same
  missing-evidence check as `thegn dispatch set-status done` and refuses with
  a `missing_evidence` message. The override stays CLI-only (`--force --why`).
- Forge HTTP failures are logged with the status, a failure class, the
  repository and a short redacted message, never a token.

### Fixed

- A sandbox shell whose command already starts with `exec` no longer launches
  as `exec exec <shell>`, which exited every pane.
- Editing the font configuration re-checks the file just before replacing it
  and refuses with "<path> changed while editing; retry" if it changed.
- The worktree delete menu says the branch is kept.
- Dropping an image into a remote pane has a 60-second deadline and bounded
  captured output.

## [0.1.0-alpha.56] — 2026-11-06

### Added

- `thegn pr queue list` shows the last agent run for local checkouts; `--json`
  adds an `agent_outcome` object or null.
- `thegn plugin status [--json]` shows per-plugin activity counters while the
  UI runs with `[daemon] enabled`.
- `thegn merge list --json` and control `merges.list` rows include
  a notice when the queued branch or worktree has changed.
- The Home Manager module adds null-default options for the `[notifications]`
  booleans and `[activity]` settings.
- `THEGN_PERF` rollups report cumulative pushed, taken, dropped and gap-marker
  counts in a `log ingress` event.
- Pane-fallback launch notices point to `thegn doctor bundle`.
- A README quickstart, a minimal starter configuration and a generated list of
  third-party crate licenses are available.
- Pull requests now run lean checks for formatting, linting, tests and smoke;
  the full gate runs nightly and on demand.
- A script traces the network connections made during a first run so you can
  check what thegn contacts; see [Privacy and network use](docs/help/privacy.md).
- A [step-by-step guide](docs/extending/plugin-hello-world.md) shows how to
  write a first plugin.
- A demo script drives a repeatable live walkthrough (`test/demo/demo.sh`).
- In-app help covers privacy, upgrading and uninstalling, troubleshooting,
  known limitations, comparisons with tmux, Zellij and lazygit, and a quick
  reference.

### Changed

- Agent pipeline runs are refused before they start if the sandbox cannot
  write the worktree, Git common directory or relocated credential home, or
  if the `thegn` binary was replaced since launch.
- `thegn dispatch set-status <id> done` refuses a `spawning` row without a
  published session; `--force --why '<reason>'` records an override.
- Typed sandbox preparation failures lead with the refusal class and
  remediation in launch warnings, the new-worktree wizard and while preparing
  a worktree's pipeline. Untyped failures keep their plain text.
- Linear and Jira listings or searches that cross an aggregate response-size
  cap after reading rows return those rows as a partial account.
- After a CI autofix run, the PR-queue row detail says what the run did to the
  PR head and awaits the next PR refresh.
- The bug report template asks for `thegn doctor bundle`; the CLI contract
  documents top-level verbs; and the stable/dev channel help reflects the
  shipped feature set.

### Fixed

- Hunk and submodule previews are scoped to their worktree and path, so a
  preview that finishes after a worktree switch does not clear or render into
  the active worktree's preview.
- Abandoning a calendar command fetch kills the plugin process group.

### Security

- A registered daemon endpoint is trusted only when it is an absolute Unix
  socket path owned by the current user.

## [0.1.0-alpha.56] — 2026-11-05

## [0.1.0-alpha.55] — 2026-11-05

### Added

- `thegn merge list` shows a landed row's source tip, target base and result
  OID; `--json` adds a `landed` object. Rows landed by `thegn land` or before
  this release show only what was recorded.
- `thegn theme rename OLD NEW` moves a local theme to a new name. It refuses
  built-ins, the selected theme, a name already in use, or one that maps to
  the same file.
- `thegn plugin preview --plugin-dir <DIR> --record <FILE>` writes a
  metadata-only trace of the live session to a new owner-only file, capped at
  8 MiB. Headless and interactive previews both support it.
- The control API's `dispatches.put` and `dispatches.set_status` accept an
  optional `lead` proof (`lease`, `owner`, and optional `lease_gen`) for rows
  bound to a live Lead lease; requests without it keep their previous behavior.
- After each PR agent run, pass warnings say whether the agent changed nothing,
  committed locally without pushing, or another person pushed meanwhile.
- The status modal's plugins section adds a `bridge` column for provider-bridge
  timeout, cancel, late, malformed and closed counters.
- The Home Manager module adds null-default `programs.thegn.git*` options for
  `[git]` scalars and `programs.thegn.stats*` options for `[stats]` icons and
  the disk path: `gitOverrideGpg`, `gitMergeGuard`, `gitAutoFetch`,
  `gitAutoFetchIntervalSecs`, `gitAutoFetchMinIntervalSecs`,
  `gitAutoFetchNotify`, `gitAutoFetchColocated`, `statsCpuIcon`,
  `statsMemIcon`, `statsNetIcon`, `statsGpuIcon`, `statsTempIcon`,
  `statsSwapIcon`, `statsFreqIcon`, `statsLoadIcon`, `statsUptimeIcon`,
  `statsBatteryIcon`, `statsBatteryChargingIcon`, `statsDiskIcon` and
  `statsDiskPath`.
- Tracker issue detail JSON includes `labels_complete`: it is `false` when a
  label list is truncated and `true` when the list is complete.

### Changed

- A queue agent whose sandbox cannot write the worktree or Git metadata is held
  with `cannot-write-worktree` or `cannot-write-git-metadata` instead of being
  started and failing.
- On startup, a saved `starting` share whose process exited becomes `failed`,
  and an `up` share that could not restart becomes `unknown`; `thegn share list`
  shows `(needs reconcile)` for the latter.
- A damaged CalDAV resource fails the whole sync page, leaving existing events
  and the sync position untouched. A sync pass is bounded at ten minutes, and
  accounts still fetching are reported as timed out.
- The state database schema is now v108. The migration adds PR-queue agent
  outcome, generation and timestamp columns. Restart the host and daemon after
  upgrading so they use the migrated database.

### Fixed

- The devcontainer image tag covers build-context files, so editing one rebuilds
  the image; the first launch after upgrading rebuilds once. If the bounded
  context digest fails, the warning explains that the tag falls back to the
  Dockerfile (when readable) and build declaration.
- A clone still recorded as `cloning` after its process exits is marked
  `abandoned` at the next start.

### Security

- A projection mount refuses refresh and unmount if another thegn replaced its
  ownership record after this process mounted it.

## [0.1.0-alpha.54] — 2026-11-05 (maintenance batch 92)

### Added

- `thegn issue tracker get|update|comment <provider>:<key> --account <name>`
  routes an issue through that named account; `--account` is required.
- URL clones record their credential-free canonical origin, thegn version,
  start time and validation time. `thegn repo provenance [PATH] [--json]` lists
  recorded clones with their current status: `verified`, `origin-changed`,
  `missing`, `unverifiable`, `cloning`, `interrupted` or `abandoned`.
- `thegn plugin preview --plugin-dir <DIR>` previews a resident app plugin.
  `--headless` prints a JSON snapshot of its first valid view, with `--wait-ms`
  bounding the wait; without it, views render live in the terminal.
- The Telemetry status modal adds a `plugins` section with current in-flight
  host calls and cumulative per-plugin counts for refused calls, timeouts, lost
  replies and event-queue drops. The section is absent while all displayed
  counts are zero.
- `thegn doctor`'s worker row checks a stage agent's relocated credential home
  (for example, `CODEX_HOME`) and reports `provider-home-unavailable` if it is
  missing or not a directory.
- The Home Manager module adds null-default `programs.thegn.drawer*` options
  for `[drawer]` scalar settings and `programs.thegn.stripVisible` for
  `[strip]` `visible`.

### Changed

- `thegn share stop` records a Tailscale share as `stopping` before teardown,
  so a later stop retries an interrupted teardown. `thegn share list` marks
  rows needing recovery, and `--json` includes a `recovery` field.
- The PR queue distinguishes a checkout Git could not read from one that is
  missing in its refusal and agent-dispatch hold messages.
- Shell completion's default time budget is 300 ms, up from 100 ms.
  `THEGN_COMPLETE_BUDGET_MS` still overrides the default.
- The state database schema is now v107. The v106 migration added clone
  provenance; v107 records merge landing OIDs. Restart the host and daemon
  after upgrading so they use the migrated database.

## [0.1.0-alpha.53] — 2026-11-05

### Added

- The Problems list shows an Info row with running, starting and backing-off
  language server counts while a server is starting or backing off.
- The Home Manager module adds null-default `programs.thegn.loc*` and
  `programs.thegn.replay*` options for the `[loc]` and `[replay]` scalar
  settings.

### Changed

- CI autofix claims a failing job once per provider run attempt (GitHub), so a
  re-run attempt of the same job at the same commit is a new candidate. A
  suggested fix's `needs_human` notification carries the attempt.
- The control API and daemon `ci.logs` reply include top-level
  `redaction_changed` metadata, matching `thegn ci logs --json`.
- `thegn host list` and `thegn host status` show ages in seconds below a
  minute, minutes below an hour, hours below a day, then days (for example,
  `1m ago`, `1h ago` and `1d ago`).
- The GitLab CI doctor note for a CLI-managed credential says remote and
  provider worktrees use the remote host's own `glab` login.
- (Dev channel) Managed SSH key names at VPS providers and Machine0 are scoped
  by provider and account; an identical registered key under an older name is
  reused. A restored remote pane whose SSH-over-WebSocket or provider exec
  attach fails gets one notice per kind per run as it takes another launch
  path.

### Security

- Machine0 provisioning attestation records and projection ownership manifests
  are written through held, owner-only directory handles.

## [0.1.0-alpha.52] — 2026-11-04

- Observe and plugin app tabs are now available on the stable channel.

## [0.1.0-alpha.51] — 2026-11-04

### Added

- `thegn doctor` prints a `worker` row for the current worktree when
  `[[pipeline.stages]]` are configured: `ready`, `NOT READY` with the first
  missing prerequisite, or `NOT PROVEN` where the platform or backend is
  unsupported.
- The Home Manager module adds null-default `programs.thegn.session*` and
  `programs.thegn.clipboard*` options for the `[session]` and `[clipboard]`
  scalar settings.
- The `examples/plugins/cell-grid` plugin draws typed input
  into a `grid` app widget.

### Changed

- When a pane cannot be daemon-backed and runs in-process, or a saved pane
  session cannot be reattached, thegn posts one notice per kind per run with
  the redacted reason; the reattach log line is redacted too.
- Daemon `ci.logs` requests for the same run and job share one provider fetch
  when log caching is enabled; with caching off, a waiting request fetches
  after the first finishes. A request that waits 120 seconds is refused as
  unavailable.
- A starting pane daemon reads at most the 256 newest registry rows per state
  directory and sweeps a larger pre-upgrade registry in steps, refusing to
  start with a retry message until it can check all of it.
- `thegn doctor` notes when a forge can merge but cannot fence the merge to
  the reviewed head, and that the PR queue holds its merges.
- The state database schema is now v105. Restart the host and daemon after
  upgrading so they use the migrated database.

### Fixed

- Interactive rebase, reword, amend, retag and move pin the HEAD commit they
  verify. If HEAD moves before Git starts, the operation is refused with
  nothing changed.
- `wt new --program` reports a member repository whose branch Git cannot
  read as failed instead of creating it, and the new-worktree wizard stops
  with Git's reason when it cannot list branches or resolve the base.

### Security

- The MCP secret-name index carries a generation checked at publish, so a
  concurrent update is refused instead of overwritten.

## [0.1.0-alpha.50] — 2026-11-04

### Added

- `thegn plugin preview` (dev channel) renders an app-tab fixture
  (`--fixture <file>` or the bundled `gallery`) or a recorded trace (`--replay`)
  with the shared app
  renderer, without starting a plugin process. `--headless` prints a JSON
  screen snapshot, `--size COLSxROWS` sets the geometry, and `--record` writes
  a new owner-only trace.
- The Problems list shows a warning row for a language server that failed to
  start and is backing off, with its failure count and retry time.

### Changed

- The state database schema is now v104. Restart the host and daemon after
  upgrading so they use the migrated database.
- Lead lease proofs for `dispatch put`, `dispatch claim`, `dispatch set-status`
  and `session open` now require `--lease`, `--owner` and `--lease-gen`; the
  audited `--admin --why` override is unchanged.
- Cached CI logs record their provider run attempt (GitHub) and whether
  redaction changed the text. The CI panel drill and background ingestion
  fetch a new log when the same run id has a new attempt, and
  `thegn ci logs --json` adds `redaction_changed` metadata.
- A `[model_proxy]` reload that moves `listen` to an address held by a foreign
  or incompatible listener keeps the running proxy instead of stopping it
  first.
- Cross-host merge-queue ingest refs carry the creating drain's claim
  generation, and a pre-ingest prune keeps refs from a newer generation.
- CI autofix authorization holds with a `needs_human` notification when the
  cached log belongs to a different provider context from the worktree's
  current context.
- `thegn wt rm --delete-branch` refuses failed or detached branch reads, and
  `wt new` reports that an empty base has no commits yet and distinguishes it
  from a Git read failure.

### Fixed

- Linear and Jira issue listings and searches no longer show or count an issue
  repeated on a later page.
- Switching worktrees cancels an in-flight submodule scan between its Git
  reads and drops its result.

### Security

- The managed-SSH custody ledger and MCP secret-name index use held,
  owner-only directory handles. Custody rows that are symlinks, hard-linked
  or over 64 KiB, inventories over 1024 rows, and non-owner-only directories
  are refused.

## [0.1.0-alpha.49] — 2026-11-04

### Changed

- `thegn merge rm`, re-enqueueing with `thegn merge add`, and deleting a
  worktree release a merge claim that expired on a row that already landed.
- Landed-worktree cleanup keeps the entry with class `identity` when the
  worktree was removed and re-created or now belongs to another repository
  since the branch was queued.
- The PR queue holds a merge as `needs_human` before requesting anything when
  the forge cannot fence the merge to the reviewed head.
- Configuration rejects `[ci] log_tail_lines` values above 100000. Log tails
  drop invalid UTF-8 lines and mark the tail truncated, keep CRLF line endings
  and discard an over-long line.
- The Home Manager module adds typed, null-default `programs.thegn.search*` and
  `programs.thegn.palette*` options for search and palette settings.

### Fixed

- Managed SSH key rotation separates a new key from an `authorized_keys` last
  line without a trailing newline and verifies the published file after
  authorizing or deauthorizing a key, failing instead of reporting success.
- Sandbox TIMELINE rows in the System panel show the event's age instead of
  `0s`.
- The rebase status check reports an apply-backend rebase or an in-progress
  `git am` as an error naming the marker instead of reporting no rebase.

## [0.1.0-alpha.48] — 2026-11-04

### Added

- Plugin app tabs (dev channel): resident API 0.4 plugins can contribute an
  `AppTab` workspace rendered by the new `thegn-plugin-ui` engine; F2/F3
  (`plugin-app-drafts`/`plugin-app-requests`) open draft and request recovery.
  Plugin API is now 0.4.0; 0.3 plugins load unchanged.

- (dev channel) Experimental dictation adds `thegn voice setup`, which
  configures embedded, HTTP or command transcription, and `thegn voice status`
  reports readiness without opening the microphone. `thegn voice model list|pull|rm`
  manages pinned, verified Whisper models in the user data
  directory; setup and pull report download size and ask before downloading
  unless `--yes` is used. `thegn doctor` adds a Voice block. Release builds
  omit native capture; HTTP and command providers need an external
  `capture_command` in those builds.
- `thegn doctor` reports where `pdftoppm`, `pdftotext` and `mmdc` resolve and
  whether each would run under `bwrap` (`preview_renderers` in `--json`), and
  reports the local provider bridge artifact's path, size, SHA-256 and
  resident path (`resident_bridge`). These checks do not start or install
  anything.
- `thegn share list` marks shares whose owning `thegn share start` process has
  exited (`owner_state` in `--json`), and `thegn share stop` names an exited
  owner.

### Changed

- The state database schema is now v102. Restart the host and daemon after
  upgrading so they use the migrated database.
- Daemon agent launches with rendered commands over 100 KiB are refused before
  launch, with the command size and limit, instead of failing at exec.
- The `devcontainer` CLI provider's trust digest covers Compose references
  (`env_file`, build Dockerfiles, `extends` and `include`). Absolute, remote,
  interpolated or repository-escaping references refuse provider use; provider
  exec and session lookup also recheck config containment.
- Landed-worktree cleanup skips rows held by another live drain claim; the
  `merge sweep` marks them `merge_claimed`, reports incomplete and does not
  start a retry delay.
- `thegn ci logs` and the control API's `ci.logs` do not reuse cached logs
  from another known provider context. The PR queue quotes cached logs only
  for the current context.
- GitHub issue listings and searches that exceed the 100-issue cap without a
  limit are reported as partial instead of complete. A full Kaneo project page
  is partial unless the requested limit was met.
- The control API's `dispatches.put` normalizes issue ids and worktree paths
  like `thegn dispatch put`, and rejects an unusable worktree before writing.
- Worktree issue links are bound to the account that produced them.

### Fixed

- A share owned by a running `thegn share start` process is no longer
  restarted when the thegn window opens.
- Worktree creation and autopilot refuse when Git cannot read branches,
  worktrees or the base branch, instead of assuming none exist or silently
  choosing a default.
- A clone holds an exclusive destination claim while it runs; a second clone
  to the same destination is refused while the first process is alive.
- The System > Telemetry LOOP row reports `no samples` or sample-aware render
  tails (`sparse`, p95 and p99) instead of a bare p99 on an empty window.
- On Linux, `./install.sh` validates the generated desktop entry before
  publishing it and restores the previous installation if validation fails.
- (dev channel) Cancelling a merge-queue drain also stops the consumer's
  cross-host `git fetch` from the transferred bundle.
- (dev channel) Opening a worktree against an existing provider sandbox
  verifies its non-empty network policy, reapplies drift, and refuses an
  unverifiable sandbox without destroying it.

### Security

- GitLab CI clients stay bound to the origin snapshot that selected them, so
  a changed remote cannot redirect a credential-bearing API call.
- (dev channel) Credentialed dictation HTTP endpoints require HTTPS unless
  their host is loopback. Setup and configuration validation reject
  credentialed plain HTTP; redirects are not followed.

## [0.1.0-alpha.47] — 2026-11-03

### Added

- `thegn doctor` reports stored problem counts for configured calendar
  accounts, from each account's last applied page, in text and `--json`,
  without creating the state database.
- `--lease-gen` on dispatch claim, put and set-status and on `session open`
  fences Lead mutations to the acquired lease generation; stale generations are
  refused before writing.
- `thegn api coverage` reports a disposition and any review trigger for each
  excused control-surface gap, in text and `--json`.
- The Home Manager module exposes `confirmDelete` and scalar `[theme]` options
  for preset, focus border, pane padding, undercurl, color, glyphs, agent glyphs,
  mascot and mascot motion.

### Changed

- The state database schema is now v100. Restart the host and daemon after
  upgrading so they use the migrated database.
- `thegn merge history` records a refused queue-aware `thegn merge land` as a
  `land` event with its refusal outcome when a matching queue row exists; the
  queue row remains unchanged.
- Theme save and import refuse before writing when another theme file claims
  the same exact name or slug.
- PR review-task refreshes cannot add or reactivate a task when the active
  roster has reached its 4,096-row limit; revising an active task remains
  allowed.

### Fixed

- Oversized agent prompts are shortened to fit the command line and prompt
  environment variables, with an omission marker, so they do not prevent the
  agent from starting.
- Worktree creation is refused without changes when Git cannot verify whether
  the branch already exists; failed branch renames and worktree moves include
  Git's reason.
- Quitting during a scheduled calendar refresh starts no further account
  fetches; fetches already in flight may finish.
- Orphan detection keeps a live worktree's main container when its slug ends in
  `-tgvpn` or `-tgagent`.
- (dev channel) The x86_64 Linux `.#dev` package sets `THEGN_BRIDGE_BINARY` to
  its bundled static provider bridge unless the variable is already set.

### Security

- (dev channel) SSH data projections (sshfs or sync) with different SSH extra
  arguments have different identities; a mismatch is refused before any mount,
  pull or push. Existing SSH projections with non-empty extra arguments may
  need to be recreated after upgrading.
- (dev channel) Managed SSH custody records use a monotonic generation, so a
  revoked and republished key cannot match a stale rotation snapshot.

## [0.1.0-alpha.46] — 2026-11-03

### Added

- The control API's `issues.get`, `issues.update` and `issues.comment` accept an
  optional `account` to route each issue to its configured tracker account.
- With `THEGN_PERF=1`, the perf rollup adds p95 latency keys; the status modal
  shows sample counts for sparse loop windows and `no samples` for empty ones.

### Changed

- The state database schema is now v99. Restart the host and daemon after
  upgrading so they use the migrated database.
- CI log cache entries and autofix claims record their provider context.
  Background ingestion reuses a cached log only for the same context; a
  failure from another provider project becomes a new autofix candidate.

### Fixed

- Rebase, move, retag, reword and amend refuse a shallow-clone boundary with a
  hint to fetch more history, instead of rewriting it as a root commit.
- (dev channel) Cancelling a merge-queue drain stops an active cross-host bundle
  transfer and reports cancellation as `gate_error`, rather than unreachable;
  fetching the bundle is bounded by the same subprocess deadline.

### Security

- On Linux with usable `bwrap`, PDF renderers run without network access and
  with only private scratch writable; otherwise they run as before.
- The `devcontainer` CLI provider re-checks the selected config before
  snapshotting and starting, and refuses a file that is no longer regular and
  inside the worktree. If the later check refuses the CLI provider, thegn warns
  and uses its native OCI path for the supported approved subset.
- A language-server executable is re-checked before start; changed binaries and
  paths that are not valid UTF-8 are refused.
- (dev channel) Warm-pool claims verify the worktree's network egress policy on
  the spare, re-apply drift, and quarantine an unverifiable spare before fresh
  provisioning.
- (dev channel) Remote and bridge worktree listings reject forged or ambiguous
  records instead of returning a partial list.
- Sandbox image builds no longer use registry credentials; each architecture's
  image and the manifest list carry build-provenance attestations.

## [0.1.0-alpha.45] — 2026-11-03

### Added

- `./install.sh --uninstall` (with `--dry-run`) removes unchanged files recorded
  by the install manifest, keeps and reports changed files, preserves config,
  and refuses a malformed manifest before removal.
- MCP `tools/list` marks tools with `readOnlyHint` and `destructiveHint`
  annotations so clients can distinguish read-only, destructive and other
  writes.
- With `THEGN_PERF=1`, the `thegn::perf` rollup adds a separate
  `sched admission` event with cumulative background-work counters.
- (dev channel) `thegn doctor` reports the host-cache signer for provider
  environments that use it, without exposing key material; JSON includes its
  signer state.

### Changed

- `thegn dispatch lease acquire` prints the lease generation, also in `--json`.
  A first take, or a take after expiry, gets a higher generation than any
  before it; a live same-owner acquire keeps the current one.
- `thegn merge history` records outcomes from `thegn merge land`, its panel
  action, `thegn land` and `thegn integrate`.
- Dispatch refuses a new row before writing when 4,096 rows are active.
- `issues.list` returns `issues`, `complete` and `incomplete`; validated rows
  from partial accounts are kept and the affected accounts are reported. The
  request fails only when every configured account fails.
- Hook-run logs start with `thegn-hook-run 1` and include a unique run id and
  start/finish times; the command is recorded as `[redacted]`.
- Queue-agent and autopilot sandbox refusals lead with the refusal class and
  its fix. The queue records the refusal reason without starting the agent.
- The Media section keeps the last player list and marks it “sources may be
  stale” or “sources unavailable” when listing players fails; it marks the
  up-next queue “queue unavailable” when that cannot be read.
- A pane clipboard copy discarded because the pane exited or restarted adds a
  fixed status note; the copied text is never included.

### Fixed

- The Environments section no longer marks a default environment bound when
  the worktree selection cannot be read; it reports `binding unreadable` and
  refuses to rebind.
- Stale `{prompt_file}` files left by an interrupted local agent run are
  removed when a later run prepares that directory, after 30 days.
- GitLab CI no longer sends API requests to the SSH port from an `ssh://`
  remote with a non-default port.
- An open CI run drill fetches its run details and logs once instead of again
  on every CI refresh.
- After a configuration reload, a calendar refresh still working through
  accounts under the old settings starts no further account fetches; fetches
  already in flight finish.
- (dev channel) Managed SSH key provisioning, rotation, completion and revoke
  share one lock, preventing provisioning from overwriting an unfinished
  rotation record and preventing two rotations from both succeeding.

### Security

- (dev channel) After a checkpoint restore, `thegn env restore` and warm-pool
  recycle verify the environment's network egress policy, re-applying it if
  needed. An unverifiable restore fails the command or destroys the spare.

## [0.1.0-alpha.44] — 2026-11-03

### Added

- `thegn merge history` shows the target base of each merge-queue drain
  landing, in text and `--json` output.

### Changed

- IDE handoff is outbound only: thegn opens your editor and registers no
  `thegn://` handler, OS association or IDE extension.
- Compose preflight distinguishes a missing service, no running container,
  and a scaled service whose replica 1 is not running.
- Pipeline reports and notes replace hostile characters with U+FFFD instead
  of silently deleting them.
- When native MPRIS cannot connect or read a player, the playerctl fallback
  offers shuffle, repeat and volume only when reported; seek requires a track
  length.

### Fixed

- A first launch that races another process creating a new state database
  waits up to 5 seconds for it to finish, instead of reporting an old-schema
  refusal. An existing older database still gets that refusal.
- A CalDAV changes-only reply without a sync token falls back to a full fetch.
- `thegn mcp secret set` and `rm` update the keyring and MCP secret index
  under one lock; a corrupt index is refused before the keyring is touched.
- A sandbox events watcher killed during shutdown is waited on even if the
  child reaper is full or unavailable, avoiding a zombie process.
- A failed share restart deletes its own saved share row, so it does not
  return as a stale share on the next start.
- (dev channel) Filesystem snapshot publication refuses to overwrite an
  existing snapshot. The S3 store accepts a manifest only at its matching
  `<id>/manifest.json`, caps object reads at 1 GiB and follows no redirects.

### Security

- A resurrected pipeline-stage pane keeps project settings off; a removed
  stage is refused and the shell fallback remains.
- Pipeline handoff files are read without following symlinks and must be
  bounded valid text: at most 1 MiB and 8,192 lines, without NUL, invalid
  UTF-8, terminal controls or bidi overrides.
- `session open --resume-work` refuses any parent other than the row being
  resumed when that parent is missing, belongs to another issue, or is failed,
  abandoned or unknown.
- CI autofix re-checks the worktree HEAD just before starting its agent; a
  moved or unreadable HEAD prevents the start and holds the row as
  `needs_human`. A successful attempt refund is reported as no attempt spent.

## [0.1.0-alpha.43] — 2026-11-02

### Added

- `[lsp] require_containment = true` refuses to start a language server that
  cannot join the thegn resource slice; the default keeps starting it with one
  warning, and `thegn doctor` shows which applies.

### Changed

- `thegn merge history` also lists the merge-queue drain's own status
  transitions, with the landed commit as the result.
- `thegn share start` refuses a port that already has a saved share and records
  it as `starting` before the tunnel comes up; `thegn share stop` also tears
  down a Tailscale share left `starting` or `failed`.
- A plugin declares at most one `IssueProvider`; `thegn plugin check` reports a
  further one as rejected. The `io` host verb is reserved and answered with an
  `unsupported` error.
- (dev channel) The Hetzner provider is labelled experimental: `thegn doctor`
  and the configuration example say its lifecycle is not verified on a live
  account.

### Fixed

- A sandbox launch whose container name is held by another worktree's
  container, or whose owner cannot be read, is refused with `container-owner`
  instead of removing that running container.
- A Tailscale share reports its URL only once `tailscale serve status` shows
  it; otherwise the serve is turned off and the share fails.
- A media playlist listed for one player is not activated on a different
  player; the pick is skipped with the "player changed" message.
- Each merge-queue drain removes leftover `refs/thegn/mq/` ingest refs that no
  queued entry uses.
- (dev channel) Provisioning refuses to overwrite a managed SSH key record
  whose rotation is unfinished and points at `thegn secret ssh rotate`. A
  second provisioning of the same host-placed container while one runs is
  skipped; the next open checks again.

### Security

- Queue agents and autopilot see the repository's `.git/hooks` and the
  worktree's `config.worktree` read-only, so they cannot plant a hook that
  host-side git later runs.
- A named-profile startup also unsets credential names ending in `_KEY`,
  `_AUTH`, `_CREDENTIAL`/`_CREDENTIALS` or `_PASSPHRASE`, and
  `DOCKER_AUTH_CONFIG`.
- (dev channel) An unreadable environment selection halts a launch when a
  remote placement is configured instead of using the default environment;
  the resident bridge also scrubs secret-shaped text from command error output.

## [0.1.0-alpha.42] — 2026-10-28

### Added

- `thegn merge history` lists a worktree's recorded retry, remove and clear
  outcomes, newest first, with `--worktree`, `--limit` and `--json` options.
- `thegn doctor --json` reports a skipped non-executable `tgproxy` sibling as
  `model_proxy.tgproxy_skipped`.

### Changed

- Creating, opening or cloning a project runs database and git work off the UI
  thread and shows the `Opening <name>…` spinner while it opens.
- `thegn wt new --from-issue` names plugin issue providers it skips.
- `[forward]` uses the one container runtime holding the worktree's container;
  if multiple runtimes have one with that name, forwarding pauses with a
  status-line explanation.

### Fixed

- Deleting the active project switches to the first remaining project; deleting
  the last leaves an empty view.
- Disabling or reloading a plugin stops its in-flight one-shot run, and failed
  one-shot runs are reported on the status line.
- An unverified pane-daemon health check makes `thegn daemon stop` exit
  non-zero naming it and prevents startup from starting a second daemon beside
  it.
- (dev channel) Closing a worktree with `data = "sync"` skips files excluded
  by `.git/info/exclude` or the global git excludes file; an unreadable warm-pool
  ledger warns before provisioning fresh; autoscale keeps a host whose age and
  use are both unknown; and idle hibernation keeps a sandbox when it cannot
  tell whether a shell is attached.
- Closing a tab whose worktree location cannot be read falls back to path-only
  teardown and warns that a remote sandbox may remain.

### Security

- (dev channel) If the worktree location cannot be read, pane launches and
  provider connections are refused when a remote placement is configured;
  without one, panes use the local path.

## [0.1.0-alpha.41] — 2026-10-23

### Changed

- Managed difftastic downloads are disabled until reviewed SHA-256 pins ship;
  `auto` structural diffs use `[managed_tools.difft] path` or `difft` on
  `PATH`, and `thegn doctor` shows which resolved.
- `thegn issue list` names plugin issue providers it skipped; those providers
  are served only by the running UI.
- A non-executable `tgproxy` beside `thegn` is skipped for `PATH`, and
  `thegn doctor` names the skipped file.
- (dev channel) With `data = "sync"`, closing a worktree compares files with
  the upload baseline: unchanged sandbox files do not overwrite local edits,
  conflicting edits stop the sync and keep the sandbox, and `.gitignore` or
  `.ignore` exclusions are skipped.
- (dev channel) The resident bridge connects using the worktree's pinned
  environment or workspace default. Failed agent-login uploads to a sandbox
  mark “Sync agent logins” Failed with a line per account, while provisioning
  continues. `thegn placement plan` uses the selected environment.

### Fixed

- A split preset reports failed panes and adds its tab only after a pane
  starts.
- If a systemd pane's protected environment file cannot be created, it starts
  without the host-sourced secrets and warns with their keys, never values.
- Deleting a project while the state database is busy reports that promptly
  instead of freezing the UI; workspace focus changes no longer wait on the
  database.
- (dev channel) Autoscale keeps an engine host when its tenant count or last
  use cannot be read, and warns with the host name.
- (dev channel) A stray `.git` entry in the temporary directory no longer
  disrupts hibernation bundle checks.

### Security

- More worktree operations refuse when the recorded location cannot be read:
  control API commit, review handoff, autopilot push and worker sandbox,
  `thegn issue create` and `thegn issue comment`, PR creation, `thegn env`
  resolution, `thegn repo trust` and submodule init. Background fetch and
  merged-worktree cleanup skip that worktree. (dev channel) Hibernation
  refuses and its bridge-key drop warns and skips; provider provisioning,
  provider and host worktree pipelines, host placement and provider attach
  refuse too. A plain local worktree is unaffected unless a provider or host
  placement is configured.
- An unreadable env-bundle or coding-agent account selection refuses a
  configured agent launch; a shell starts with a warning instead of using the
  default account.
- On Linux, stopping a resident plugin also kills descendant processes it
  started in a new session, such as with `setsid`.

## [0.1.0-alpha.40] — 2026-10-23

### Changed

- Editing `[model_proxy]` takes effect on config reload while the UI runs:
  disabling stops the supervised proxy, enabling starts it and other changes
  restart it. Renaming a worktree with a merge-queue row is refused; remove the
  row with `thegn merge rm` first.
- Launch-menu and `thegn open --preset` warnings, and pane spawn failures, also
  appear on the status line, in a toast and in the notification inbox.
- (dev channel) Failed SSH sandbox teardowns and skipped or failed close
  auto-checkpoints appear on the status line and in a toast.

### Fixed

- Launching several agents at once into a newly added repository no longer
  fails with "database is locked"; a pane launch briefly retries a
  transiently locked state database instead of refusing.
  `thegn pr --worktree` resolves a symlinked or nested spelling to the
  registered worktree's PR state.
- (dev channel) Reopening a worktree adopts its pending machine0 VM instead of
  creating a duplicate. Autoscale reserves a newly created host for the
  worktree; a blocked or failed scale-down reports that the host keeps billing.

### Security

- Worktree writes and code runs refuse when the recorded location cannot be
  read or is unrecognised, rather than acting on the host checkout. This covers
  panel git actions, Tests and Jobs, `thegn land`, mutating `thegn pr` actions,
  `thegn ci rerun`, `thegn ci trigger` and `thegn ci cancel`, worktree delete,
  Sandbox container stop or restart, clipboard image paste and toolchain
  install.
- An unreadable environment selection no longer applies the default
  environment's trust policy: trust-gated devcontainer steps and devshell
  injection are skipped with a warning, and `thegn doctor` reports unknown.
- The environment chip and loading splash show the unreadable state, and a
  failed pane connection is not blamed on a guessed provider; an unreadable
  account selection refuses a provider pane. (dev channel)
- (dev channel) Reopening a hibernated worktree refuses to provision a fresh
  sandbox when its hibernation record cannot be read while hibernation is on.

## [0.1.0-alpha.39] — 2026-10-23

### Changed

- Launch-gate warnings also appear on the status line and in the notification
  inbox while the UI is running.
- `thegn doctor` reports where it found `tgproxy`, or that it is not installed.
  The release tarball and Nix flake ship only `thegn`; build the proxy from
  source.
- Adding a managed account picks the lowest free name and never reuses an
  existing account home.
- A burst of media player-change notifications is read at most every 250 ms
  (Linux).

### Fixed

- PR-queue and CI-autofix agents that never start no longer spend an attempt;
  the row is held as `needs_human` with the reason.
- `thegn pr` actions other than `thegn pr queue` refuse an explicit
  `--worktree` outside a registered remote worktree or local git worktree
  before contacting the forge. A nested local path targets its worktree root.
- `thegn merge land`, its queue-section action and the drain ask you to
  re-enqueue a branch that moved after verification with `thegn merge add`.
- A cached running or pending CI run no longer counts toward working attention
  while that worktree's CI refresh is failing. A cached failure still counts
  (pending PR checks still count).
- The daemon removes dead, relocated `/tmp` socket directories at startup.
- Unreadable or malformed VPS and Fly ledger records are refused instead of
  being treated as absent by destroy, retarget, address lookup and the
  `vps-ssh` bridge (dev channel).
- A workspace clone is not promoted if its parent directory is replaced while
  it is cloning; the staging directory is left for the startup sweep, and the
  parent directory must be owned by you.

### Security

- An unreadable worktree environment selection no longer falls back to the
  default. `thegn env` reports an error, worktree deletion refuses before
  removing anything, and tab close uses path-only cleanup with a warning.
  Provider SSH connect, warm-pool operations, hibernation, provisioning, host
  placement and the close auto-checkpoint are refused or skipped with the
  reason (dev channel).
- SSH keys registered with Hetzner, DigitalOcean and machine0 are named from
  the first 32 hexadecimal characters of the SHA-256 of the decoded public
  key, and an invalid OpenSSH public key is refused (dev channel).

## [0.1.0-alpha.38] — 2026-10-23

### Fixed

- PR queue and CI autofix report why an agent did not start instead of treating
  a refused launch as an ordinary agent failure. The attempt was still counted.
- `thegn forward list` and `thegn forward stop` report unreadable recorded
  state as an error instead of an empty answer, and `forward stop` says when no
  forward was recorded for that port.
- `thegn placement list` reports unreadable placement state as an error, and
  `thegn host rm` refuses, removing nothing, when it cannot read the host's
  placement tenants (dev channel).
- One one-minute deadline bounds a whole GitLab run detail. When it cuts off a
  later job page, `thegn ci view` and `ci logs` warn that the job list is
  incomplete and show the jobs read so far; a cut-off first page is an error.
- The merge-queue drain lands the commit it verified; it refuses a branch that
  moved after verification. A failed PR fetch no longer changes the queue
  row's status. Local worktree listing preserves paths containing newlines.
- A workspace with an unreadable removal record starts as a temporary
  workspace with a warning instead of being registered again. Late lifecycle
  messages from a reloaded plugin no longer change the new plugin's state.
- On the dev channel, the Sprites list follows every page and refuses
  incomplete results; warm-pool recycle destroys a spare when its base
  checkpoint cannot be confirmed. Machine0 creates keep a pending reservation
  across an unknown outcome, count it toward `max_instances`, and adopt the VM
  on retry.

### Security

- Configured agents and tools are refused when the repository root or selected
  execution environment cannot be read from state; a plain shell still starts
  with a warning. PDF and Mermaid previews refuse renderers in any worktree of
  the previewed repository, and refuse when its worktree list is unreadable.
- Configured provider ids for Sprites, Hetzner, DigitalOcean, Fly and machine0
  must be lowercase (dev channel). Host provisioning checks staged files
  inside the container and marks provisioning complete only for the applied
  plan (dev channel).

## [0.1.0-alpha.37] — 2026-10-23

### Changed

- `thegn pr queue rm` refuses to remove a row while its PR agent is running;
  `clear` removes other rows, names the kept PRs and exits with an error.
  The PR-queue section's `x` and `c` actions report the same result. `thegn
session move` drops pane working directories outside the moved worktree, so
  those panes start in the worktree root. The daemon's `ci.runs` (and `ci.logs`
  when nothing is cached) refuse a worktree with no CI provider with a
  `no-provider:` failed-precondition error. CI autofix acts on the one active
  PR-queue row for the failing worktree and holds with a notification when
  several active rows share it.

### Fixed

- `thegn merge land` and the merge-queue section's `l` fold exactly the commit
  the queue verified; a branch that moved since is refused before anything is
  folded. A workspace clone refuses a destination directory that already
  exists when it is promoted, and staging left by crashed clones is reclaimed
  at startup. MPD controls the server does not permit for the connection are
  no longer sent, and a burst of player-change notifications is read once. A
  keyring write or delete that does not answer in time is treated as a
  retryable unknown outcome instead of a denial. The pane daemon exits and
  removes its socket when SIGTERM arrives right after it starts.
- VPS destroys retire their record only after the instance is absent from the
  provider inventory, and machine0 destroys refuse when the VM lookup is
  unclear instead of treating the VM as already gone (dev channel).

### Security

- Configured agents and tools are refused when the state database cannot be
  opened at launch; a plain shell still starts. PDF and Mermaid previews do
  not run a renderer found inside the previewed repository. `file:` secret
  references in the push inbox, share and issue tokens, the embedded git UI
  and the proxy go through the same owner-only check (and size cap) as other
  `file:` secrets. A managed account home that another account's legacy name
  also matches (`work` vs
  `Work`) is refused instead of being reused. The macOS launcher passes
  `--env` values as data, never as shell code (macOS).
- Provider close-time downloads stay within held directory handles; unreadable
  warm-pool bindings refuse instead of targeting a derived sandbox; Sprites
  sshd setup refuses a symlinked `authorized_keys`, ignores commented-out
  copies of the key, and refuses an unparsable managed key before custody
  (dev channel).

## [0.1.0-alpha.36] — 2026-10-23

### Changed

- When an active drain holds merge-queue rows, `thegn merge add`, `merge rm`,
  `merge retry` and `merge land` refuse them; `merge clear` removes unclaimed
  rows, names the rows it keeps and exits with an error. The control API
  answers `conflict` for these refusals on add and clear. A row queued by an
  older thegn without a worktree identity is held until `thegn merge add`
  re-enqueues it.
  `thegn ci runs --json` returns a `no-provider` error object when no provider
  is configured; `[]` means a configured provider returned no runs.
- An explicit `[merge_queue] gate_target_dir` is split per repository;
  isolated gates use private targets. `thegn doctor` warns about
  `[disk] shared_target_dir` and reports the `gate_target_dir` layout. In the
  share panel, `o` explains when a share has no browser link, and `↵` copies
  its endpoint.
- Machine0 inventory lists reject unrecognised shapes, unidentifiable rows
  and duplicates. Machine0 readiness, status and SSH-auth waits, and Fly's
  sshd probe, count in-flight requests against their total time budget. A
  failing local `tar` or `git` producer fails a host transfer instead of
  sending a partial stream (dev channel).

### Fixed

- A late event from a replaced resident plugin process no longer stops the new
  process or schedules an extra restart. The files panel reads local Git paths
  as NUL-separated records, preserving names with embedded newlines.
- Quitting no longer waits on an unbounded terminal flush, and the crash-path
  terminal restore cannot block.

### Security

- Sandbox mount guards protect thegn's control-socket directories and state
  `run/` directory: bwrap hides them; systemd makes them inaccessible in
  read-only-root profiles (hardened, sealed) and refuses a profile if the
  mask would hide the worktree or a carved-back mount; container backends
  refuse every mount exposing a control-socket directory and refuse only
  writable mounts covering the state `run/` directory (a read-only `$HOME`
  mount over `run/` stays allowed). The `all` and `host` file-access modes
  expose the host by design.
- Thegn's info, warning and error output, including the final error line,
  replaces control and invisible formatting characters so hostile names or
  messages cannot inject terminal escape sequences.

## [0.1.0-alpha.35] — 2026-10-23

### Added

- `[forward] allow_external_bind = true` opts in to forwarding ports on a
  non-loopback address. `thegn config validate` warns when
  `max_lifetime_secs` is set for a provider without a lifetime reaper (dev
  channel).

### Changed

- Usage alerts keep one inbox entry per account, window, alert level and reset
  period. Repeats update it in place, and a dismissed alert stays dismissed
  until the window resets or usage recovers; existing duplicate alerts are
  merged when the state database is upgraded. A `Needs you` process failure
  clears when acknowledged, when the same program later exits cleanly, when
  its worktree is removed, or after seven days.
- Stopping a bore, frp or iroh share remains `stopping…` until its tunnel
  client exits. `thegn merge land` and the panel's land key require the branch
  to still point at the commit the queue verified. With the `rebase` strategy,
  a branch whose history is unrelated to the target or contains a root commit
  is held for a human before replay instead of failing the fold.
- A plugin host call newer than the plugin's declared API is refused as
  unsupported. On the stable channel, SSH and provider `[env.*]` environments
  are removed, and a worktree selecting one halts by default (or prompts with
  `ask`) with a hint to set
  `THEGN_CHANNEL=dev`. The README and help clarify the alpha's limits,
  including plugin isolation, sandbox access to `$HOME`, and MCP write scopes.

### Fixed

- Clearing an inbox row no longer lets an older background refresh restore it.
  Media controls are not sent when the player's state cannot be read, and a
  slow read after a control no longer reports that control as timed out.
- System-monitor graphs retain their colours, including the Thermal and
  Network ramps, dim Power labels, and the background of stacked graphs.
  Staging directories left by a crashed URL clone are cleaned up on a later
  clone. Daemon startup no longer waits indefinitely on a held control-runtime
  allocation lock. Language servers log one warning when a configured CPU cap
  cannot be applied. `just release-artifacts` honours `CARGO_TARGET_DIR`.
- Accounts that resolve to the same credential directory through a symlink or
  `..` path are refused.

### Security

- GitHub reviews are pinned to the head commit shown by the panel; a forge that
  cannot pin a review refuses to submit it. `file:` secrets are read only
  from a regular file owned by you or root, with no world access, one hard
  link and at most 64 KiB; final-component symlinks are followed and checked,
  and group access produces a one-time warning. Keyring reads distinguish
  missing, denied and unavailable secrets.
- `thegn session move` refuses bundles over 4 MiB, pane metadata deeper than
  32 levels, or objects with duplicate keys before listing or stopping
  sessions. Non-loopback `[forward] bind` values are refused unless
  `allow_external_bind = true`.

## [0.1.0-alpha.34] — 2026-10-22

### Added

- `thegn merge sweep --json` reports kept branches with a cleanup class and
  reason, and its `incomplete` field matches the command's exit status.

### Changed

- Merge-queue rows are held for review if the worktree's repository identity
  changes before the drain. PR-queue failure prompts can include a bounded,
  redacted log excerpt for that PR head. One-shot plugin runs give the
  least-recently-started work priority, and results from a plugin disabled,
  reloaded or stopped mid-run are dropped.

### Fixed

- Partial or failed CI run lists no longer replace the last good list; the
  cached list is labelled stale, and unavailable job details stay visible.
  Repository discovery skips roots nested inside another configured root.
- Exit no longer hangs when a frame write is stuck; terminal settings are
  still restored. Font changes report when the file was applied but its
  directory was not synced.
  Projection rollback timeouts report a transport failure instead of claiming
  that the projection is not mounted (dev channel).
- Fly machine lists reject malformed entries and multiple live managed
  machines rather than choosing one (dev channel). Machine0 MCP calls use the
  shared provider control gate (dev channel).

### Security

- `thegn session` and `thegn logs tail` keep untrusted text from emitting
  terminal control or formatting sequences. The managed file secret store
  refuses token files with group or other access, or extra hard links.
- Sandbox mounts whose source lies inside a daemon control-socket directory
  are refused, including for OCI and Compose. Media labels strip invisible
  formatting characters, and font family names with invisible or bidirectional
  characters are refused.
- Invalid VPS names are refused instead of sharing a fallback `known_hosts`
  pin (dev channel).

## [0.1.0-alpha.33] — 2026-10-22

### Added

- `just release-verify <tag>` rehearses a local archive and refuses a binary
  tied to a `/nix/store` interpreter unless `host_only=1` is supplied.
- `thegn doctor` reports unbound claimed and quarantined warm-pool spares, and
  VPS/Fly reapers alert when the set of billable resources kept for review
  changes (dev channel).

### Changed

- README, release instructions, known issues and in-app help describe the
  public-alpha platform and install-channel support, including that
  `install.sh` is for source checkouts.

### Fixed

- An unsafe predictable `/tmp` control fallback no longer prevents daemon
  startup; clients find its private replacement through an owner-only locator.
- Failed share stops remain visible and retryable. PR check reruns target the
  current head, and approvals and reviews refuse if that head has moved.
- After a PR-queue or CI-autofix agent returns, thegn rechecks the PR context
  before follow-up writes and holds the row for human review if it changed.

### Security

- Release publication requires an annotated, version-matching tag admitted
  through a protected environment; release checkouts do not persist GitHub
  credentials.
- Named-profile directories and profile SSH keys must be owner-only. Plugin
  launch fields are validated and bounded, and bore secrets in
  `[share.bore] extra_args` are refused without echoing the value.

## [0.1.0-alpha.32] — 2026-10-22

### Added

- Calendar reminders report skipped-event categories, and worktree aliases
  resolve consistently across issue notifications and automation.
- Stale media badges are cleared, and pane user-bus visibility can be hidden.
- Pool halts and deletes keep their state visible and preserve the row on failure
  (dev channel).

### Changed

- CI log excerpts honor configured limits on cached and served results; CI
  details stay bound to their originating worktree. The workspace picker
  avoids redundant directory checks.
- Doctor reads state without creating or migrating the database and resolves
  toolchain locations through its read-only handle.
- Merge cleanup retries use bounded backoff, recover expired drain claims,
  and report deferred cleanup rows once per retry window. Soft-cap refusals
  identify the reason (dev channel).

### Fixed

- Control socket masks preserve inaccessible-node sources while refusing
  mounts that would hide a bind or expose the privileged socket.
- Terminal frames use the writer queue, keeping frame writes ordered and
  bounded.

### Security

- Release archives require matching attestation identity. Managed tool
  provenance reasons remain visible in doctor output.

## [0.1.0-alpha.31] — 2026-10-22

### Added

- Dispatch history is available in bounded newest-first pages, and issue and
  worktree aliases resolve consistently across pipeline views.
- Calendar reminders, plugin alerts and media shutdown use coordinated,
  bounded delivery and cleanup.

### Changed

- Results appear only for matching branches and provider contexts. Incomplete
  details include a warning.
- `thegn config show`, `get` and `explain` redact calendar credentials and
  other sensitive values.

### Security

- Font configuration replacement uses an exclusive staged file, and release
  archives with sparse members or truncated data are refused. Lost PR-agent
  leases cancel the run; pool reconciliation stops on database read errors.

## [0.1.0-alpha.30] — 2026-10-22

### Added

- Player lists report truncation; calendar validation reports rejected-event
  categories; tracker notifications identify an issue's account.
- Startup warnings and merge cleanup results are surfaced in the status line.

### Changed

- CI log payloads remain bounded, and attention ignores results from another
  provider context. Dispatch history supports bounded paging.
- Player and workspace discovery work is capped to keep large inventories from
  stalling the interface.

### Security

- Managed tools require matching provenance records. VPS reservations count
  only records for the same provider account, and unsafe execution or socket
  probes fail closed.

## [0.1.0-alpha.29] — 2026-10-22

### Added

- The Merge queue shows claim owners and lease state. Issue rows retain their
  tracker account, and issue details indicate incomplete comment lists.

### Changed

- CI operations can be canceled with Ctrl-C; logs and media queues use bounded
  pages and distinguish errors from empty results. CI log excerpts retain the
  first failure within a bounded window; provider results use bounded pages
  (dev channel).
- Dispatch history and `supervise plan/status --all` support bounded paging.

### Security

- Terminal launches use opaque sandbox admission. VPS, Fly and warm-pool
  operations refuse account mismatches or unreadable ledger data (dev channel);
  share backends are limited to supported built-ins.

## [0.1.0-alpha.28] — 2026-10-21

### Added

- `thegn doctor` reports sandbox execution routes and admission status.
  `thegn merge list` shows claim owner and lease details.

### Changed

- Cached CI results and logs are trusted only for a matching provider, branch
  and commit; expired PR-agent claims are reconciled before work resumes.
- Issue rows preserve account identity, and media picker results distinguish
  errors from empty lists.

### Security

- Remote bridge binaries, provider credentials, calendar plugin output and
  managed VPS/Fly records are checked against recorded identity before use
  (dev channel).

## [0.1.0-alpha.27] — 2026-10-21

### Added

- The Overview monitor places its power panel only when readings are
  available.

### Changed

- CI results are accepted only for the current branch and provider context;
  pipeline issue aliases resolve consistently. Headless agent limitations are
  reported.

### Security

- Remote worktree sync, projection mounts and provider execution use staged,
  bounded operations (dev channel). Unsafe plugin surfaces and storage are
  refused or reported explicitly.

## [0.1.0-alpha.26] — 2026-10-21

### Added

- `thegn pr merge` and the panel verify the reviewed commit, refusing a
  changed pull request. `session move` drops history by default;
  `--include-history` opts in.

### Changed

- Calendar cancellation updates remove canceled occurrences. Playlist, player,
  preview and CI results have explicit limits; provider results have explicit
  limits (dev channel). Media and daemon work can be canceled cleanly.

### Security

- Font replacement refuses foreign-owned or hard-linked files. Daemon stop
  reports listeners it could not verify and exits non-zero.

## [0.1.0-alpha.25] — 2026-10-21

### Added

- Agent handoffs return explicit outcomes.
- Media queues expose bounded pages and watcher health; calendar page updates
  are atomic and include diagnostics.

### Changed

- VPN sidecar reuse now checks the resolved tunnel configuration. An
  unlabelled sidecar left running from before this upgrade is refused; stop
  that worktree's
  sandbox so the sidecar can be recreated with the current configuration.
- `thegn daemon stop` shuts down every verified listener and reports
  survivors. Tracker search uses bounded pages with partial-result reporting.
- Session moves mark active dispatches abandoned and discard source-local
  paths. Corner previews replay after occlusion clears.

### Security

- Bundle ingestion, Mermaid output, SSH control sockets and plugin startup
  validate private paths and bounded inputs before use.

## [0.1.0-alpha.24] — 2026-10-21

### Added

- `thegn doctor` reports requested, resolved and enforced sandbox isolation.
  Project configuration can use `repo_path` to select a checkout.

### Changed

- `thegn secret list` and audit report calendar tokens; `thegn secret migrate`
  moves literal tokens to private file references.

### Fixed

- Failed hunk previews say “diff unavailable”; corner previews no longer
  occlude themselves.

### Security

- Credential-free agents lose credential locator variables. Private plugin
  state directories, signed Nix cache paths and validated sandbox settings are
  enforced.

## [0.1.0-alpha.23] — 2026-10-21

### Added

- CI reruns use explicit failed-or-canceled scopes; `thegn doctor` probes the
  debug adapter. Linux users can run `install.sh --batteries` for a verified
  font and user-local Alacritty.

### Changed

- Calendar tokens resolve through the secret store; ICS parameters retain
  multiple values while rejecting conflicting singleton values.
- Merge status and worktree-close failures are reported explicitly. Media
  operations recheck player identity and reconcile failed loop changes.

### Security

- CI logs use a versioned redaction policy. Share credentials use a redacting
  secret type, and the VPS SSH control socket uses a private identity-based
  path.

## [0.1.0-alpha.22] — 2026-10-20

### Added

- The default sandbox profile is `standard`; `hardened` and `guarded` remain
  aliases. Repositories can opt into development shell injection with
  `thegn repo trust` (dev channel).

### Changed

- Snapshot uploads use server-side encryption by default;
  `[lifecycle.snapshot] server_side_encryption = false` opts out for
  incompatible S3 endpoints. Calendar sync is bounded; remote transfers are
  bounded (dev channel).

### Security

- Queue handoffs run without inherited credentials. Sealed profiles refuse
  unsupported hardening; provider launches refuse isolation they cannot
  enforce (dev channel). Containers and snapshots are verified by identity
  before reuse or cleanup.

## [0.1.0-alpha.21] — 2026-10-20

### Added

- Calendar transparent events appear as free and still trigger reminders.
  Media backends report typed errors, and calendar accounts fetch
  concurrently.

### Changed

- `thegn merge sweep` exits non-zero on refused or incomplete cleanup.
  `thegn wt reopen <path>` restores a worktree suppressed by close; doctor
  reports Git metadata exposure.
- Compose and dotfile updates use pinned or staged inputs and preserve prior
  state on failure.

### Security

- Daemon dispatch routes can require a capability. Note storage, plugin
  environments and secret files enforce bounds; provider connections enforce
  identity checks (dev channel).

## [0.1.0-alpha.20] — 2026-10-20

### Added

- Refusal messages include a remediation hint. `thegn merge retry` and
  queue-aware landing explain why a row is ineligible.

### Changed

- Compose selects a consistent replica and is recreated when its policy
  changes. Issue lookups retain the selected tracker account; remote GitLab
  calls are pinned to their host (dev channel).

### Security

- Merge operations refuse branch drift and histories that cannot be safely
  replayed. Provider resources, snapshots and shares are bound to verified
  identities (dev channel); secrets and sandbox launches use pinned tools and
  bounded output.

## [0.1.0-alpha.19] — 2026-10-20

### Added

- Remote worktree clones and projections are staged before publication (dev
  channel), and share shutdown stops the child process group under one bounded
  operation (dev channel).

### Changed

- Failed snapshots and parity captures report rollback or inconsistency.
  Worktree projections are staged and published only after successful sync
  (dev channel).

### Security

- OCI bind mounts are rechecked after container creation, and provider
  provisioning checks egress before execution (dev channel). Remote account
  homes, secrets and resources use private ownership records (dev channel).

## [0.1.0-alpha.18] — 2026-10-20

### Added

- `thegn wt reopen <path>` restores a worktree after close suppression; doctor
  reports sandbox access to shared Git metadata.

### Changed

- Merge updates reject unknown statuses and report partial cleanup. Strict
  worktree and reflog reads no longer skip malformed records; hydration marks
  unavailable reads clearly.
- Plugin list and check use the same admitted plugin set, while media results
  distinguish errors from empty lists.

### Security

- Sealed tunnels with VPN requests are refused until enforceable. Compose
  containers are inspected and torn down if they escape isolation.

## [0.1.0-alpha.17] — 2026-10-19

### Added

- `thegn merge retry` re-arms held rows; queue-aware landing accepts only a
  ready row for the checked-out branch. Calendar overrides replace or cancel
  generated occurrences.

### Changed

- Worktree close tears down SSH-placed sandboxes and reports failures
  (dev channel). The new-worktree wizard resolves repository context from its
  current model. `session move` validates input before changing sessions.

### Security

- S3 snapshot endpoints require HTTPS. Secret files use a no-follow directory
  handle, and provider credentials and SSH key rotation use checked,
  serialized updates (dev channel).

## [0.1.0-alpha.16] — 2026-10-19

### Added

- Calendar sync refreshes today's events and the visible month.

### Changed

- Pipeline completion reports a verdict and gate result; unknown worktree
  status blocks completion, resume and cleanup unless overridden. Plugin calls
  use an allowlisted environment and one deadline.

### Security

- Remote SSH keys, parity manifests and callback origins are validated before
  use. Failed machine inventory reads no longer permit resource creation.

## [0.1.0-alpha.15] — 2026-10-19

### Added

- The monitor adds Containers, network-owner details, thermal cooling and CPU
  core drill-down. Observe dashboards can label and hide targets (dev channel).

### Changed

- Memory, GPU and network monitor panels show denser histories. Extended
  keyboard input is forwarded only to opted-in panes.

### Security

- Sandbox garbage collection runs only when sandboxing is enabled. Quitting
  cancels in-app merge work, and cleanup refuses to act on hidden monitor
  rows.

## [0.1.0-alpha.14] — 2026-10-18

### Added

- Disk and Power monitor views show filesystem, worktree, rail and power-cap
  details. Processes adds an identity-checked signal menu with confirmation.

### Changed

- Memory adds kernel history; container discovery and plugin reads are
  bounded. GPU and monitor histories preserve missing readings as gaps.

### Security

- Share shutdown uses bounded, verified state (dev channel). Container cleanup
  and network-owner attribution use bounded, verified state.

## [0.1.0-alpha.13] — 2026-10-18

### Added

- Disk, Memory, Thermal, Network, Power and GPU monitor tabs show denser
  histories, Wi-Fi details, power rails and GPU engine activity.
- Processes adds `=` to adjust priority and Linux `I` for I/O priority; both
  require confirmation. Doctor explains denied monitor collectors.

### Changed

- Power monitoring works without a battery; container statistics are
  collected on demand. Reverse tunnels retry transient errors and enforce
  connection and lifetime limits (dev channel).

### Security

- Plugin contributions and retained data are bounded. One-shot plugin
  processes stop when their deadline expires.

## [0.1.0-alpha.12] — 2026-10-18

### Added

- GPU and Network are monitor tabs with engine, memory, interface and
  throughput histories. Containers gains bounded resource statistics; network
  monitoring adds TCP state and latency summaries.

### Changed

- Disk and Power monitor panels show device, filesystem, battery and power-cap
  histories on labeled scales.

### Security

- Plugin calls have bounded queues, concurrency limits and deadlines.
  Network-tunnel admission enforces resource limits (dev channel); container
  scans enforce resource limits.

## [0.1.0-alpha.11] — 2026-10-18

### Added

- The Processes panel includes task, thread, state and owner summaries.

### Changed

- Memory, Disk, GPU and Network views retain bounded histories. Linux
  monitoring avoids refreshing unused system information. Sandbox fallback
  clears its loading indicator and avoids repeated failed backend attempts.

### Security

- Plugin stderr is bounded, and daemon shutdown waits for session actors to
  stop within a deadline.

## [0.1.0-alpha.10] — 2026-10-18

### Added

- The monitor adds CPU and Memory views, a filterable Processes table and
  AMD GPU readings. The masthead can use NVML, with `nvidia-smi` fallback.
  Doctor reports UDisks2 SMART health when available.

### Changed

- Linux process monitoring uses a native `/proc` reader with a fallback.
  Process filters support owner, age, CPU, memory and PID terms.

### Security

- Resident plugin stderr is bounded.

## [0.1.0-alpha.9] — 2026-10-18

### Added

- CPU, Memory and Thermal monitor views are available, with process filtering
  and configurable table columns. Linux systems can report CPU topology,
  filesystems, GPU devices and power-cap readings.

### Changed

- Linux base monitoring reports hardware temperatures; GPU discovery
  recognizes integrated graphics and avoids duplicate devices. Adding a host
  reports database-write failures.

### Security

- Process priority changes are identity-bound and never escalate privileges.

## [0.1.0-alpha.8] — 2026-10-17

### Added

- Linux monitoring now includes CPU interrupts and scheduling, thermal zones,
  CPU topology, memory, network and process readings.

### Changed

- Sandbox startup and Podman event monitoring clean up child processes on
  fallback or shutdown. Worktree scans refresh only when fetched refs change;
  preview preparation uses background work.

### Fixed

- The sidebar retains its last known change counts when refresh fails.

## [0.1.0-alpha.7] — 2026-10-17

### Added

- Linux monitoring adds memory health, network traffic, disk activity,
  container inventory and battery or power-supply readings.

### Changed

- Process signals are tied to process start identity. Preview
  graphics and log drains use bounded background work.

## [0.1.0-alpha.6] — 2026-10-17

### Added

- Linux monitoring now reports CPU frequency, hardware sensors, cgroup
  resource use and supported AMD GPU metrics.

### Changed

- Calendar, weather and usage pollers reuse HTTP clients. GitHub requests back
  off after server errors, and worktree operations refuse to reopen a row
  while deletion is pending.

## [0.1.0-alpha.5] — 2026-10-16

### Added

- Plugin environment values can reference secrets through the token resolver.

### Changed

- Merge conflict reuse is isolated in thegn's state. Removing a worktree
  prunes its read notifications and issue links. Diff watching excludes
  pipeline metadata while keeping those files tracked.

## [0.1.0-alpha.4] — 2026-10-16

### Added

- GPU monitoring recognizes integrated graphics and avoids duplicate devices.

### Fixed

- The monitor corrects disk, CPU-frequency, memory and network readings,
  including duplicate mount and interface accounting. Podman container
  statistics support current field names and are sampled only when needed.

### Changed

- Internal thegn metadata is excluded from file watching and ignored by Git.
  Git filter operations use a managed long-lived process rather than starting
  a process for each filtered file.

### Security

- Process signals are checked against process identity before delivery, so a
  stale process listing cannot signal a different process that reused its ID.

## [0.1.0-alpha.3] — 2026-10-16

### Changed — issue trackers are stable-channel; config output redacts secrets

- **Linear, Jira and Kaneo issue trackers are now available on the stable
  release channel.** Their configuration is no longer stripped by the channel
  clamp, and `wt new --from-issue` now honours the repo `.thegn.toml`'s
  `[issues]` restrictions and pins (account filter, team/project) — a repo
  overlay can only narrow, never add accounts or tokens.
- **`thegn config get`, `config show` and `config explain` redact secrets.**
  Tracker, forge and model-proxy credentials (including arrays such as
  `api_keys`) print as `***redacted***` in text and `--json`; `*_env` names,
  `project_key`, `binary_cache_key` and numeric/bool values are left intact.

### Changed — ambiguous `[project.<key>]` blocks are refused, not first-matched

- **A trusted per-project block now applies only when it is unambiguous.** The
  key must be the normalized slug of the repo's directory name (`My_Repo` →
  `my-repo`). A differently spelled key, or two keys that normalize to the same
  slug, fails `thegn config validate`, and the block is **refused**: none of it
  applies, and that repo's merge/PR queue drains, CI autofix and autopilot stop
  (including the in-app Integrate, drain and PR-queue ticker). `[git]` is
  clamped to its strictest settings (submodules off, merge guard on, signing
  kept, background fetch off), and agent launches whose refused block pins
  accounts or an env bundle are refused. `thegn land`, `integrate`,
  `merge drain/add/…` and `pr-queue drain/add` stop with the reason;
  `merge/pr-queue list|rm|clear` still work. `config explain` and
  `repo trust` print the refusal.
- **Migration:** rename a non-normalized key to the slug the error names, and
  keep exactly one block per slug. A repo whose name has no slug (`___`,
  non-ASCII-only) no longer inherits `[project.repo]`, and a generated tab name
  (`repo`, `foo-2`) never selects the block that shares its spelling: the
  account / env-bundle block is chosen by the repository's own directory name.
  When two separate LIVE checkouts with the same name are registered (stale
  registrations, plain folders, linked worktrees, and every other spelling of
  the same checkout — symlink, bind mount, case-insensitive mount — don't count), that name's accounts / env bundle are refused and agent launches
  there stop with the reason; shells always launch. Rename or delete the other
  checkout to recover (binding blocks to the canonical repository identity is
  the next step). Landed-worktree sweeps and review-agent handoffs also stop
  under a refused block instead of using the global policy.

### Security — agent `permissions` no longer write into the worktree

- **`[[agents]]` / `[[pipeline.stages]]` `permissions` are now command-scoped.**
  A launch used to read-modify-write `<worktree>/.claude/settings.local.json`
  before sandbox preparation — following repository-controlled symlinks,
  non-atomically, dirtying the branch, persisting after the session, and
  launching anyway when the write failed. The grant now rides the launch
  command itself (claude: `--settings '{"permissions":{"allow":[…]}}'`, a
  documented per-session layer that merges with your settings files, keeps
  their deny rules and hooks, and writes nothing). Nothing under the worktree
  is touched. Behaviour change: the list is now _added to_ the worktree's own
  `permissions.allow` instead of replacing it, and a non-empty list on a
  harness with no command-scoped grant (codex, pi, aider) refuses the launch
  — a stage dispatch is held before its roster row exists, and `thegn dispatch
claim` refuses the slot — instead of being ignored with a warning. `thegn
config validate` and `thegn doctor` report such entries.
- **Upgrading: review the files the old seeder left behind.** Every worktree a
  permissioned launch ever touched still holds a thegn-written
  `permissions.allow` in `.claude/settings.local.json`. thegn no longer writes
  or manages that file, but claude still reads it, and it now ADDS to the
  command-scoped grant: narrowing an entry or stage (say `["Bash"]` →
  `["Read"]`) does not take effect while the stale file still grants `Bash`.
  Review those files and delete the entries you did not put there — thegn will
  never write them again. `git status` will show the file in worktrees where it
  is tracked or where the seeder created it.
- **A branch's own committed allow-list now survives.** The old seeder
  overwrote `permissions.allow` on every launch, which incidentally clobbered
  any list a checked-out branch had committed. With merge semantics a
  repository-authored allow-list is layered under thegn's grant and reaches a
  headless worker. Command-scoping removes thegn's write primitive; it does not
  contain what a candidate branch's own harness settings claim (that is the
  mutable-project-harness work).

### Changed — worktree paths, tabs and program features are collision-proof

- **New worktrees get a collision-resistant directory.** A checkout is now
  `<worktrees_dir>/<repo>/<label>--<digest>` (or `<repo>/.worktrees/…` under
  `worktree_mode = "in_repo"`), where the digest covers the repository's exact
  Git common directory and the exact branch name. `feat/a`, `feat-a` and
  `feat_a` used to slug to ONE directory and one tab, and two repositories
  sharing a basename shared a parent. **Existing worktrees are never moved** —
  they stay exactly where Git has them registered; only newly created (or
  renamed) checkouts use the new shape.
- **A tab is `<repo>/<exact branch>`** instead of `<repo>/<slugged branch>`,
  so two branches can no longer land on one tab key. Existing rows keep their
  recorded names. A branch literally named `home` renders as `home~` to avoid
  the repo's home tab.
- **A worktree registered under a tab that another worktree also claims is
  quarantined**: both are left untouched on disk, neither is routed by that
  tab, and the contested paths are reported at launch and by
  `thegn doctor` (section "Worktree identity"). Rename one branch or remove one
  checkout to clear it.
- **BREAKING (CLI): `thegn wt new <name> --program <p>` no longer slugs the
  feature name.** Program membership is exact branch equality, and slugging
  silently federated `payments/retry`, `payments-retry` and `Payments-Retry`
  into one cross-repo feature. A name that is not already a valid Git branch
  name (e.g. `"Fix login bug"`) is now refused with the literal to use
  (`fix-login-bug`); re-running with that literal attaches to the branches an
  older thegn created under the same slug, so existing features keep working.
- **Creation refuses a destination that already exists** instead of adopting an
  empty directory, and says how to clear it. A rename whose `git worktree move`
  fails rolls the branch rename back; if that rollback also fails, the split is
  reported with a copy-pasteable recovery command instead of being reported as
  success.

### Fixed — a project no longer resumes in a terminal

- **Switching back to a project landed you in a terminal instead of your work.**
  A terminal group shares `session.worktrees` with the worktrees, so whichever
  one you were standing in when you left a project became that project's
  remembered focus — and every later visit "magically jumped to the terminals".
  But a terminal belongs to a global region: the `terminals` registry is not
  scoped to a repo, and the group migrates between sessions when you open it
  from another project. A workspace now parks and resumes on one of its
  **worktrees**, whichever door the switch came through (`Shift-Alt-↑↓`,
  `Ctrl-1-9`, a sidebar row, the palette, `thegn open`). Leaving a project from
  the terminals region steps back onto the worktree you were last in first, so
  the return trip lands exactly where you were. The cold path is covered too: a
  workspace whose persisted layout named a terminal as active resurrects on a
  worktree.
- **Every way into the terminals region now bookmarks the worktree you left.**
  The bookmark was written only by the `toggle-region` chord, so entering the
  terminals any other way — the sidebar row's `↵`/click, the ring's terminal
  stop, and above all the new-terminal wizard — left it unset, and toggling back
  out dropped you on the home worktree rather than the one you had come from.
  It is now written at the region crossing, by the one door every activation
  gesture already goes through.
- **The bookmark is a group name, not an index.** Two things re-index a
  session's groups underneath it — a workspace switch, and a terminal migrating
  out to another project — after which the old index silently named a
  _different_ worktree. A name either resolves or falls back to the home
  worktree, so a stale bookmark degrades instead of lying.

### Changed — the sidebar names its sort, and holds it while you navigate

- **The active sort mode is now on screen.** It is the quiet word at the right
  of the `WORKSPACES` header, and clicking it opens the sort menu. Previously
  the mode lived only inside the `s` menu, so a `live` sort quietly reordering
  rows read as the sidebar misbehaving rather than as a mode doing its job.
  The chip degrades by dropping rather than clipping — a narrow column loses
  the qualifier, then the whole chip — and yields the header row to the `/`
  filter. The NAVIGATE footer's `s` row now reads `sort: <mode>` too. (The
  one-line statusbar strip is deliberately unchanged; it elides under width
  pressure, and a 15-cell hint there would evict an essential one.)
- **The `attention` and `live` sorts no longer re-rank under the cursor.**
  Both order by a signal that changes on its own — tier ranks and last-active
  time, recomputed off-loop every few seconds — so a row you were walking
  towards could move as you reached it. The ordering keys are now held for as
  long as the sidebar has focus (the chip appends `hold`), and for ~2s after an
  `Alt-↑↓` / `Alt-1-9` jump made from a pane, where the sidebar never takes
  focus. The order re-settles the moment focus moves into a pane. A worktree
  created while the order is held still sorts to its real position rather than
  being exiled to the bottom, and project blocks are held too under `[ui]
sidebar_project_sort = "attention"`. Expiry is evaluated lazily on a frame
  that was happening anyway — no timer, no wake source, nothing at idle. New
  `[ui] sidebar_freeze_sort` (default `true`); a no-op under manual/name/recent,
  which never move on their own.

### Fixed — `just smoke` is green again (and actually hermetic)

- `test/smoke.sh` now clears `THEGN_DATABASE_MIGRATION_EXECUTABLE` and
  `THEGN_DATABASE_MIGRATION_AUTHORITY`. Both are ambient in any shell started
  from inside a live thegn, and inheriting them pinned migration authority to a
  _different_ binary than the one under test — every schema advance in a
  database the script owns outright was refused. The v5→v6 layout check now
  also opts into `migration_authority = any` for its one `thegn list` call,
  since `controller` (the default) forbids ordinary CLIs from advancing a
  schema at all.
- Two lifecycle-hook checks had been failing since branch prefixes existed:
  both compared `$THEGN_BRANCH` against a bare worktree name, but hooks
  correctly receive the real branch (`tg/smoke-cli`). The pre-create check now
  pins the hook's value to what git reports rather than hardcoding a prefix,
  and the pre-destroy veto matches on suffix — so the refusal that check exists
  to prove is actually exercised now.
- `config explain` has named the layer `project` since the workspace→project
  rename; the assertion still grepped for `workspace`.
- A failed `session move` left no target database, and the unguarded `sqlite3`
  read of it aborted the whole suite under `set -e` — reporting a bare "unable
  to open database file" and skipping every remaining check. Those reads now
  fall back to a sentinel so the failure surfaces in its own check.

### Fixed — `manual` sidebar order is actually stable

- A worktree no longer changes place under `manual` sort when it is loaded or
  unloaded. Manual order keyed on a tie-break that conflated two independent
  sequences — live groups numbered by their session slot, registry-reconstructed
  rows numbered _after_ all of them — so a worktree crossing between the two
  re-sorted against its siblings even though its persisted position never moved.
  Activating a dormant row appends it to the session and would bump it up the
  list permanently; a failed create demoted one to the tail; and the same repo
  could read in a different order depending on whether you reached it by a warm
  switch or a cold restore. Manual now ranks on the registry itself, so the tree
  reads the same live, dormant, warm and cold. Worktrees with no registry row
  keep their session slot and sort first, matching how a session resurrects.

### Changed — the sidebar tree reads tighter

- Adjacent projects are now separated by an **alternating background tint**
  rather than a blank separator row. Each project block — its header, its
  worktrees, its folders — shares one tint, and the next project takes the
  other; project headers keep their recessed band on both. `[ui]
sidebar_dividers` still gates the separation (off = one uniform tint), but it
  no longer costs a screen row per project, and it now applies in the rail and
  under a `/` filter too. The new `[theme.colors] panel_alt` slot is derived
  from each theme's own `bg0`/`panel` and can be overridden.
- **Pipelines only ever nest under a project.** The top-level `Pipelines`
  group that collected unattributable lanes is gone; a lane whose worktree is
  not registered anywhere is now filed by the directory its siblings live in,
  and one that still matches no project is left out of the tree rather than
  opening a second, root-level pipeline section. The flat layout, which has no
  project rows to nest under, grows no pipeline rows.
- The compact `Pipeline ▸ N running` roster row at the bottom of the tree has
  been removed. `Alt b` opens the pipeline board from anywhere, as before, and
  the board carries the counts.

### Changed — projects are the one-repo product concept

- One-repo sidebar, picker, navigation, and help surfaces now say
  **project**. The stable action ids are `new-project`, `delete-project`,
  `switch-project`, `next-project`, `prev-project`, and `summon-project`;
  the former `*-workspace` ids remain accepted as compatibility aliases.
- The multi-repo grouping commands are now `thegn program …` and
  `wt new --program`. `thegn project …` and `wt new --project` remain
  behavior-identical deprecated aliases for three stable releases and warn
  with their replacements. The `projects_dir` config key and `THEGN_PROJECTS_DIR`
  environment variable are canonical; their old spellings remain accepted
  with the same bounded warning window.

### Changed — merge queue lives in scoped surfaces

- The default bottom bar no longer emits a merge-queue badge. Add `"mq"` to
  `[bars] bottom_right` to opt into its compact scoped summary; activating it
  opens the existing unified merge-queue detail.
- Full-mode project headers now show the highest active queue tier and count
  before the warm-pool token, with the right-panel Work ▸ Merge queue section
  as the activation destination. Empty and landed-only queues stay quiet.
- E2E baselines were not re-recorded for this change.

### Changed — the sidebar reads in tiers, and repos no longer run together

- **A project header now looks like a project header.** Every structural
  row in the sidebar used to get the identical bold-text-on-a-band treatment,
  so several open repos stacked into one undifferentiated column.
  Project and terminal-host headers are now the loudest tier — bold, in the
  accent color, marked `◆` (or `⌂` / the host glyph) and the only rows on a
  recessed band. Folder headers drop the bold and the band, and their filed
  count is grayed: a drawer inside the repo, not a repo itself. The ladder
  survives mono/16-color quantization — weight, glyph and indent carry it,
  not color alone.
- **A blank separator row now sits above each project header**, so adjacent
  repos read as separate blocks. Opt out with `[ui] sidebar_dividers = false`
  (the old dense layout, byte-identical). Separators are suppressed in the
  rail and while a `/` filter is active; scrolling and the "N more" chips
  count them, and clicking one selects the header below (never folds).
- **e2e baselines have NOT been re-recorded in this change** — every muse
  snapshot showing the sidebar (`sidebar__*`, `chrome_regions__chrome`,
  `responsive_breakpoints__layout`, `panel_*`, `themes__*`, `glitch_hunt_*`)
  moves; re-record in a deliberate later pass (`just e2e-update`).

### Added — the pipeline mechanism: one dispatch, verified waits, a gated done

- **`thegn session open --stage <name> --issue <id>` performs a stage
  dispatch atomically**: insert the roster row, render the stage's prompt
  template (`{issue_*}`, `{branch}`, `{worktree}`, `{stage}`, `{artifact}`,
  `{parent_artifact}`), refuse an empty render, open the session headless
  and stamp the row with its session id and artifact path — the Lead keeps
  the judgment, thegn performs the mechanism. An explicit `--prompt` is
  refused (the template owns the task); `--stage` without `--issue` stays
  the overlay open described above. A dispatch that fails after the insert
  leaves the row `failed`, never a `queued` row the Lead re-drives forever.
- **Artifact paths are thegn's convention**:
  `.thegn/pipeline/<ISSUE>/<stage>/<row>.md`, sanitized against traversal
  (`[A-Za-z0-9._-]`, everything else collapsed to `-`) — per-row, so
  parallel coders of one stage cannot collide, and committed on the branch
  so git stays the source of truth.
- **`thegn dispatch verify <id>`** checks a finished row's artifact — exists
  under the worktree, tracked by git (`git ls-files --error-unmatch`) —
  prints one fact line plus the reasons, exits 2 when not done. Dirty
  (uncommitted sibling work) is reported, never blocking.
- **`thegn dispatch wait [--row <id>] [--any] [--timeout ms]`** blocks until
  a row's session exits: first wake wins with `--any` (the default), the
  losers are cancelled, timeout exits 2 with `{"matched":false}`, a dead
  session answers immediately from the tombstone, and an unknown one wakes
  as gone rather than hanging.
- **`thegn dispatch set-status <id> done` is gated**: a row with an artifact
  must have it present and tracked or the write is refused with the reasons
  and a pointer to `dispatch verify` — `--force` is the deliberate override.
  Artifact-less rows (plain dispatches) pass by construction, and every
  other status behaves exactly as before.
- **`thegn session close <id>`** — terminate a session's PTY child; the
  daemon's tombstone keeps `session list`/`session wait` truthful about how
  it ended.
- **`session list` carries a liveness token in its second column** — `live`,
  `exited(0)`, `exited(7)`, `exited(?)` when unreapable, suffixed with the
  final state word — so a supervisor greps a fixed column; `--live` filters
  to non-exited rows before serialization.

### Added — the pipeline board is its own surface, and the sidebar files pipelines under their project

- **The agent-dispatch roster gets a dedicated board** (`Alt b`, or **Pipeline
  board** from the command palette — the old monitor tab is gone): stages laid
  out left to right in `[[pipeline.stages]]` declaration order, the `next`
  arrow chain drawn as the org chart, every configured stage always a column
  even with no rows in it, parent→child fan-out drawn with tree connectors,
  and a stack-to-one-column fallback below 22 cells per stage that is a
  tested pure decision, not an accident of terminal size. `Space` freezes the
  view (never the pipeline), `x` hides finished rows, and `↵` opens a row's
  worktree even when it is not open as a tab. The board re-reads the roster
  only while it is open and unfrozen; nothing here advances, starts or stops
  a stage.
- **The sidebar files pipeline worktrees under their project by default**:
  inside every project a pipeline has spawned worktrees in, a derived
  `Pipelines` folder holds one folder per pipeline (named from the roster's
  issue id) with every worktree that pipeline's roster rows reference inside
  it. The folders come from the roster's rows of any status, so they survive
  a restart and a finished lane stays until its rows go; a worktree no
  roster row references stays exactly where it was.
- **Legacy dispatch ages fixed**: rows written while the roster stored
  seconds instead of milliseconds rendered as ~20 671 days old. The database
  migrates them in place at schema v58, every read normalizes through one
  guard, and the queued/spawning/running status glyphs are distinct and ride
  the terminal's glyph ladder (ASCII terminals included).

### Added — a harness, a model and an account per agent, per stage

- **`[[agents]]` / `[[tools]]` entries take `model`, `env` and `permissions`**
  (and `harness`, the launch-shaping harness id, which precedes `provider`). The model is appended through the
  harness's own flag at every launch (`claude --model X`, `codex -m X`,
  `pi --model provider/id`, `aider --model X`) — a model on a harness with no
  model flag is a `thegn config validate` error, never a silent default tier.
  `env` is a per-entry overlay applied last (with the `env:`/`file:` secret
  expansion presets use), so one entry can be pinned to an account
  (`CLAUDE_CONFIG_DIR`) or a pi home; `permissions` is the headless tool
  allow-list, seeded into the worktree's harness settings at launch instead of
  hand-written per worktree.
- **`[[pipeline.stages]]` entries override `harness` / `model` / `env` /
  `permissions` per stage** (a stage is a generic role; the chart mixes
  harnesses and tiers per stage), and `thegn session open --stage <name>` (also the control API's
  `AgentLaunch.stage`) layers them over the agent — a cheap tier for coders and
  a strong one for reviewers from the same entry.
- **`pi` is a first-class harness** (`pi -p <prompt>` headless, `--model
provider/id`), so a stage can run on the local model proxy's tiers.
- **The daemon reads its config per agent launch.** A `[[agents]]` entry added
  or retuned while the daemon runs is honoured by the next `session open`; the
  startup snapshot is only the fallback when the file no longer loads. Until
  now every registry change meant restarting the daemon — and every pane it
  owned.
- `thegn doctor` lists each entry's effective harness, model, env keys and
  permission count (`doctor --json` → `agents`); **`thegn agent list`** is the
  compact, agent-readable form of the same view (entries + stages, one line
  each, `--json` for `{agents, stages}`), and **`thegn dispatch list --active`**
  keeps only the roster rows that occupy a slot.
- **The `/pipeline` and `/supervise` skills are bundled and seeded** into every
  worktree's `.claude/skills/` the way `/mq` already was — `/pipeline` once a
  `[[pipeline.stages]]` chart is configured, `/supervise` always — so an agent
  in any project thegn opens can run a chart without hand-installing anything.
  Existing worktrees pick them up at the next launch.

### Fixed — a raised hand is live state, not an inbox entry

- **The inbox no longer fills with "Claude is waiting for your input".** An
  agent's `OSC 9` / `OSC 777` attention signal used to append a notification row,
  and coding CLIs emit one at the end of _every_ turn — so the log gained a row
  per turn per agent, forever, and buried everything else. The signal is now live
  per-session state: the sidebar dot, the `✋` chip and the needs-you ring light
  up exactly as before (same tier, same reason, same surfaces), and the row is
  gone.
- **Answering the agent now clears the demand.** Typing into the pane always
  cleared the in-memory signal, but the notification row stayed unread — so the
  worktree scored blocked until you marked it read by hand, against this
  capability's own "resume clears the signal" contract. The hand is lowered when
  your input reaches the process, when the session ends, when the worktree is
  removed, and when you acknowledge or clear the worktree's needs-you signal.
- **Upgrading retires the backlog once.** The first open of an existing database
  marks the accrued unread `agent_attention` rows read, so the inbox and the
  `⚑`/`✋` counts start clean instead of carrying months of them. Deliberate
  pushes (`thegn notify push --urgency alert`, the control API, MCP) are
  untouched: those are real events and still get real rows.
- **Set `[notifications] agent_attention_inbox = true`** (env
  `THEGN_NOTIFICATIONS_AGENT_ATTENTION_INBOX`) if you want the audit trail back.
  It records one _current_ row per session, replaced each turn, rather than one
  row per signal.

### Fixed — "clear all" clears everything the inbox shows

- **Pressing `a` now clears every notification the list displays**, including
  rows tagged to the repo's own main checkout. The inbox's display filter is
  deliberately fail-open — a row tagged with a worktree path the registry does
  not know (the main checkout, which never gets a registry row; an
  externally-created or renamed worktree) is shown rather than hidden — but the
  clear was fail-closed and covered only untagged rows plus this repo's
  registered worktrees. Exactly that set was displayed forever and could never be
  retired: `a` greyed the rows, the next refresh re-read them, and they came
  back. Display and clear now project one shared predicate, so they cannot drift
  apart again. A row belonging to another repo's known worktree is still neither
  shown nor cleared.

### Fixed — configured credential homes are the whole story

- **A harness with an explicitly configured `[[usage.accounts]] dir` no longer
  also gets its default home scanned.** Usage rows, the token rollup and session
  discovery used to append the ambient home (`$CLAUDE_CONFIG_DIR`, else
  `~/.claude`) on top of whatever you configured, so a machine's real credential
  home was read even when the config never mentioned it — double-counting
  sessions and tokens wherever the two homes overlap. An explicit `dir` is you
  saying where that harness lives; the default home stays the zero-config
  fallback for harnesses you left unconfigured, and an `enabled = false` entry
  (an opt-out of one home, not a statement about the harness) still leaves it in
  place.

### Fixed — `thegn land` files into Merged instead of un-filing

- **A successful `thegn land` now moves the worktree into the _Merged_ folder**,
  the same place a queue land reaches, instead of un-filing it back to the
  ungrouped repo root. The one-shot land had been dodging the merge-lifecycle's
  destructive `remove`/`detach` arms by emitting a plain dequeue — which was
  over-broad: under the default `move`/`expire` those arms are just a folder
  move, not a removal, so a land left the worktree ungrouped. It now files into
  _Merged_ and, under the destructive arms, still files rather than deleting the
  worktree it was scripted from. `on_landed = "off"` is unchanged (it still
  clears a stranded _Merging_ membership), and because `thegn land` writes no
  queue row the filed worktree is never expiry-swept.

### Fixed — activity dots that mean what they say

- **A dot no longer turns red while the agent is still working.** Arming the
  "needs you" state now takes two consecutive quiet observations plus the grace.
  The old rule compared idleness against a grace that happened to equal the poll
  cadence, so it was already at the threshold on the very next poll: a single
  quiet window flipped the dot and the grace damped nothing. An agent thinking at
  ~0% CPU tripped it constantly.
- **A bare terminal never goes red.** Red means "an agent needs you", so a
  worktree with no agent shows white while it genuinely burns CPU and then
  returns to no dot. Previously any CPU under the worktree path cleared the
  ~3%-of-a-core threshold — `git status`, `direnv`, an LSP, shell
  autosuggestions — and latched a permanent alert on a plain shell. A red dot
  inherited by such a worktree now heals itself. The same `has_agent` gate has
  always governed the statusbar's needs-you chip; only the dot lacked it.
- **CPU is measured per process instead of as a running sum.** Summing the live
  set lied in both directions: a newly-appeared process brought its whole
  accumulated lifetime along as one delta (which is why running `ls` armed a
  dot), and a busy child exiting made the sum drop to a saturating zero — a false
  idle window in the middle of real work.
- **An agent started by hand is recognized.** The pane test read the _spawn_
  argv, so `claude` typed at a shell prompt reported `zsh`, contributed no output
  signal, and left CPU alone to judge an agent that is near-idle while waiting on
  a model. A live foreground probe now identifies it — descending through
  sandbox/remote wrappers — and the filter is positive (a recognized agent CLI),
  so `htop`, `watch` or a dev-server spinner can no longer masquerade as one.
- **Solicited repaints stop reading as agent output.** A resize SIGWINCHes every
  pane and full-screen programs redraw; a daemon reattach replays scrollback.
  Both arrived through the same path as live output, so one sidebar toggle marked
  every agent-bearing worktree busy.
- **"Finished" and "blocked on you" are now different dots** — amber and red
  respectively, where both used to be the same red. Which one shows comes from
  the worktree's attention tier, so the loud state is reserved for real evidence
  (an agent asking for input, a queue needing a human). Seen-versus-unread stays
  the filled/hollow distinction, so the two axes read independently.
- **New `[activity]` config section** exposes every threshold (busy percentage,
  quiet/resume graces, suppression windows, the agent gate, recognized agent
  program names), with defaults preserving the documented behaviour — and makes
  the "configured cooldown" the Windows spec already promised actually exist.
  New `[theme.colors] activity_done` colours the finished dot.
- **The dots are documented.** They had five states and two colours and no
  user-facing explanation anywhere; `docs/help/sidebar.md` now carries a legend.

### Added — merged worktrees get a grace period instead of vanishing

- **`on_landed = "expire"` is the new default.** A branch that lands keeps its
  worktree, filed into `merged_folder`, and is swept away (with its branch) once
  `merged_ttl_secs` — 7 days — has elapsed. The old default, `"remove"`, deleted
  both the instant the branch landed.
- **Why a grace period at all:** the two halves of a land are not equally
  recoverable. A deleted branch ref is the merge commit's second parent, one
  command away. The worktree **directory** holds gitignored state — `target/`,
  `.direnv`, env files — that exists nowhere else, and a wrong land destroyed it
  with no undo. A week is how long you now get to notice.
- **`thegn merge sweep`** collects everything past its grace period on demand,
  `--force` clears the lot early, and the `sweep-merged` action does the same
  from the palette (no default chord, matching `integrate` / `merge-drain`). The
  sweep also runs at startup and after each land — **no timer**, so an entry that
  comes due while thegn is closed is collected at the next launch and the idle
  loop still never polls.
- **A merged worktree you have gone back to and edited is never swept**, forced
  or not: dirtiness is re-read at collection time, not at landing, so resuming
  work in one cancels its collection.
- **The merge section counts the grace period down** — a landed row's right
  column reads `✓ 6d`, then `✓ due` once it is waiting on the next sweep. An
  expiry you cannot see is one you cannot act on before it fires.
- **The section's `c` now sweeps as well as clears.** It used to drop the landed
  rows only; under `expire` a landed row _is_ the grace-period clock, so removing
  it alone would strand its worktree in `merged_folder` with nothing left to
  collect it. The hint reads `sweep ✓`.
- The expiry arithmetic is pure (`thegn_core::merge_sweep`) and tested, including
  the two cases that decide whether a directory survives: `merged_ttl_secs = 0`
  means "never expire" rather than "expire everything", and a `landed_at` in the
  future (a backwards clock step) is never due, so an NTP correction cannot
  collect every merged worktree at once.

### Fixed — `thegn integrate` folds what you queued, not every branch you own

- **`thegn integrate` ignored the merge queue entirely.** Every description of
  it — the CLI help ("Drain the local merge queue"), `docs/help/cli.md` ("drains
  the queue once"), the merge-queue help page — promised a queue-consuming
  operation. `candidate_branches` never read the queue: it enumerated `git
worktree list` and folded every _eligible_ branch, where eligible means only
  "clean, and not already on the target". That test cannot distinguish finished
  work from a branch still being built, so a single `thegn integrate` (or the
  one-keypress in-app action) could land branches nobody had nominated — and with
  the default `on_landed = "remove"`, delete their worktrees afterwards, taking
  gitignored local state (`target/`, `.direnv`, env files) with them. Work in
  progress was protected only by the accident of being dirty.
- **`[merge_queue] require_enqueue` (default on) makes the queue an input.** Only
  branches added with `thegn merge add` are folded. `thegn integrate --all`
  restores the fold-everything behavior for one run; setting the key to `false`
  restores it permanently. The guard lives in `fold_active_repo`, so the in-app
  action is covered too, and it fails closed — a queue that cannot be read folds
  nothing rather than everything.
- **The queue recorded what was _considered_, not what was nominated.**
  `persist` enqueued every candidate, so a fold wrote `queued` rows for
  bystander worktrees and filed them into the `queued_folder` ("Merging") in the
  sidebar — a command users reach for as a drain silently reorganizing their
  tree. It now records only branches that actually landed or deferred, which is
  also what lets `require_enqueue` tell a human's `merge add` from the previous
  run's own bookkeeping.
- **`integrate` prints its plan and confirms.** It names each branch before
  folding any of them; `--dry-run` prints that plan and changes nothing, and
  `--yes` skips the prompt — required now to fold non-interactively, rather than
  letting an absent prompt auto-answer itself.

### Fixed — the merge guard no longer fights the pre-commit framework

- **`git merge` in the canonical checkout failed with a hook-plumbing error.**
  The in-sandbox merge guard installed itself into the `pre-merge-commit` slot
  and displaced whatever was there to `pre-merge-commit.thegn-orig`, chaining to
  it on the allow path. When the displaced hook was a prek/pre-commit **shim**,
  that chain was poison: a shim invoked while it is not the installed hook
  reports `prek's Git shim is installed in migration mode` and exits non-zero, so
  _every_ merge failed — sandboxed or not. It also flip-flopped, because the
  framework would reclaim the slot and thegn's next startup would displace it
  again, so the breakage returned on its own after any fix.
- **The guard now installs _beside_ a framework shim instead of over it.** Both
  prek and Python pre-commit run `<hook>.legacy` at runtime, so when a shim owns
  the slot the guard is written to `pre-merge-commit.legacy` and the shim keeps
  its slot. Both gate the merge, neither displaces the other, and there is
  nothing left to flip-flop over. With no framework present the guard takes the
  slot exactly as before, still chaining a genuine user hook to `.thegn-orig`.
- **Existing broken checkouts repair themselves.** A checkout left in the old
  shape (our hook in the slot, a shim parked in `.thegn-orig`) is detected on the
  next launch: the shim is moved back into its slot and the guard reinstalls
  alongside it, so the poisoned chain does not survive the upgrade.
- `merge_guard::install` now returns a `Plan` (`restore_shim`, `placement`,
  `action`) and the startup log records all three — a framework in play was
  otherwise invisible until a merge failed.

### Fixed — sidebar drag-drop lands where you aim, and can reach the end

- **A drop now puts the dragged row in the slot of the row you release on**, and
  the row you dropped on moves aside. Dropping on the **last** row of a run lands
  at the end. The old rule decided "before or after" by asking whether the
  pointer was in the top or bottom half of the hovered row — but a sidebar row is
  often exactly **one terminal cell** tall, and a one-cell row has no bottom half,
  so the test was unconditionally true. Every such drop landed one slot above the
  aim point, and the "append to the end of the run" path had no way to be reached
  at all. Rows are one cell whenever the sidebar is unfocused (which is where a
  drag starts if you were typing in a pane), under `sidebar_focus_detail =
"cursor"`/`"off"`, for a worktree with no detail line, for the row clipped by
  the bottom of the window — and **always** for project and folder headers, so
  those two could never be moved to the last position by anyone.
  - **Behaviour change:** if you had learned to aim one slot low to compensate,
    you will now overshoot the other way.
  - The tail of a run you are not already in is reached through that run's
    header, as before: drop on a folder header to file at its end, or on the
    project header to unfile at the end of the loose list.
- **Rows no longer move under a held pointer.** Pressing a row can change sidebar
  focus, which grows or shrinks every worktree row by a line (the focused detail
  tier) — so a perfectly still pointer used to resolve to a different row on the
  first drag sample than the one it was pressed on. Row heights are now frozen
  for the life of a gesture.
- **A drag can no longer get stuck.** If the pointer crossed a pane whose
  application requested mouse reporting, that pane swallowed the button release,
  so the gesture never ended and the next left-drag anywhere in the app was
  hijacked back into the sidebar. A live drag now captures the pointer, and
  **`Esc` abandons a drag** without moving anything.
- **Edge autoscroll keeps up.** It advanced exactly one row per motion sample,
  while a fast drag's samples are coalesced down to the last one — so flicking to
  the edge scrolled a single row. It is now proportional to how far past the edge
  the pointer is (and capped), and it writes the scroll position back.
- **The insertion rule paints where the drop will land.** For folder and
  project drags it was drawn under the _header_ while the drop landed after
  that header's whole subtree.
- **A project drop is atomic.** It used to step-swap its way to the target, one
  rebuild and one database write per step, and could bail out half way on a
  pinned neighbour — leaving the project parked between where it started and
  where you aimed. Every drop is now one resolved order, applied once; a refused
  drop changes nothing.

### Added — per-account AI usage tracking

- **The AI-account usage tracker now tracks every configured account, not one
  per harness.** Codex and Claude Code both locate their entire credential home
  from a single environment variable (`CODEX_HOME` / `CLAUDE_CONFIG_DIR`), which
  is how a machine ends up with several logins side by side — thegn's own
  `[[accounts]]` switcher works that way, and Claude Code profiles park theirs
  under `~/.claude-profiles/`. The gather previously read only the one home the
  variable currently pointed at, so a machine with eight logins showed one.
  Homes are now discovered (default home + a scan of `[usage] profile_roots` +
  `[[accounts]]` + the new `[[usage.accounts]]`), read independently, and
  identified from each home's own `.claude.json`, so rows are labelled
  `you@example.com (Your Org)` rather than eight identical "Claude"s. Two paths
  to the same login collapse into one row.
- **A statusbar gauge, a keybind, and a panel section.** `◔ 87% 2h14m` shows the
  most-consumed window across all accounts (`[usage] statusbar = false` hides
  it); `Alt-u` opens the overlay, which previously had no default chord; and
  **System ▸ Usage** is the docked version, widening from one row per account to
  full per-account identity (org, seat, rate-limit tier, credential home) and
  the provider-stated window length. `r` re-gathers.
- **`[usage.alerts]` warns as a window approaches its limit** — a toast and, by
  default, a notification-inbox entry, with the same
  sustain/repeat/`clear_margin`/`notify_clear` semantics `[stats.alerts]` uses.
  Thresholds inherit `[usage] warn_percent` / `crit_percent` unless overridden,
  so the lines you are warned at cannot drift from the colours you are looking
  at.
- **History and forecast (roadmap V 300's unfinished half).** Each poll's
  windows are recorded to `usage_samples` (schema v53, pruned to `[usage]
history_days`), giving a trend sparkline and a projected "full in …". The
  forecast is deliberately conservative: it is drawn only from the run since the
  last window reset, needs at least five minutes of span, and stays silent when
  the window resets before the projection lands.
- **Host-wide transcript token rollups** (`[usage] token_rollups`), bucketed by
  day, model, and project. **These are labelled host-wide and are never filed
  under an account**, because they cannot honestly be attributed to one:
  transcript records carry no account field, and profiles routinely share a
  single `projects/` directory. The scan dedupes streaming re-emits by request
  id, never sums `usage.iterations[]` (both of which otherwise inflate totals),
  and reports how many files it had to skip rather than presenting a truncated
  count as a total.
- **`[usage] allow_network` now defaults to `true`.** Claude publishes no window
  state to disk, so with the fetch off the feature had nothing to show for a
  Claude account. It remains one lightweight authenticated request per account
  per poll, using the OAuth token already on disk; `false` restores the fully
  offline behaviour (Codex only). Polling moved onto the background ticker at
  `[usage] poll_interval_secs` (default 300, floored at 60), off the event loop,
  and a poll returning unchanged numbers repaints nothing.

### Changed — one dev shell (devenv removed)

- **`devenv.nix`, `devenv.yaml` and `devenv.lock` are gone; `flake.nix`'s
  `devShells.default` is the only development environment.** The git hooks moved
  into the flake via a `git-hooks.nix` input — the same upstream project devenv
  was wrapping, locked to the same revision (`43b3c1ab`), still driven by `prek`
  and still tiered the same way (pre-commit: treefmt/shellcheck/yamllint;
  pre-push: clippy/`just test`/`just smoke`). The generated
  `.pre-commit-config.yaml` is byte-identical modulo store hashes. Edit
  `flake.nix` and re-enter `nix develop` to regenerate it.
- **`.envrc` is now a single `use flake ".#${THEGN_DEVSHELL:-default}"`.** It
  used to prefer devenv on the host, which meant `direnv allow` dropped you in a
  _different_ shell from the one CI gates with (`nix develop --command just
<gate>`). Two consequences, both fixed by this change:
  - **The formatter that gated your commit was not the one that formatted your
    code.** devenv carried its own nixpkgs lock, drifted ~6 weeks from
    `flake.lock`, so the pre-commit `treefmt` hook ran **rustfmt 1.97.1** while
    `just fmt` / `nix fmt` ran **rustfmt 1.96.1**. Both now resolve the same
    store path, because the hook takes its formatters from the same
    `fmtPackages` list the `nix fmt` wrapper does.
  - **`just coverage` and `just check-cross` could not run in the default
    shell.** devenv's `languages.rust` was the plain nixpkgs toolchain — no
    `llvm-tools-preview` (`failed to find llvm-tools-preview`) and no cross
    `rust-std` (`can't find crate for 'core'`). The flake toolchain has both, so
    the documented "run `just ci` from `nix develop`, not devenv" caveat is
    deleted rather than restated.
- **`rust-src` added to the flake toolchain.** It is not in rust-overlay's
  `default` profile; devenv's rust module had been supplying it via
  `RUST_SRC_PATH`, so without this rust-analyzer would lose stdlib sources.
- Dropped with devenv, deliberately: its clang/lld host-linker wrapper (the
  flake links gcc + mold), bare `pkgs.treefmt` (the flake's wrapper resolves
  `treefmt.toml` from the repo root), an unpinned `yazi` (the flake pins one and
  exports `THEGN_YAZI_BIN`), and `clang`/`gdb`/`valgrind`/`make`. The first
  build after switching recompiles from cold — sccache keys on the compiler.

### Fixed — macOS, audited on the machine instead of from the code

Two on-device passes on Apple silicon. The theme is that macOS diverges from
Linux most dangerously where the two _look_ identical: several of these paths
reported success while doing nothing at all.

- **The `apple` sandbox backend could never start a container.** Three separate
  reasons, each invisible to a unit test. Its CLI is not docker's: image
  operations live under an `image` noun, so `container image exists` and
  `container pull` both exit 64 (EX*USAGE) and failed the backend out of the
  chain on every launch. `--security-opt` and `--pids-limit` are rejected
  outright, so the default hardened profile could not create anything even once
  the verbs were right — they are now omitted, and the narrowing is \_reported*
  rather than silently applied. And the health probe ran
  `container container inspect --format '{{…}}'` — Apple has no `container`
  noun and no Go templates, so a container thegn had just successfully created
  read as "not running", was declared a failure, and was left running.
- **Host-toolchain mounts broke container creation outright on macOS.** thegn
  binds the host's `/usr`, `/bin`, `/lib` and `/nix/store` into the container so
  the user's real shell works inside it — correct on Linux, where host and guest
  are the same system. An OCI guest is _always_ Linux; on a Mac those paths hold
  Mach-O binaries, and mounting them over the guest's own directories produced
  `failed to find target executable sleep` and, for `/bin`, **`Exec format
error`**. Now gated on host and guest sharing an ABI. This affected podman and
  docker on macOS too, not only `apple`.
- **Two Nix injections went around that ABI gate, and podman could not start a
  single container on a Nix-managed Mac.** The gate covered the toolchain mounts;
  the Tier-B nix-daemon socket and the devenv `/nix` bind are injected separately
  and were unconditional. podman machine shares only `/Users`, `/private` and
  `/var/folders`, so `podman run` exited 125 with
  `statfs /nix: no such file or directory` — no container, every launch, on every
  such machine. thegn reported `could not start podman container '<name>'` and
  fell through to a host shell. The gate is now one predicate
  (`guest_shares_host_abi`) applied at all three sites, and a `nix_daemon` that
  was explicitly asked for says it was dropped rather than going quietly missing.
  Verified on the machine: podman and Apple `container` both start and show the
  full worktree.
- **A bind mount that delivered nothing was reported as a working sandbox.** The
  worktree is bind-mounted at its own absolute path; on macOS every OCI runtime
  resolves that _inside a Linux VM_, which only sees the host directories it was
  told to share. Both of thegn's checks agreed anyway: `container_status` built
  its expected set from `spec.mounts[].host` — the strings thegn had just asked
  for — and compared them to `.Mounts[].Source`, which the runtime echoes back
  unchanged, so it compared a request to a copy of itself; and the preflight probe
  ran `/bin/sh -lc true` with `--workdir <worktree>`, which an empty directory
  satisfies. Measured, the two runtimes fail differently: **podman 5.8.6 refuses
  the bind loudly** and thegn was discarding the one line that named the path,
  while **docker 29.5.2 via colima starts the container with the mount empty** —
  a pane opened on an empty worktree while thegn reported real containment. Now
  the create's stderr is kept and diagnosed, and a probe derived _only from paths
  just observed on the host_ verifies the bind from inside. On a verified failure
  the container is removed, because its binds are fixed at create and
  `container_status` would call it healthy forever — so widening the share would
  otherwise change nothing. The message names the missing path and the fix for
  that specific runtime (`podman machine init -v …` and that `machine set` has no
  `--volume`; `colima start -V …`; Docker Desktop's File Sharing pane), keyed on
  the _missing_ path's share root so a repo on an external volume is not
  misdiagnosed as its worktree's. Where nothing is provable the probe body is the
  literal `true` it was before, byte for byte.
- **A remedy that would have sent Apple `container` users down a dead end.**
  Written from the reasonable-sounding assumption that Apple's VM, like podman's
  and colima's, has a fixed share set — so a failed bind was told to "move the
  worktree under /Users". Measured on macOS 26, it does not: `/opt/homebrew`
  binds complete, and a worktree at `/opt/...` launches end to end. The only
  refusal Apple produces is for a genuinely absent path
  (`Error: path '<p>' does not exist`, exit 1, no container), which is now
  matched, and the remedy points at existence and readability instead. A test
  asserts the relocate/widen advice stays absent.
- **`doctor` under-reported its own isolation on macOS**, the one bug here that
  ran the other way: podman and docker were classed shared-kernel unconditionally,
  but on a Mac they reach their Linux container through a VM. Now guest-kernel
  there, unchanged on Linux, and an explicitly stronger OCI runtime still wins.
- **The activity scanner enumerated the whole process table at up to 1 Hz.**
  `sysinfo` reads `KERN_PROCARGS2` (two `sysctl`s plus an `ARG_MAX`-sized
  allocation) for _every_ process before consulting the refresh kind, so asking
  for two fields cost ~5 syscalls per process, ~500 processes, forever. Replaced
  with the libproc seam the codebase already argued for. Measured idle CPU:
  **0.075 → 0.058 cores**. The conversion is the subtle part —
  `proc_taskinfo`'s CPU fields are mach absolute units, and reading them as
  nanoseconds understates CPU by ~41× (every worktree permanently idle), so the
  timebase is applied and pinned by a test that burns real CPU.
- **The fs-watcher's ignore filter panicked** on any worktree under a symlinked
  prefix — `/tmp`, `/var/folders`, `~/code → /Volumes/…`. FSEvents delivers
  canonicalized paths, and `matched_path_or_any_parents` _asserts_ its argument
  is under the matcher root, so the callback died and the panel silently stopped
  updating. Roots and matcher are canonicalized; an out-of-root path now degrades
  to "this is an edit" rather than killing the thread that feeds it.
- **The font picker found none of its recommended fonts** on a machine that had
  one installed: it scanned only the top level of the macOS font directories,
  while macOS resolves them recursively and nix-darwin nests eight deep.
- **`doctor` reported a CPU cap that can never fire.** `nice` is on PATH so the
  probe selects it, but the wrapper only ever wraps `bwrap` (Linux-only) or a
  local `Backend::None` (which never produces a spec) — no macOS pane is ever
  wrapped. It now reports what it observes, the same rule already applied to
  sandbox containment.
- Smaller: a charge-capped Mac plugged in and idle read as "not on AC" (the exact
  bug the Linux adapter read was written to avoid); the bundled Alacritty profile
  forced `TERM` and thereby erased its own identity, resolving itself down to
  256-colour and no undercurl; `timeout --kill-after` failed with a bare `ENOENT`
  on any Mac without GNU coreutils; the host probe claimed userns support on
  every Mac and knew no `brew`.
- **Two sandbox backends looked finished and were not.** `smol`/`smolmachines`
  and `wsl` parse, sit in `Backend::ALL_OCI`, answer yes to `is_oci()` and are
  treated as docker clones for `--user`/`--gpus` — a complete surface with
  nothing behind it. `liveness_argv` returns `None` for both, so they fall back
  to a bare PATH probe: **"the binary exists" standing in for "the runtime
  works"**, the same defect `06ec12ff` fixed for docker and Apple. `doctor` now
  marks them unverified and says what `ready` actually means there, and a launch
  under one warns. Neither is in the default chain, so this only ever reaches
  someone who named it. Deliberately **not** "finished" by guessing its verbs —
  that is exactly how the `apple` backend acquired three launch-breaking bugs
  above.

### Added — macOS integrations that were absent

- **Thread QoS.** The render loop declares `Interactive`; hydration, samplers,
  the refresh ticker and the fs-watch builder declare `Utility`/`Background`, so
  off-loop work is efficiency-core eligible on Apple silicon. A no-op elsewhere.
- **Temperature sensors on Apple silicon.** `sysinfo::Components` is empty there
  and `ioreg` publishes no value — the sensors exist but only as HID _events_.
  Read via `IOHIDEventSystemClient`, with every symbol resolved by `dlsym` so a
  future macOS that drops them degrades to "no thermals" instead of a binary that
  will not launch. Curated to 16 distinct sensors (from 77 services under 17
  names), 80ms → 10ms, and `tdie` added to the CPU-temp matcher so the reading is
  the die rather than a calibration reference ~15C hotter.
- **`LC_TERMINAL` detection** — the one terminal identity that survives ssh,
  answering the case the 80ms DA/XTVERSION probe exists for, at no cost.
- **A Terminal.app truecolor gate**, colour-only and gated on a floor verified by
  eye at build 470.2 (macOS 26): glyphs, undercurl and synchronised output stay
  off, because Terminal.app has none of them.
- **`doctor` runs the probe the compositor runs**, so the two can no longer
  disagree about the same terminal over ssh/tmux, and gained a macOS section:
  Option-as-Meta for the detected terminal, `RLIMIT_NOFILE` against
  `kern.maxfilesperproc`, whether `$TMPDIR` can shorten the pane-daemon socket,
  and which of `osascript`/`afplay`/`pbcopy`/`fc-list`/`mediaremote-adapter` are
  present.
- **Font application targets the terminal that is running** (Ghostty, kitty,
  Alacritty) and declines with the exact setting to change for WezTerm,
  Terminal.app and iTerm2 — whose configs are Lua and plists. Previously it always
  patched an Alacritty config, which the macOS `.app` launcher only starts as its
  4th choice, and reported success either way.
- **The perf suite runs on darwin.** `cpu-sample.sh` gained a `top`-based
  sampler, `flood`/`t3` stopped hard-failing on a `/proc` liveness check, and
  `perf_host_tag` gets a real per-Mac fingerprint instead of every Mac sharing
  one baseline key. First darwin idle baseline recorded.

### Changed — a correction to an earlier claim

An earlier note in this work reported widespread test flakiness and a hanging
pre-push gate on macOS. **That was an artefact of the wrong runner.** `just test`
runs `cargo nextest`, which reads `.config/nextest.toml` — a concurrency cap, a
slow-timeout, and process-per-test. Run under bare `cargo test`, none of that
applies. Under the real runner the suite is clean. The keyring probe was
memoized and bounded anyway (it does a real Keychain _write_ per call, and can
block with no GUI session to authorize it), but it was not repairing a broken
gate. Two genuinely environment-dependent tests were fixed: a bind-0/drop/re-bind
port TOCTOU, and two `newest_child` tests that asked about the _test runner's own_
pid — sound on Linux, where the children file is per-thread, and racy on macOS,
where `proc_listchildpids` returns every child of the process.

### Added — a stopped runtime is a question, not a silent downgrade

- **`[sandbox] on_dormant`.** A container runtime that is installed but not
  running — a stopped `dockerd`, a `podman machine` nobody started, colima down —
  was folded into "absent" by the backend chain, and the pane opened on the host
  without a word. thegn already knew the difference (`BackendState::NotRunning`
  carries a remedy) but only said so in the onboarding wizard, which is the one
  moment you are not launching anything. Now a launch that asked for containment
  and would degrade offers the fix: **[s] start it · [h] run on host · [n]
  cancel**, with the start command shown verbatim before you approve it.
- Policy is configurable: `ask` (default), `start` (run it unattended and
  re-resolve), `host` (the old silent degrade, now truthfully labelled), or
  `cancel` (refuse to run uncontained). `start` falls back to `ask` for a runtime
  with no unattended start — rootful podman needs a password, and a launch path
  will not prompt for one.
- An `auto`/`host` launch that lands on the host is the configured outcome, not a
  degradation, and never raises the prompt.
- Starting runs off the event loop with a bounded 90s budget (booting a VM is
  slow), then drops the probe cache so the retry re-probes instead of replaying
  the cached "absent" that caused the degrade.
- **The macOS Docker remedy was wrong**: it told Mac users to run `systemctl`,
  which does not exist there, and never mentioned colima. The remedy sentence and
  the start command are both OS-aware now.

### Fixed — the macOS release process, rehearsed end to end

Built a real macOS release locally and ran it through the documented path. Three
things the docs claimed turned out to be wrong or imprecise:

- **`brew install --formula ./packaging/homebrew/thegn.rb` does not work.** Modern
  Homebrew rejects a formula given as a file path ("Homebrew requires formulae to
  be in a tap"), and both `RELEASING.md` and `KNOWN_ISSUES.md` told users to do
  exactly that. Replaced with the `brew tap-new` recipe, which was then used to
  install the real artifact end to end — formula installs, caveats render, `thegn`
  and `tg` both run from the brew prefix, and the installed binary carries **no**
  `com.apple.quarantine`, confirming the claim the whole no-notarization decision
  rests on.
- **"Unsigned" was imprecise.** Apple silicon requires a signature to execute, so
  the linker ad-hoc signs every arm64 binary (`codesign -dv` → `adhoc,
linker-signed`). That is not a Developer ID signature and does not satisfy
  notarization — `spctl -a` rejects it either way — but the docs now say what is
  actually true.
- **A quarantined binary hangs rather than failing.** It stalls on a Gatekeeper
  dialog. And once macOS has denied it, the verdict is cached: removing the
  attribute afterwards is not always enough. So the docs now say to clear it
  _before_ the first run, and name the recovery (System Settings → Privacy &
  Security, or a fresh path).

- **Release archives shipped no license text.** thegn is `MIT OR Apache-2.0` and
  the archive contained only the binary — the upload action includes nothing else
  by default. Both licenses and the README now ride along; the binary stays at the
  archive root, which is what the formula's `bin.install` requires.
- **`just release-artifacts <tag>` / `just release-verify <tag>`** reproduce the
  CI archive byte-shape locally (root layout, `shasum -a 256` fallback, no
  `.tar.gz` infix on the checksum file) and assert it. With remote CI paused this
  is the only way to find out a release build is broken before the tag is public,
  so `RELEASING.md` now has it as a step.

### Added — macOS reaches users, not just this machine

- **The macOS CI job is re-enabled.** It was disabled outright because its first
  real run OOM-killed building `openspec` — a tool the job never invokes. A new
  lean `devShells.ci` (toolchain, just, nextest, pkg-config, zlib) carries only
  what `just build` and `just test` use, removing the failure and cutting the
  cost. It stays off by default on the same `extras` gate as the `windows` and
  `e2e` jobs — macOS runners bill at 10x, and while dispatch is the only way CI
  runs at all, a bare dispatch must not quietly cost ten times more. Run it with
  `gh workflow run ci.yml --ref <branch> -f extras=true`.
- **`aarch64-apple-darwin` rejoins the release matrix**, so the next tag ships
  Apple-silicon binaries alongside linux-gnu and linux-musl.
- **The Homebrew formula is release-ready**: Apple-silicon only (matching the
  matrix), with caveats that tell the user how to generate the `thegn.app`
  launcher and how to make Option send Alt. `RELEASING.md` documents the exact
  tap layout so step 6 is mechanical.
- **A decision, written down: thegn does not sign or notarize.** Notarization
  costs a paid Apple Developer account and a signing key in CI. The supported
  macOS paths are chosen so it is not needed — Homebrew formula downloads, Nix
  store paths, and the locally generated `thegn.app` are none of them
  quarantined. Only a tarball fetched through a browser is, and both
  `RELEASING.md` and the README now say so plainly, with the `xattr` escape and
  the conditions under which the decision should be revisited.

### Fixed — thegn's own chords were untypeable on macOS

- **The bundled terminal profiles now map Option to Alt.** thegn's primary
  chords are Alt-based (`Alt-w` new worktree, `Alt-o` switch, `Alt-s` sidebar,
  `Alt-.` panel, plus every `Ctrl-Alt` chrome toggle), and macOS composes
  characters with Option by default — so `Alt-w` typed `∑`, nothing happened,
  and the key read as dead rather than unbound. `config/alacritty.toml`
  (`option_as_alt = "Both"`, where Alacritty's default of `None` meant thegn
  shipped a profile that could not type thegn's own keymap) and
  `config/ghostty.config` (`macos-option-as-alt = true`) both set it now, so
  `tg --standalone` and the generated `thegn.app` work out of the box.
- For the terminal **you** launch thegn in, the setting is yours to make: the
  in-app help (Terminal compatibility) and `KNOWN_ISSUES.md` list it for
  Ghostty, Alacritty, kitty, WezTerm and iTerm2.

### Fixed — a pane could claim a sandbox it did not have

- **Containment is now derived from the argv a pane executes, never from the request.** A terminal
  created with an explicit `podman-rootless` pick, on a host with no podman machine running,
  resolved through the chain to `Backend::None`, spawned a bare `sh -lc 'cd … && exec $SHELL'` —
  and was still labelled `podman-rootless`, because the label was copied from the pick. A label
  that can disagree with reality is worse than no label: it says "sandboxed" about a pane running
  on the host with no kernel boundary. The new `thegn_core::sandbox_truth` module reads the backend
  out of the command that actually runs and reconciles it against what was asked for, producing the
  label, a degraded flag, and a warning; `panes.rs` and `agent.rs::compose_spec` both go through it,
  and a degraded terminal now falls through to a plainly labelled host shell.
- **The gate that keeps it fixed**: `every_backend_round_trips` renders the real `enter_argv` for
  every `Backend` and asserts the derived label matches, over a list that is exhaustive by
  construction — adding a backend without extending the gate fails to compile. Companion tests pin
  the dangerous direction: a worktree path, git remote, or image reference named `docker` must
  never promote a host shell into a claimed container.

- **Recorded intent and observed containment are now separate columns.** The wizard's pick was
  written to `terminals.sandbox_backend` / `worktrees.sandbox_backend` before anything launched,
  and every surface displayed that column — so the chip reported the pick as fact. The pick stays
  where it is (it is a deliberate override that drives re-resolution, so a user who later starts
  their runtime still gets the sandbox they asked for), and a new `observed_backend` column records
  what each launch actually entered. The tab chip, the sidebar rows for both worktrees and
  terminals, and `active_backend` all read the observed value.
- The tab chip no longer **predicts**. It used to show the backend config would resolve to before a
  worktree had ever launched, so it was never empty; that was a claim rendered as fact. A chip that
  is briefly empty is honest, and it fills in the moment a pane launches.

### Added — macOS app launcher

- **`thegn.app`, generated locally** (`packaging/macos/make-app.sh`). macOS has
  no freedesktop registry, so `install.sh` used to opt darwin out of launcher
  integration entirely and leave Mac users with nothing to search for. It now
  detects the platform and writes a `thegn.app` bundle into `~/Applications`
  instead — indexed by Spotlight, Raycast, Alfred and the Dock — which opens the
  first terminal it finds (Ghostty → WezTerm → kitty → Alacritty → Terminal.app)
  running thegn through a login shell, so the tools thegn shells out to are on
  `PATH` under launchd's bare environment. `just macos-app` generates the same
  bundle for the Nix and Homebrew installs, which never run `install.sh`.
  Generating on the machine rather than shipping a prebuilt bundle is what keeps
  Gatekeeper out of the way: no `com.apple.quarantine`, so no Developer ID
  signing or notarization is needed to open it.
- **`--env KEY=VALUE`** bakes environment into a bundle, so a second,
  side-by-side launcher can run a debug binary with `THEGN_LOG` / `THEGN_PERF`
  and an isolated `XDG_STATE_HOME`.
- **`packaging/macos/thegn.icns`** — the owl app icon for the bundle, rendered
  from the same `owl.rs` sprite as `config/thegn.svg` by
  `scripts/gen-owl-icns.py` (pure stdlib: no rasterizer, no `iconutil`, so it
  also runs on Linux). `just icons` regenerates both.
- The `install.sh` summary no longer claims to have written a `.desktop` entry
  and an hicolor icon on platforms where it wrote neither.

### Added — macOS development (Apple silicon + nix-darwin)

- **`darwinModules.default`** — a nix-darwin module that puts thegn on PATH
  system-wide (`programs.thegn.enable`). It is deliberately thin: nix-darwin has
  no per-user config-file mechanism, so configuration stays with
  `homeManagerModules.default`, which already worked on darwin. Enabling both is
  the intended shape and they install the same store path. See the README's
  "nix-darwin (macOS)" section.
- **The flake's darwin outputs evaluate.** `packages.sandbox-image`,
  `fly-sandbox-image` and `thegn-musl` are now gated to Linux — the two OCI
  images could not even be _evaluated_ on darwin (`shadow`/`procps` refuse a
  darwin hostPlatform), which took `nix flake show` and `nix flake check` down
  with them. The systems list also drops `x86_64-darwin`, which the pinned
  nixpkgs has retired and which therefore threw on every Intel-mac attribute.
- **`devenv shell` no longer builds a cross-compiler on a Mac.** The mingw-w64
  cross-CC env vars were set unconditionally, so entering the shell on darwin
  forced a from-source mingw GCC build — and `.envrc` prefers devenv, so that
  was the shell a Mac contributor landed in. Now `isLinux`-gated, matching the
  flake's `mingwCrossEnv`.
- **The `openspec` derivation caps its memory** (`NODE_OPTIONS`, pnpm
  child-concurrency). Its `pnpm install` being OOM-killed on the 7 GB macOS
  runner is why the `[ci-macos]` job had never reached thegn at all.

### Fixed — macOS runtime parity

- **Activity dots work on macOS.** The per-worktree CPU scanner had a Linux
  `/proc` arm, a Windows `sysinfo` arm, and an empty stub for everything else —
  so dots never lit on a Mac. The `sysinfo` arm now serves every non-Linux
  platform.
- **`open` instead of a hardcoded `xdg-open`.** "Open in browser" now honours
  `$BROWSER` and falls back to the platform opener, which is what the
  `[forward] browser` docs had always promised and the code never did.
- **`apple` joins the default sandbox chain**, after `docker` and before
  `bwrap`. A Mac with Apple's `container` installed silently resolved `auto` to
  an unsandboxed host pane; the backend was fully wired but unreachable without
  naming it explicitly. Its probe is macOS-gated, so no other platform changes.
- **Pane cwd and foreground-command capture work on macOS**, via a new
  `platform::proc` seam (libproc/`sysctl`, no new dependency) behind what were
  bare `/proc` reads. Restores "respawn panes where they were" and "relaunch
  what was running" off Linux.
- **The font picker degrades** to scanning the standard macOS font directories
  when fontconfig (`fc-list`) is absent, instead of dead-ending.
- A watcher failure no longer blames "inotify watches exhausted" on platforms
  where `notify` rides FSEvents rather than inotify.

### Changed — dev loop portability

- **`just check-cross` covers six crates on darwin, not two** — every crate that
  builds without a darwin cross C toolchain. Each leg now skips loudly when its
  toolchain is missing instead of failing, so `just ci` is runnable on a Mac
  (where the mingw cross-cc is deliberately absent).
- **GNU-userland assumptions removed** from the dev loop: `sed -i`, `setsid`,
  `script -qec` (util-linux vs BSD, now via `test/lib/pty.sh`), `sha256sum`,
  and an `aarch64`-only `uname -m` case that Apple silicon reports as `arm64`.
  `install.sh` skips the freedesktop `.desktop`/icon files on darwin, and
  `test/perf/cpu-sample.sh` (hard `/proc` dependency) skips with a clear message.

### Added — PR queue (team mode)

- **`thegn pr queue`** — the merge queue's counterpart for a repo other people
  push to. Queue a pull request, and thegn polls its state on the forge,
  classifies what is blocking it (red checks, a conflict with the base,
  requested changes, awaiting review), optionally hands that blocker to a
  configured agent in the PR's own worktree, and merges it once it is green.
  Panel section, statusbar chip, notifications, and `--json` on every verb.
  **Off by default** (`[pr_queue] enabled`): it is the one part of the shell
  that makes network _writes_.
- **The forge stays in charge of merging.** The default `merge_mode =
"auto_merge"` switches on the forge's own auto-merge, so branch protection,
  required reviews, and any server-side merge queue remain authoritative —
  thegn's view of "ready" can never race a rule it cannot see. `"thegn"` merges
  directly; `"ready"` never merges. A draft, or a PR without its required
  approval, is never merged either way.
- **Safe to point at a shared repo.** The agent pushes only with
  `--force-with-lease`; a push thegn did not make pauses it rather than racing a
  teammate (`pause_on_foreign_push`), and also refills its attempt budget so a
  long-lived PR never gets permanently stuck. A pull request you did not author
  is watched but never written to (`own_prs_only`). Review threads can be
  replied to, never resolved — that is the reviewer's call.

### Added — configurable agent handoff (both queues)

- **Prompts are now templates, not Rust.** `[merge_queue.prompts]` and
  `[pr_queue.prompts]` let you tell the fixing agent about your repo's
  conventions, per blocker kind. Leave a key empty for thegn's built-in
  instructions — which are byte-for-byte what previous versions sent, so
  nothing changes unless you opt in. Unknown placeholders are a `config
validate` error rather than a silent blank.
- **`agent = "claude"`** — name one of your `[[agents]]`/`[[tools]]` entries
  instead of restating its command with the right headless flags. thegn fills
  those in per provider (claude / codex / aider), and an agent it does not
  recognize still runs with the prompt appended. `agent_command` still wins
  when set, so any agent at all remains configurable.

### Fixed

- **`agent_command`'s documented example was wrong.** The shipped
  `config.toml.example` showed `claude -p "{prompt}"`, but placeholders are
  already shell-quoted during substitution — the agent received a prompt with
  literal quote characters wrapped around it. Placeholders must be written
  **bare**; `thegn config validate` now reports the mistake.

### Fixed — worktree deletion and terminal graphics

- **Worktree deletion now updates the sidebar immediately and restores the row
  if deletion fails.**
- **Terminal graphics transfers stay intact.** Kitty image chunks are no longer
  combined or emitted partially, preventing incomplete images in panes.

## [0.1.0-alpha.2] — 2026-08-20

A packaging and hygiene release. `v0.1.0-alpha.1` was tagged but never
published; this is the first release the public actually gets, and it fixes the
things a first-time cloner hit rather than anything about the running shell —
plus one genuine crash.

### Fixed — a process-wide abort in the metrics sampler

The system-metrics sampler could abort the whole of thegn — every pane in the
session — with:

```text
fatal runtime error: IO Safety violation: owned file descriptor already closed
```

It asked sysinfo to refresh a PID list that could name the same PID twice (when
the recorded pane-daemon PID equalled our own). sysinfo 0.39 fans that list
across a rayon pool by list position rather than by PID, so a repeated PID hands
two worker threads the same process entry at once; both take its cached
`/proc/<pid>/stat` handle and one closes a descriptor the other still owns. std
detects the double close and aborts.

Reaching it needed a daemon PID equal to ours — normally impossible, but
possible when a still-heartbeating registry row names a PID the OS has recycled
onto us. It was, however, unconditional in `thegn-metrics`' own unit test, where
it aborted ~5% of runs and reddened CI at random. The sampler now drops the
duplicate.

### Fixed — first-clone experience

- **`git clone --recurse-submodules` failed.** A stale `.gitmodules` still
  listed two private `apps/*` repos that are not reachable anonymously and are
  no longer submodules of this project. Removed.
- **Every `just` build printed a warning about private code.** The `_apps`
  recipe — a dependency of `build`, `quick`, `release`, `build-musl`, `lint`,
  `test`, `coverage` and `build-profiling` — warned that the host's chat/agent
  tabs would not build whenever no `apps/` checkout existed, which for everyone
  outside the maintainer's machine is always. Nothing depends on it.
- **The default agent picker offered a binary nobody has.** With no user
  config, `[[agents]]` was seeded with `termite`/`termite tui`, an unpublished
  TUI. The defaults are now `claude` and `shell`.
- **The project read as MIT-only.** A stray bare `LICENSE` (naming a different
  copyright holder than `LICENSE-MIT`) overrode GitHub's detection of the
  `MIT OR Apache-2.0` dual license declared in every manifest. The README now
  states the licensing too.

### Fixed — `install.sh`

- No longer deletes four pre-rename entry-point names and two `.desktop` files
  from your `bin` directory. No public user ever had the old brand, and one of
  those names is short enough to plausibly belong to an unrelated tool. An
  installer must not remove what it did not create.
- Installs a **copy** of the release binary instead of a symlink into
  `target/release/`. The symlink broke silently on `cargo clean`, a pruned
  worktree, or a moved repo. Re-run `./install.sh` after a rebuild.
- Fails loudly when the build produced no binary, instead of installing a
  dangling symlink.

### Fixed — CI and tooling

- **`pull_request` jobs no longer default to self-hosted runners.** On a public
  repo that fallback meant fork-PR code running on a personal machine against a
  persistent nix store, sccache and `CARGO_HOME`. GitHub-hosted is now the
  default and self-hosted is an explicit opt-in.
- **`NIX_GITHUB_TOKEN` is documented as optional.** Four places claimed it was
  required for "private flake inputs"; every flake input is public, and fork
  PRs — which receive no secrets — were being told their CI was unfixable.
- **Three gates were narrower than they claimed.** `doc-check` ran rustdoc
  without `--document-private-items` on a codebase that is overwhelmingly
  private, hiding seven broken intra-doc links and unclosed-HTML-tag errors
  (all fixed); `shellcheck` ran a hand-kept list that had drifted to 9 of 20
  tracked scripts, excluding the user-facing `setup-macos.sh`; and `taplo lint`
  was linting `.direnv`, `.devenv` and `target/` — 122 files, almost none ours.
- `CONTRIBUTING.md` now says that `just ci` must run under `nix develop`: the
  direnv/devenv shell's toolchain has neither `llvm-tools-preview` nor the
  cross targets, so `coverage` and `check-cross` fail there.

### Removed

Dead artifacts carrying no references: the Python LLM sidecar and its
agent adapters (`src/`, `tests/`), 196 directories of visual-regression
baselines for a harness that no longer exists (`snapshots/`), and ~195 KB of
mascot design-exploration scripts.

## [0.1.0-alpha.1] — 2026-08-18

The first public release: the AI-free terminal shell. Everything below is the
initial feature set rather than a delta.

### Platforms

Prebuilt binaries ship for **x86_64 Linux** (gnu + musl) only. Nix
(`nix profile install github:blakeashleyjr/thegn`) and `./install.sh` cover
Linux too.

macOS and Windows are **not supported** in this release and neither has been
run interactively. Windows compiles on msvc and passes its named-pipe IPC and
Job-Object tests — the repo was previously unclonable there, because a source
file used the reserved DOS device name `aux` — but its release build has not
yet completed inside the CI job budget. macOS has never been compiled at all:
it cannot be cross-checked from Linux, since `thegn-host`'s build scripts need
a real darwin C toolchain, and the macOS job cannot currently construct the dev
shell (the `openspec` derivation's `pnpm install` is OOM-killed on the runner).

### The shell

- **Worktree-native multiplexing** — each repo is a project, each git
  worktree is a tab; panes, splits, zoom, copy mode, scrollback search, and a
  command palette, all rendered in-process (no tmux/zellij underneath).
- **Pane daemon** — PTYs are owned by a background `thegn daemon` process
  (unix socket), so quitting the UI detaches sessions and the next launch
  warm-reattaches them, scrollback intact. `thegn serve` exposes the same
  sessions to remote thin clients over TCP (token-auth). Disable with
  `[daemon] enabled = false` for fully in-process panes.
- **Git, first-class** — sidebar tree with live status/activity, diff and PR
  panels, blame, semantic (entity-level) diff summaries, visual staging,
  branch actions, undo/redo of repo operations, and a GitHub PR/issue/CI
  surface (`gh`-backed with native fallbacks). Worktree rows track the live
  HEAD branch, drag-and-drop reordering is scoped to folder-aware sibling runs,
  and click hit-testing resolves against the painted focus state.
- **Merge queue (“fold-actor”)** — `thegn merge` / `land` / `integrate` fold
  queued branches into the target branch in the object database (no target
  checkout), gate the folded tip on your test command in a reusable gate
  worktree, and advance the ref by compare-and-swap. Conflicts can be handed
  to any configured `agent_command` subprocess or deferred to you. Landing or
  dequeuing a worktree un-files it from its lifecycle folder.
- **Sandboxing** — each worktree's interactive process can run in a container
  (`podman` → `docker` → `bwrap` → `none` auto-chain) with hardening presets
  (`open`/`hardened`/`sealed`/`sealed-tunnel`), egress allow/block lists, a
  VPN sidecar option, and devshell/direnv injection. The worktree stays on
  the host so git reads keep working.
- **Sessions that survive** — worktrees, tabs, layouts, terminal order, pins,
  and sidebar state persist in SQLite and resurrect on launch; git remains
  the source of truth.
- **Launcher picker** — creating a worktree opens a tab and prompts for what
  to run there: a shell, or any configured `[[agents]]`/`[[tools]]` entry
  (claude, aider, lazygit, yazi, …) launched as an ordinary command.
- **Terminal compatibility** — capability detection with graceful
  degradation: truecolor→256→16→mono, Unicode→ASCII glyphs, undercurl and
  mouse feature-gating. `thegn doctor` prints what was detected.
- **Performance invariants** — sub-300ms launch to first frame, damage-region
  rendering (<16ms frames), and ~0% idle CPU (the idle loop blocks; it never
  polls). Enforced by unit-tested render-plan invariants in CI. Pane spawns
  (including crash respawns and the new-terminal wizard) resolve their sandbox
  off-thread, and pane stdin rides a bounded per-pane writer queue — a
  flow-stopped or wedged child can never freeze the UI (a dropped paste
  surfaces as a toast instead).
- **Input safety** — bracketed pastes are delivered to the child as one atomic
  chunk with embedded `ESC[200~`/`ESC[201~` markers neutralized on every paste
  path, so pasted content can't close the bracket early and inject keystrokes.
- **CLI** — a stable, documented exit-code contract; worktree targeting
  unified on a canonical `--worktree` flag across verbs (legacy positionals
  still parse); `thegn config validate` strict-checks every enum-valued config
  key, and `thegn config set` rolls back a value the loader would reject
  (without being blocked by a pre-existing bad value elsewhere in the file).
- **Config** — layered TOML (global → repo → env/profile overlays → env vars
  → `--set`), every key documented in `config/config.toml.example`, hot
  reload, and an in-app F1 help system with generated keybinding/config
  references. Per-zone key tables are single-sourced from the dispatch, so
  every live key surfaces in the help rather than drifting out of it.

### Channels

The stable build ships the shell above. Experimental subsystems (remote
worktrees over SSH, cloud execution providers, Observe dashboards, the
placement engine, non-GitHub trackers) are compiled in but disabled outside
the dev channel (`THEGN_CHANNEL=dev` / the `thegn-dev` build).

### Removed before this release

The in-repo AI/agent layer (an LLM proxy daemon, an ACP-embedded agent with a
tool-approval “bouncer”, and a managed agent install) was removed from the
codebase prior to this release to ship a focused, AI-free shell. The generic
launcher picker and the merge queue's `agent_command` hook remain — they run
whatever commands you configure. Leftover `[llm_proxy]` config sections and
`proxy_*` tables in existing state databases are ignored harmlessly.

[Unreleased]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.123...HEAD
[0.1.0-alpha.123]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.122...v0.1.0-alpha.123
[0.1.0-alpha.122]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.121...v0.1.0-alpha.122
[0.1.0-alpha.121]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.120...v0.1.0-alpha.121
[0.1.0-alpha.120]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.119...v0.1.0-alpha.120
[0.1.0-alpha.119]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.118...v0.1.0-alpha.119
[0.1.0-alpha.118]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.117...v0.1.0-alpha.118
[0.1.0-alpha.117]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.116...v0.1.0-alpha.117
[0.1.0-alpha.116]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.115...v0.1.0-alpha.116
[0.1.0-alpha.115]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.114...v0.1.0-alpha.115
[0.1.0-alpha.114]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.113...v0.1.0-alpha.114
[0.1.0-alpha.113]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.112...v0.1.0-alpha.113
[0.1.0-alpha.112]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.111...v0.1.0-alpha.112
[0.1.0-alpha.111]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.110...v0.1.0-alpha.111
[0.1.0-alpha.110]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.109...v0.1.0-alpha.110
[0.1.0-alpha.109]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.108...v0.1.0-alpha.109
[0.1.0-alpha.108]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.107...v0.1.0-alpha.108
[0.1.0-alpha.107]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.106...v0.1.0-alpha.107
[0.1.0-alpha.106]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.105...v0.1.0-alpha.106
[0.1.0-alpha.105]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.104...v0.1.0-alpha.105
[0.1.0-alpha.104]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.103...v0.1.0-alpha.104
[0.1.0-alpha.103]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.102...v0.1.0-alpha.103
[0.1.0-alpha.102]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.101...v0.1.0-alpha.102
[0.1.0-alpha.101]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.100...v0.1.0-alpha.101
[0.1.0-alpha.100]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.99...v0.1.0-alpha.100
[0.1.0-alpha.99]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.98...v0.1.0-alpha.99
[0.1.0-alpha.98]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.97...v0.1.0-alpha.98
[0.1.0-alpha.97]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.96...v0.1.0-alpha.97
[0.1.0-alpha.96]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.95...v0.1.0-alpha.96
[0.1.0-alpha.95]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.94...v0.1.0-alpha.95
[0.1.0-alpha.94]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.93...v0.1.0-alpha.94
[0.1.0-alpha.93]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.92...v0.1.0-alpha.93
[0.1.0-alpha.92]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.91...v0.1.0-alpha.92
[0.1.0-alpha.91]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.90...v0.1.0-alpha.91
[0.1.0-alpha.90]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.89...v0.1.0-alpha.90
[0.1.0-alpha.89]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.88...v0.1.0-alpha.89
[0.1.0-alpha.88]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.87...v0.1.0-alpha.88
[0.1.0-alpha.87]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.86...v0.1.0-alpha.87
[0.1.0-alpha.86]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.85...v0.1.0-alpha.86
[0.1.0-alpha.85]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.84...v0.1.0-alpha.85
[0.1.0-alpha.84]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.83...v0.1.0-alpha.84
[0.1.0-alpha.83]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.82...v0.1.0-alpha.83
[0.1.0-alpha.82]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.81...v0.1.0-alpha.82
[0.1.0-alpha.81]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.80...v0.1.0-alpha.81
[0.1.0-alpha.80]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.79...v0.1.0-alpha.80
[0.1.0-alpha.79]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.78...v0.1.0-alpha.79
[0.1.0-alpha.78]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.77...v0.1.0-alpha.78
[0.1.0-alpha.77]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.76...v0.1.0-alpha.77
[0.1.0-alpha.76]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.75...v0.1.0-alpha.76
[0.1.0-alpha.75]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.74...v0.1.0-alpha.75
[0.1.0-alpha.74]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.73...v0.1.0-alpha.74
[0.1.0-alpha.73]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.72...v0.1.0-alpha.73
[0.1.0-alpha.72]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.71...v0.1.0-alpha.72
[0.1.0-alpha.71]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.70...v0.1.0-alpha.71
[0.1.0-alpha.70]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.69...v0.1.0-alpha.70
[0.1.0-alpha.69]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.68...v0.1.0-alpha.69
[0.1.0-alpha.68]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.67...v0.1.0-alpha.68
[0.1.0-alpha.67]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.66...v0.1.0-alpha.67
[0.1.0-alpha.66]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.65...v0.1.0-alpha.66
[0.1.0-alpha.65]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.64...v0.1.0-alpha.65
[0.1.0-alpha.64]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.63...v0.1.0-alpha.64
[0.1.0-alpha.63]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.62...v0.1.0-alpha.63
[0.1.0-alpha.62]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.61...v0.1.0-alpha.62
[0.1.0-alpha.61]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.60...v0.1.0-alpha.61
[0.1.0-alpha.60]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.59...v0.1.0-alpha.60
[0.1.0-alpha.59]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.58...v0.1.0-alpha.59
[0.1.0-alpha.58]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.57...v0.1.0-alpha.58
[0.1.0-alpha.57]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.56...v0.1.0-alpha.57
[0.1.0-alpha.56]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.55...v0.1.0-alpha.56
[0.1.0-alpha.55]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.54...v0.1.0-alpha.55
[0.1.0-alpha.54]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.53...v0.1.0-alpha.54
[0.1.0-alpha.53]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.52...v0.1.0-alpha.53
[0.1.0-alpha.52]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.51...v0.1.0-alpha.52
[0.1.0-alpha.51]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.50...v0.1.0-alpha.51
[0.1.0-alpha.50]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.49...v0.1.0-alpha.50
[0.1.0-alpha.49]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.48...v0.1.0-alpha.49
[0.1.0-alpha.48]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.47...v0.1.0-alpha.48
[0.1.0-alpha.47]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.46...v0.1.0-alpha.47
[0.1.0-alpha.46]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.45...v0.1.0-alpha.46
[0.1.0-alpha.45]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.44...v0.1.0-alpha.45
[0.1.0-alpha.44]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.43...v0.1.0-alpha.44
[0.1.0-alpha.43]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.42...v0.1.0-alpha.43
[0.1.0-alpha.42]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.41...v0.1.0-alpha.42
[0.1.0-alpha.41]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.40...v0.1.0-alpha.41
[0.1.0-alpha.40]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.39...v0.1.0-alpha.40
[0.1.0-alpha.39]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.38...v0.1.0-alpha.39
[0.1.0-alpha.38]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.37...v0.1.0-alpha.38
[0.1.0-alpha.37]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.36...v0.1.0-alpha.37
[0.1.0-alpha.36]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.35...v0.1.0-alpha.36
[0.1.0-alpha.35]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.34...v0.1.0-alpha.35
[0.1.0-alpha.34]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.33...v0.1.0-alpha.34
[0.1.0-alpha.33]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.32...v0.1.0-alpha.33
[0.1.0-alpha.32]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.31...v0.1.0-alpha.32
[0.1.0-alpha.31]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.30...v0.1.0-alpha.31
[0.1.0-alpha.30]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.29...v0.1.0-alpha.30
[0.1.0-alpha.29]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.28...v0.1.0-alpha.29
[0.1.0-alpha.28]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.27...v0.1.0-alpha.28
[0.1.0-alpha.27]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.26...v0.1.0-alpha.27
[0.1.0-alpha.26]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.25...v0.1.0-alpha.26
[0.1.0-alpha.25]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.24...v0.1.0-alpha.25
[0.1.0-alpha.24]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.23...v0.1.0-alpha.24
[0.1.0-alpha.23]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.22...v0.1.0-alpha.23
[0.1.0-alpha.22]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.21...v0.1.0-alpha.22
[0.1.0-alpha.21]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.20...v0.1.0-alpha.21
[0.1.0-alpha.20]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.19...v0.1.0-alpha.20
[0.1.0-alpha.19]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.18...v0.1.0-alpha.19
[0.1.0-alpha.18]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.17...v0.1.0-alpha.18
[0.1.0-alpha.17]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.16...v0.1.0-alpha.17
[0.1.0-alpha.16]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.15...v0.1.0-alpha.16
[0.1.0-alpha.15]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.14...v0.1.0-alpha.15
[0.1.0-alpha.14]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.13...v0.1.0-alpha.14
[0.1.0-alpha.13]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.12...v0.1.0-alpha.13
[0.1.0-alpha.12]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.11...v0.1.0-alpha.12
[0.1.0-alpha.11]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.10...v0.1.0-alpha.11
[0.1.0-alpha.10]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.9...v0.1.0-alpha.10
[0.1.0-alpha.9]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.8...v0.1.0-alpha.9
[0.1.0-alpha.8]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.7...v0.1.0-alpha.8
[0.1.0-alpha.7]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.6...v0.1.0-alpha.7
[0.1.0-alpha.6]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.5...v0.1.0-alpha.6
[0.1.0-alpha.5]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.4...v0.1.0-alpha.5
[0.1.0-alpha.4]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.3...v0.1.0-alpha.4
[0.1.0-alpha.3]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.2...v0.1.0-alpha.3
[0.1.0-alpha.2]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.1...v0.1.0-alpha.2
[0.1.0-alpha.1]: https://github.com/blakeashleyjr/thegn/releases/tag/v0.1.0-alpha.1
