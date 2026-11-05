---
id: upgrading-and-uninstalling
title: Upgrading and uninstalling
order: 41
---

# Upgrading and uninstalling

This page covers the release archive, Nix, and source-checkout installs. Your
configuration, state database, and worktrees are kept when you upgrade or
uninstall the binary.

## Before you start

Quit thegn, then run:

```sh
thegn daemon stop
```

It checks the pane-daemon listeners it knows about and reports any it cannot
check or stop. It prints that no pane daemon is running only after checking all
listeners.

Windows support is deferred. On Linux and best-effort macOS, config is under
`$XDG_CONFIG_HOME/thegn` (usually `~/.config/thegn`); run `thegn config path`
to print the config file path. State is under `$XDG_STATE_HOME/thegn` (usually
`~/.local/state/thegn`) and includes `thegn.db`, logs, and crash reports.
Worktrees are under the configured `worktrees_dir`, which defaults to
`~/.thegn/worktrees`. Upgrading or removing the binary does not remove your
repositories or worktrees.

## One state database, forward-only schema

The state database is shared by thegn processes using that state directory.
Schema migrations move forward; they cannot be reversed. With the default
`[database] migration_authority = "controller"`, a long-lived controller
(such as the host or pane daemon) can migrate the database on startup, while an
ordinary worktree CLI command cannot. Stop the old host and daemon, rebuild,
then start the pinned controller to upgrade. See [[configuration]] for the
migration authority settings.

An older binary refuses to use a database written by a newer build. The error
includes these lines (with the schema versions filled in):

> database schema v{on_disk} was written by a newer thegn than this build
> (v{build}).

> Refusing to run: this build cannot see the columns the newer one writes, so
> it would mis-read live state (a pipeline roster, a merge queue) and act on
> the gap.

A running host shows a persistent `DB REFUSED` banner. The environment variable
`THEGN_ALLOW_SCHEMA_DOWNGRADE=1` is a read-only debugging override; it is not a
downgrade path, and data written by the newer build remains invisible to the
older one.

## Upgrade by install channel

### Release archive

Download the GNU or musl archive for Linux x86_64 and its `.sha256` sidecar.
Verify both the checksum and the build attestation before installing:

```sh
sha256sum -c thegn-<tag>-<target>.sha256
gh attestation verify thegn-<tag>-<target>.tar.gz --repo blakeashleyjr/thegn
tar -xzf thegn-<tag>-<target>.tar.gz thegn
mkdir -p "$HOME/.local/bin"
if [ -x "$HOME/.local/bin/thegn" ]; then
  cp "$HOME/.local/bin/thegn" "$HOME/.local/bin/thegn.prev"
fi
install -m 755 thegn "$HOME/.local/bin/thegn"
```

The archive contains `thegn` at its root. The commands above keep a copy of
the previous binary when one exists.

### Nix

For a binary installed in a Nix profile, run `nix profile upgrade` for the
thegn entry. To upgrade a Home Manager install, update the thegn flake input
and rebuild your Home Manager configuration. `nix profile rollback` returns a
profile to its previous generation; this is Nix's profile history, not a
thegn rollback command.

### Source checkout

From the source checkout used for installation, update it and run the
installer again:

```sh
git pull
./install.sh
```

The installer replaces its installed files and records them in its install
manifest. `install.sh` is not included in the release archive.

### Finish the upgrade

Start thegn once so the controller can migrate the database, then run
`thegn doctor` to see the build and on-disk schema versions. Restart the daemon
from the same build as the CLI: quit the host, run `thegn daemon stop`, then
launch the new build. See [[debugging]] for doctor details.

## Rolling back

A previous binary is safe to use only with a database that the newer build has
not opened. Before upgrading, back up the state database using the procedure
below. To roll back, quit thegn, run `thegn daemon stop`, reinstall the older
binary, and restore that backup.

If the newer build has already migrated the database, the older binary refuses
to open it. In that case, continue forward with a compatible build or restore
a backup made before the upgrade. Never edit SQLite's `user_version` by hand.

## Back up and restore the state database

The database is the SQLite file `$XDG_STATE_HOME/thegn/thegn.db`. It uses WAL
mode; while a process has it open, `thegn.db-wal` and `thegn.db-shm` may also
exist. Git remains the source of truth for worktrees, and the forge remains the
source of truth for pull requests. The database is a cache and resurrection
layer, so losing it can cost saved layout and session state, notes, and cached
data, but not your checked-out code.

The database can contain live bearer credentials. Keep its backup private.

### Back up

1. Quit thegn and run `thegn daemon stop`.
2. Copy `thegn.db` and any `thegn.db-wal` and `thegn.db-shm` files that exist
   together to a private backup directory. Copy all of the existing files as
   one set; copying the main database by itself while a WAL exists can lose
   recent commits.
3. Restrict access to the backup files (`chmod 600`) and keep the directory
   private.

### Restore

1. Quit thegn and run `thegn daemon stop`.
2. Remove any existing `thegn.db-wal` and `thegn.db-shm` next to the live
   database, so they cannot be applied to the restored file.
3. Copy the backup's `thegn.db` over the live file. If the backup includes a
   `thegn.db-wal`, copy it beside the restored database too.
4. Start thegn. A database from an older build migrates forward when the
   controller starts; a newer database is refused by an older binary.

## Uninstall by install channel

### Source checkout

From the source checkout, run `./install.sh --uninstall`. Add `--dry-run` to
preview what it would remove, and pass the same bindir used for installation
if it was not the default `~/.local/bin`:

```sh
./install.sh --uninstall
./install.sh --uninstall --dry-run "$HOME/.local/bin"
```

The installer removes only manifest-recorded files whose SHA-256 still
matches. It keeps and reports changed files, never removes your config, and
refuses a malformed or forged manifest. Uninstall cannot be combined with
`--batteries`. If any file is kept, it reports `uninstall incomplete` and exits
non-zero. Review that report and remove any files you want gone yourself.

### Release archive

There is no install manifest for a release archive. Delete the `thegn` binary
you installed and any `tg` alias you created.

### Nix

Use `nix profile list` to find a profile-installed thegn entry, then remove
that entry with `nix profile remove`. For Home Manager, remove `programs.thegn`
from your configuration and rebuild it.

## What uninstall keeps

Uninstalling the binary does not remove your config, state, or worktrees:

- `~/.config/thegn` — config.
- `~/.local/state/thegn` — state database, logs, and crash reports.
- `~/.thegn` — the default worktrees directory (`~/.thegn/worktrees`) and
  managed tools.

Your config and state may be in different locations if you set the XDG paths.
The worktrees directory contains your branches' checked-out files; inspect it
before deleting it. After stopping the daemon and reviewing the contents, you
can remove any of these directories yourself if you want that data gone. Use
the actual configured XDG paths when they differ from the defaults. Run
`thegn daemon stop` before removing saved state so no pane daemon is left
running.

## Where to get help

Run `thegn doctor` for local diagnostics, or `thegn doctor bundle` to create a
support archive. See [[debugging]]. The Privacy and Troubleshooting help pages
also cover related topics.
