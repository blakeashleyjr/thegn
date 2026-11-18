# Rebuilding your real instance

Run `just live-plan` first to inspect the selected source, installation and state
paths without building, stopping anything, or opening the database for migration.
Then run `just live` from the checkout you actually intend to build. Python 3.11+
and the existing development-shell build dependencies are required.

To bootstrap the helper from a reviewed worktree before it reaches `main`, use
`python3 -B /absolute/worktree/scripts/live.py --repo /absolute/repo --plan`,
then omit `--plan` when ready. `--repo` selects the source checkout to build.
The installation is chosen separately (see below) and cannot be redirected
around a migration pin.

## Where it installs

The installed binary lives **outside the checkout**, so no release build in the
repo can replace the instance you work in:

- `<install-dir>/thegn`, the executable;
- `<install-dir>/alacritty.toml`, a copy of the repo's `config/alacritty.toml`,
  installed atomically next to the binary.

`<install-dir>` is `--install-dir` if given, else `$THEGN_LIVE_INSTALL_DIR`,
else `~/.local/lib/thegn`. `~` is the account's home from the passwd
database, not `$HOME`, because the helper is sometimes run from a shell whose
`HOME` is a profile directory. `XDG_DATA_HOME` is not consulted. The directory
must be absolute and outside the checkout, and it is created on the first real
run. `--plan` never creates it.

**First run after the move.** When nothing is installed at `<install-dir>` yet
but `<repo>/target/release/thegn` exists, that in-repo binary is treated as the
previous installation: its processes count as this installation's, and it is the
binary copied into the recovery directory. The new build is then installed to
`<install-dir>`. The plan output says so. The in-repo binary is never modified
or removed.

**Launchers.** The helper does not silently change anything in `~/.local/bin`.
`--plan`, the start of a run and the end of a run print the launcher state:
whether `~/.local/bin/thegn` resolves to the install target, and whether `tg` and
`tg-tui` still mention `<repo>/target/release/thegn` or `<repo>/config/alacritty.toml`.
Pass `--relink-launchers` (for example
`python3 -B scripts/live.py --relink-launchers`) to opt in. After the install
it repoints the `~/.local/bin/thegn` symlink to the install target. In `tg` and
`tg-tui` it replaces exact occurrences of those two old paths with the new ones.
A path that merely starts with an old one is left alone. A `.bak` copy is
written beside each wrapper first. The whole change is validated during
preflight, before anything is stopped. The helper refuses and changes nothing
if `~/.local/bin/thegn` is not a symlink, or if a wrapper is not a current-user
regular UTF-8 text file. It also refuses when a wrapper mentions neither the old
paths nor the new ones. A failure while writing, which can only happen after the
install, is reported and does not stop the launch.

`just live` is a local developer upgrade, not a merge or release command. It does
not commit, merge queued work, switch branches, or push. A rebuild of `main` does
not include changes still waiting in another worktree.

The selected checkout must pass ordinary Git clean checks before and after the
build. Its recorded revision is an observation, not proof that an immutable
commit snapshot was compiled: this helper builds the checkout in place. Do not
edit sources during the build. The staged executable's checksum binds the
artifact that is copied into the installation.

## Upgrade sequence

`just live` runs six steps without a typed confirmation, printing
`==> [n/6]` as it goes:

1. **Preflight.** Check the supported paths and migration policy, take both
   launcher locks, and recheck the clean checkout.
2. **Build** the release host with profiling, on every core, in one persistent
   private build cache (`target/live-cache`). Each upgrade therefore recompiles
   only what changed since the last one. Only the first `just live` builds
   everything. The cache is used by `just live` alone, so it stays isolated from
   other worktrees' Cargo output and from `target/release`. The running instance keeps working and the installed executable
   is untouched. The finished binary is copied into a fresh stage, which is the
   checksummed artifact that gets installed. Every stage is marked as owned
   before Cargo starts, so a failed build remains available for inspection.
3. **Validate** the configuration with the _new_ build (`thegn config validate`).
   A rejection stops here, before anything running is touched.
4. **Stop** the running instance after a 5-second window in which Ctrl-C aborts
   (`--yes` skips it). Controllers are sent SIGTERM first, which is their
   graceful-quit path, so the session layout is persisted while the daemon still
   serves them. Then the pane daemon gets `thegn daemon stop`. Pane processes end
   with the daemon. Anything of this installation still running after the grace
   periods gets SIGTERM, then SIGKILL.
   Only **this installation's** processes are ever signalled (see "Which
   processes are this installation's" below). Identity never comes from a pid
   file or a command-line pattern. A process of this installation that cannot be
   stopped still refuses the upgrade with its pid. The staged build is kept, so
   a rerun after you stop it is quick.
5. **Back up and install.** Check again for observable same-user processes of
   this installation, and for any process holding the database or the installed
   files open. Then acquire the database schema lock. Create a private recovery
   directory holding an online SQLite backup and a copy of the previous
   executable. Then atomically replace `alacritty.toml` and the executable in
   the install directory. Failures before the replacement leave the installed
   executable unchanged. With `--relink-launchers` the launchers are updated now.
6. **Launch** in the foreground with profiling and rotating logs enabled.
   Database migration is performed by the normal controller startup under the
   existing migration authority. The helper does not override
   `THEGN_DATABASE_MIGRATION_EXECUTABLE` or grant itself migration permission.
   About 20 seconds after launch, `thegn doctor` output is captured to
   `doctor.txt` in the recovery directory. Keep the terminal open while this
   supervised launch is running.

**Running it from inside thegn.** A shell in a thegn pane would die with the
daemon at step 4, taking the upgrade with it. When the helper finds a
`thegn`/`tg` among its ancestors, it reopens itself in a fresh ghostty window and
exits. That window runs in its own session and, when `systemd-run` is available,
in its own user scope, so it survives the stop. The window stays open after the
command ends, so a refusal can be read. Without ghostty the helper refuses and
asks you to run it from an outside terminal.

The default rotating application log allowance is 120 MB: 20 MB active plus five
rotations. `just live trace` changes verbosity; `just live debug 5 2` requests
15 MB instead. These are positional arguments, not `level=trace` assignments.
Build artifacts, recovery backups, stderr and profiler output are separate from
the rotating application log allowance. The helper retains the current staged
build and the two newest prior owned stages. It removes only marked, current-user
owned, private stages whose descriptor-checked trees contain regular files and
directories, and only after acquiring their per-stage lock. Active stages and
stages named by recovery metadata are preserved. Unmarked or otherwise unknown
legacy entries are preserved for manual inspection. Recovery backups are never
automatically pruned.
Recovery descriptors omit transient staged-build paths, so completed upgrades do
not pin every successful stage; explicit legacy or external recovery references
remain protected.
Each recovery directory retains its own `thegn-stderr.log`; the previous shared
stderr file is not truncated. `target/live-cache` is never pruned
automatically; `rm -rf target/live-cache` only costs the next upgrade a full
build.

## Which processes are this installation's

A same-user process named `thegn` or `tg` is attributed by two facts read from
`/proc` at the moment of the scan:

- its **state home**: `XDG_STATE_HOME` from `/proc/<pid>/environ` if absolute,
  else `$HOME/.local/state`, which is how thegn picks it. This is the same rule
  `inst_is_ours` in `test/lib/instance.sh` uses, extended with the `HOME`
  fallback because the default instance does not set `XDG_STATE_HOME`;
- its **binary**: whether `/proc/<pid>/exe` is the install target, or, on the
  first run, the legacy in-repo binary.

| State home | Binary                                               | Treatment                                                                                                 |
| ---------- | ---------------------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| this run's | this installation's, or any executable named `thegn` | **this installation**: stopped                                                                            |
| this run's | something else named `tg`                            | this installation, but never signalled; blocks until you stop it                                          |
| another    | this installation's                                  | not signalled and does not block. It keeps running the old binary until it is restarted. The plan says so |
| another    | another                                              | another installation: listed in the plan, left alone, does not block                                      |
| unreadable | any                                                  | never signalled; blocks the upgrade, since it cannot be told apart                                        |

Every listed process is printed in the plan with its pid, executable and state
home. Whatever its attribution, a process holding this installation's database
or installed files open still blocks the upgrade.

**Two instances.** A stable instance, which is the installed binary, and a dev
instance, built from a checkout with `just start` or `just dev tui` under its own
`XDG_STATE_HOME`, are separate installations. `just live` upgrades and stops only
the stable one. A running dev instance is reported and left alone, and it no
longer blocks the upgrade.

## Support boundary

This first version upgrades an existing Linux/default-profile installation,
using the normal XDG state/config paths. An existing database and installed
executable are required; this is not a first-install command. Ambiguous ownership,
unsupported path layouts, unreadable process inspection, named profiles and a migration executable pinned elsewhere
are reasons to stop, not reasons to broaden process matching or change policy.
Use the normal application configuration workflow for other profiles or pins.
Hard-linked databases are unsupported because another alias can use a different
schema lock. Hard-linked installed binaries can be replaced without changing
their other links. Another instance is left running only when its environment
proves a different state home. An instance whose environment cannot be read is
never signalled and blocks the upgrade.
The helper suppresses the legacy brand-directory rename during launch; it does
not suppress or bypass database schema migration.

The launcher locks serialize cooperating `just live` invocations by both state
root and installation directory, including two state roots sharing one binary. The schema
lock protects against cooperating schema users. `/proc` inspection is only a
point-in-time observation: it cannot prevent an older launcher, an unrelated
CLI, or a service from starting after the scan. Do not run other launchers or
database commands during the transition. Root or hostile processes running as
your own user are outside this developer tool's protection boundary.
Stage retention binds each candidate's marker, lock and directory identity to
descriptor-relative checks; if an entry changes during inspection, cleanup is
refused and the stage is preserved for manual review. Do not edit or remove
staged-build directories concurrently with the helper.

Backup time limits are checked between SQLite and copy steps. They do not
interrupt a stalled kernel or filesystem operation.

Successful process creation is **not** a readiness check. Verify the window,
worktrees and queue after startup. If startup fails, the helper reports failure
and retains recovery material; it does not silently relaunch an old binary
against a potentially upgraded database. An old executable may reject the new
schema. Restoring a backup requires a separate, deliberate offline recovery
decision and can discard changes made after that backup.

## Verification

`just test-live` (also included in `just test`) runs private regression tests
without a real build, live controller shutdown, live database migration or
installed-binary replacement. Process selection, stop ordering and escalation
are tested against a fake `/proc` and injected signal and subprocess functions.
Real controller startup and migration remain a separate operator-controlled
verification step. THE-613 tracks this bounded upgrade helper; THE-436 retains
generation-bound automatic shutdown and the other developer launcher recipes.
