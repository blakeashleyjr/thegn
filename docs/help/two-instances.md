---
id: two-instances
title: Running two instances
order: 51
---

# Running two instances

You can run a **stable** thegn for your daily work and a **dev** thegn built
from a checkout, side by side, as one user on one machine. The dev instance
can be rebuilt and restarted freely without touching the stable one.

| Item        | stable                                                              | dev                                        |
| ----------- | ------------------------------------------------------------------- | ------------------------------------------ |
| Binary      | the installed binary (`~/.local/lib/thegn/thegn` after `just live`) | `target/debug/thegn` in the checkout       |
| Launch      | `thegn`, or `just live` to update it                                | `just devel`                               |
| Config dir  | `~/.config/thegn`                                                   | `~/.config/thegn-dev` (`THEGN_CONFIG_DIR`) |
| State       | the default state dir                                               | `~/.thegn-devel/state` (`XDG_STATE_HOME`)  |
| Runtime     | the default runtime dir                                             | `~/.thegn-devel/run` (`XDG_RUNTIME_DIR`)   |
| `THEGN_DIR` | `~/.thegn`                                                          | `~/.thegn-devel/home`                      |
| Channel     | stable                                                              | dev (`THEGN_CHANNEL=dev`)                  |

Each instance has its own daemon, database, worktrees and config.

`just start` and `just start-dev` remain throwaway instances with their own state but the shared config.

## Set it up

1. Seed the dev config dir once. `just devel` refuses to start without it and
   never copies anything for you: `cp -r ~/.config/thegn ~/.config/thegn-dev`.
   Review the copy before you keep it: it may hold secrets.
2. Run `just devel`. It builds the debug binary and starts the dev instance,
   restarting only the previous dev instance.
3. Register the thegn repo in the dev instance only, so you edit thegn from
   there.
4. Update the stable instance with `just live` when you choose. Run it from a
   plain shell, not from the dev instance's environment: `just live` refuses
   when `THEGN_CONFIG_DIR`, the state or runtime dir, or `THEGN_DIR` point at
   another instance, and the message names the path and how to fix it.
   `THEGN_CHANNEL=dev` alone does not make it refuse. It relaunches outside thegn
   only when the thegn ancestor runs this installation's binary.

## Launching the dev instance from a desktop entry

`just devel-desktop` prints a `.desktop` entry to stdout and writes nothing. Install it yourself:

```
just devel-desktop > ~/.local/share/applications/thegn-dev.desktop
```

The entry and `just devel` both go through `scripts/devel-launch.sh`, the single
source of the dev environment. Because the dev instance overrides
`XDG_RUNTIME_DIR`, the launcher first rewrites a relative `WAYLAND_DISPLAY` to
`<real runtime dir>/<name>`, and when `DBUS_SESSION_BUS_ADDRESS` is unset and
`<real runtime dir>/bus` is a socket it sets
`DBUS_SESSION_BUS_ADDRESS=unix:path=<real runtime dir>/bus`. Without these the
terminal cannot find the compositor or the session bus.

Caveat (unverified): rootless podman keeps its runroot under the runtime dir,
so containers started by the dev instance use a separate runroot from the stable
instance's.

## Splitting one instance into two

To move projects you already have into the dev instance, give the dev instance
a copy of the stable database, then remove from each copy the projects that
belong to the other. `thegn repo forget` does the removal. It deletes database
rows only: files on disk, git worktrees and branches are never touched.

1. Quit both instances and stop their daemons (`thegn daemon stop`, and again
   after `eval "$(just devel-env)"`). `repo forget` refuses while the instance
   it targets is running, because a running instance would write the rows back.
2. In the dev instance's environment, seed its empty state dir from the stable
   database and keep only the projects it owns. `--copy-from` replaces the old
   `sqlite3 .backup` step: it copies through SQLite's own backup (verified),
   then forgets. It refuses when the dev instance already has a `thegn.db`,
   when the schema versions differ, or when source and destination are the same
   file, and it never writes the source. Run it once without `--apply` to see
   what would go:

   ```
   eval "$(just devel-env)"
   thegn repo forget --copy-from ~/.local/state/thegn/thegn.db --keep ~/code/thegn
   thegn repo forget --copy-from ~/.local/state/thegn/thegn.db --keep ~/code/thegn --apply
   ```

3. (Already done by step 2.)

3. In a plain shell (the stable instance), forget those same projects:

   ```
   thegn repo forget ~/code/thegn
   thegn repo forget ~/code/thegn --apply
   ```

The dry run lists every project it would forget and keep, with what goes for
each: worktree rows, tab groups, pins, queue entries and similar. `--apply`
first writes a backup to `forget-backup-<UTC time>/thegn.db` in the state dir
and prints its path; copy it back over `thegn.db` (instance stopped) to undo.
Unfinished agent dispatches and autopilot runs are marked abandoned or stopped
rather than deleted, so their history stays. Repo trust decisions are kept, so
they follow the repo to whichever instance opens it. A `--keep` path that
matches no registered project is refused, because it would forget everything.
`--json` prints `{ "applied", "backup", "forgotten", "kept" }`.

## What is shared

These are not separated, and you should know it:

- The systemd user slice `thegn.slice` has one CPU and memory cap. The last
  instance to write it wins.
- The keyring service `thegn` is the same for both, so both read the same
  stored secrets.
- `thegn serve` (127.0.0.1:5380) and the model proxy (127.0.0.1:8383) use fixed
  default ports when enabled. Only one instance can bind each. Give the dev
  instance its own in `~/.config/thegn-dev/config.toml`:
  `[serve] bind = "127.0.0.1:5381"` and `[model_proxy] listen = "127.0.0.1:8384"`.
- A repo's `.thegn.toml` overlay is read by whichever instance opens the repo.

## Which instance is this?

`thegn doctor` prints an instance block: run it in a pane to see the
binary, config dir, state dir, `THEGN_DIR`, channel and daemon socket that pane belongs to.
The block also prints the runtime dir, the sandbox controller id (sandboxes this
instance created carry it), the addresses `thegn serve` and the model proxy
would bind, what is shared with other instances (the `thegn.slice` cap and the
keyring service), and a note when `WAYLAND_DISPLAY` is relative and not under
`XDG_RUNTIME_DIR`, which breaks `wl-copy` and `wl-paste`; start the instance
with `just devel`.

## CLI calls against the dev instance

`just devel-env` prints `export` lines for the dev instance and changes nothing:

```
eval "$(just devel-env)"
```

After that, `thegn` commands in that shell target the dev instance. See also
[[release-channels]] and [[upgrading-and-uninstalling]].
