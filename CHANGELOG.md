# Changelog

All notable changes to **thegn** are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[SemVer](https://semver.org) with pre-release tags (`0.1.0-alpha.N` →
`0.1.0-beta.N` → `0.1.0`). Release tags are `v<version>` on `main`.

## [Unreleased]

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
  the bottom of the window — and **always** for workspace and folder headers, so
  those two could never be moved to the last position by anyone.
  - **Behaviour change:** if you had learned to aim one slot low to compensate,
    you will now overshoot the other way.
  - The tail of a run you are not already in is reached through that run's
    header, as before: drop on a folder header to file at its end, or on the
    workspace header to unfile at the end of the loose list.
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
  workspace drags it was drawn under the _header_ while the drop landed after
  that header's whole subtree.
- **A workspace drop is atomic.** It used to step-swap its way to the target, one
  rebuild and one database write per step, and could bail out half way on a
  pinned neighbour — leaving the workspace parked between where it started and
  where you aimed. Every drop is now one resolved order, applied once; a refused
  drop changes nothing.

||||||| da1d8d5c

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

The first public release: the AI-free workspace shell. Everything below is the
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

- **Worktree-native multiplexing** — each repo is a workspace, each git
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

[Unreleased]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.2...HEAD
[0.1.0-alpha.2]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.1...v0.1.0-alpha.2
[0.1.0-alpha.1]: https://github.com/blakeashleyjr/thegn/releases/tag/v0.1.0-alpha.1
