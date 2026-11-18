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
4. Update the stable instance with `just live` when you choose.

## Splitting one instance into two

To move projects you already have into the dev instance, give the dev instance
a copy of the stable database, then remove from each copy the projects that
belong to the other. `thegn repo forget` does the removal. It deletes database
rows only: files on disk, git worktrees and branches are never touched.

1. Quit both instances and stop their daemons (`thegn daemon stop`, and again
   after `eval "$(just devel-env)"`). `repo forget` refuses while the instance
   it targets is running, because a running instance would write the rows back.
2. Copy the stable database into the dev state dir with SQLite's backup, not
   `cp`: `sqlite3 ~/.local/state/thegn/thegn.db ".backup '$HOME/.thegn-devel/state/thegn/thegn.db'"`.
3. In the dev instance's environment, keep only the projects it owns. Run it
   once without `--apply` to see what would go:

   ```
   eval "$(just devel-env)"
   thegn repo forget --keep ~/code/thegn
   thegn repo forget --keep ~/code/thegn --apply
   ```

4. In a plain shell (the stable instance), forget those same projects:

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
  default ports when enabled. Only one instance can bind each.
- A repo's `.thegn.toml` overlay is read by whichever instance opens the repo.

## Which instance is this?

`thegn doctor` prints an instance block: run it in a pane to see the
binary, config dir, state dir, `THEGN_DIR`, channel and daemon socket that pane belongs to.

## CLI calls against the dev instance

`just devel-env` prints `export` lines for the dev instance and changes nothing:

```
eval "$(just devel-env)"
```

After that, `thegn` commands in that shell target the dev instance. See also
[[release-channels]] and [[upgrading-and-uninstalling]].
