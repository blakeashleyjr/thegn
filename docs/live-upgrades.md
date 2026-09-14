# Rebuilding your real instance

Run `just live-plan` first to inspect the selected source, installation and state
paths without building, stopping anything, or opening the database for migration.
Then run `just live` from the checkout you actually intend to build. Python 3.11+
and the existing development-shell build dependencies are required.

To bootstrap the helper from a reviewed worktree before it reaches `main`, use
`python3 -B /absolute/worktree/scripts/live.py --repo /absolute/repo --plan`,
then omit `--plan` when ready. `--repo` selects both the source checkout and its
`target/release/thegn` installation; it cannot redirect installation around a
migration pin.

`just live` is a local developer upgrade, not a merge or release command. It does
not commit, merge queued work, switch branches, or push. A rebuild of `main` does
not include changes still waiting in another worktree.

The selected checkout must pass ordinary Git clean checks before and after the
build. Its recorded revision is an observation, not proof that an immutable
commit snapshot was compiled: this helper builds the checkout in place. Do not
edit sources during the build. The staged executable's checksum binds the
artifact that is copied into the installation.

## Upgrade sequence

1. Check the supported paths and migration policy, then build the release host
   with profiling in fresh, isolated Cargo output and intermediate directories.
   The existing installed executable remains untouched during the build.
2. Close the old controllers and daemons yourself, including any automatic
   restart service. Save pane work first. Confirm the displayed installation
   before proceeding. The helper never signals existing controllers or daemons
   and never trusts a stale PID file.
3. Check for observable same-user processes using the destination executable or
   database files, acquire the database schema lock, and create a private recovery
   directory containing an online SQLite backup and a copy of the old executable.
   Failures before installation leave the installed executable unchanged.
4. Atomically replace the executable and launch it in the foreground with
   profiling and rotating logs enabled. Database migration is performed by the
   normal controller startup under the existing migration authority. The helper
   does not override `THEGN_DATABASE_MIGRATION_EXECUTABLE` or grant itself migration
   permission. Keep the terminal open while this supervised launch is running.

The default rotating application log allowance is 120 MB: 20 MB active plus five
rotations. `just live trace` changes verbosity; `just live debug 5 2` requests
15 MB instead. These are positional arguments, not `level=trace` assignments.
Build artifacts, recovery backups, stderr and profiler output are separate from
the rotating application log allowance. Review and remove retained artifacts
manually when they are no longer needed; there is no automatic backup pruning.
Each recovery directory retains its own `thegn-stderr.log`; the previous shared
stderr file is not truncated. Fresh isolated builds trade incremental build speed
and disk use for separation from other worktrees' Cargo outputs.

## Support boundary

This first version upgrades an existing Linux/default-profile installation,
using the normal XDG state/config paths. An existing database and installed
executable are required; this is not a first-install command. Ambiguous ownership,
unsupported path layouts, unreadable process inspection, named profiles and a migration executable pinned elsewhere
are reasons to stop, not reasons to broaden process matching or change policy.
Use the normal application configuration workflow for other profiles or pins.
Hard-linked databases are unsupported because another alias can use a different
schema lock. Hard-linked installed binaries can be replaced without changing
their other links. The conservative process scan also refuses other visible
same-user `thegn`/`tg` executables; it does not infer that another instance is safe
merely from a different state root or socket name.
The helper suppresses the legacy brand-directory rename during launch; it does
not suppress or bypass database schema migration.

The launcher locks serialize cooperating `just live` invocations by both state
root and installation directory, including two state roots sharing one binary. The schema
lock protects against cooperating schema users. `/proc` inspection is only a
point-in-time observation: it cannot prevent an older launcher, an unrelated
CLI, or a service from starting after the scan. Do not run other launchers or
database commands during the transition. Root or hostile processes running as
your own user are outside this developer tool's protection boundary.

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
installed-binary replacement.
Real controller startup and migration remain a separate operator-controlled
verification step. THE-613 tracks this bounded upgrade helper; THE-436 retains
generation-bound automatic shutdown and the other developer launcher recipes.
