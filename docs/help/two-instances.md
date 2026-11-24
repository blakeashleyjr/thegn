---
id: two-instances
title: Running two instances
order: 51
---

# Running two instances

You can run a **stable** thegn for your daily work and a **dev** thegn built
from a checkout, side by side, as one user on one machine. The dev instance
can be rebuilt and restarted freely without touching the stable one.

| Item        | stable                                            | dev                                                                             |
| ----------- | ------------------------------------------------- | ------------------------------------------------------------------------------- |
| Binary      | `~/.local/lib/thegn/thegn` (`just update stable`) | `~/.thegn-devel/bin/thegn` (`just live`) or `target/debug/thegn` (`just devel`) |
| Launch      | `thegn` or the app launcher                       | `just live` or `just devel`                                                     |
| Config dir  | `~/.config/thegn`                                 | `~/.config/thegn-dev` (`THEGN_CONFIG_DIR`)                                      |
| State       | the default state dir                             | `~/.thegn-devel/state` (`XDG_STATE_HOME`)                                       |
| Runtime     | the login runtime dir (not overridden)            | the login runtime dir (not overridden)                                          |
| `THEGN_DIR` | `~/.thegn`                                        | `~/.thegn-devel/home`                                                           |
| Channel     | stable                                            | dev (`THEGN_CHANNEL=dev`)                                                       |

Each instance has its own daemon, database, worktrees and config.

`just start` and `just start-dev` remain throwaway instances with their own state but the shared config.

## Set it up

1. Run `just live`. It builds if the source changed, installs the binary to
   `~/.thegn-devel/bin/thegn` and starts the dev instance. On the first launch,
   with no `~/.config/thegn-dev`, it seeds that dir once: it copies only
   `config.toml` from stable and shifts the serve and model-proxy ports by +1.
   It never overwrites an existing dev config. Review the copy: it may hold
   secrets.
2. `just live-plan` shows what `just live` would do without building or
   starting anything.
3. The dev daemon is kept across `just live` by default. Pass
   `--restart-daemon` to end the running dev daemon and start a fresh one
   (this ends only the dev instance's daemon, never stable's).
4. Pass `--here` to build and run from the current checkout or worktree rather
   than the default source.
5. Register the thegn repo in the dev instance only, so you edit thegn from
   there.

`just live` never touches the stable installation. Its old flags
`--relink-launchers`, `--install-dir`, `--yes` and `--no-scope` are refused with
a message pointing at `just update stable`: stable is the binary at
`~/.local/lib/thegn/thegn`, installed from a verified release. `just devel` is the debug-build variant and runs `target/debug/thegn`.

## Updating an instance

`just update stable` and `just update dev` replace the binary, then restart that
instance's daemon: a newer database schema refuses to migrate while a process
still uses the old one (`refusing database schema migration ... another thegn
process is still using the old schema`). The command asks first ("This closes N
open panes"); `--yes` skips the question and `--no-launch` skips reopening
thegn. Run non-interactively without `--yes` it restarts nothing and prints the
commands. Re-running it also repairs an instance whose daemon still runs a
replaced binary, and `just update` (status) lists such daemons. Only processes of
that instance (same state dir and binary) are stopped. See [[just-recipes]].

## Launching the dev instance from a desktop entry

`just devel-desktop` prints a `.desktop` entry to stdout and writes nothing. Install it yourself:

```
just devel-desktop > ~/.local/share/applications/thegn-dev.desktop
```

The entry and `just devel` both go through `scripts/devel-launch.sh`, the single
source of the dev environment. The entry runs `~/.thegn-devel/bin/thegn`.

The dev instance does **not** override `XDG_RUNTIME_DIR`: sandbox masks follow the runtime directory in the
environment, so a private one would leave the dev daemon's socket unmasked or
hide the stable one. The dev daemon's socket lives under the login runtime dir,
`/run/user/<uid>/thegn/<hash of the dev state dir>/daemon.sock`, beside the
stable one, and both instances mask it. `~/.thegn-devel/run` remains only the
launcher's pidfile dir. Because the runtime dir is the login one, rootless
podman shares the login runroot, as it does for the stable instance.

The launcher also puts the dev binary's directory (`~/.thegn-devel/bin`) first
on `PATH` for the dev TUI, the dev daemon and `--print-env`, so a pane in the dev
instance finds the dev `thegn`. NixOS login shells reset `PATH`, so a pane's
shell may still resolve the stable `thegn`; run `$THEGN_BIN` there.
`thegn doctor` names that mismatch ("`thegn` on PATH is X, not this binary
(Y)") and a pane that belongs to the other instance.

The live scripts and the first-run net trace no longer override
`XDG_RUNTIME_DIR` either. Daemon sockets are hashed per state dir, so a fresh
state dir cannot collide with the real daemon. The e2e prelude and `demo.sh`
keep their override, with a comment saying why it is safe.

The launcher also scrubs pane identity from its environment before it execs the
dev binary, so a dev instance started from inside a thegn pane does not inherit
that pane's session. It removes `THEGN_SESSION_ID`, `THEGN_CONTROL_SOCKET`,
`THEGN_WORKTREE`, `THEGN_BRANCH`, `THEGN_WORKSPACE`, `THEGN_REPO_ROOT`,
`THEGN_SANDBOX`, `THEGN_SANDBOX_NETWORK`, `THEGN_SANDBOX_ENABLED`,
`THEGN_FORKED_FROM`, `THEGN_FORK_SCROLLBACK`, `THEGN_ROUTE_TOKEN`,
`THEGN_ROUTE_ORIGIN`, `THEGN_PROMPT`, `THEGN_EVENT` and `THEGN_BIN`;
`just devel-env` prints `unset` lines for them.

`scripts/devel-launch.sh --daemon` starts the dev daemon detached from that
scrubbed environment. It replaces the hand-run `setsid nohup <bin> daemon`.

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
  default ports when enabled. Only one instance can bind each. Give the dev
  instance its own in `~/.config/thegn-dev/config.toml`:
  `[serve] bind = "127.0.0.1:5381"` and `[model_proxy] listen = "127.0.0.1:8384"`
  (seeding does this for you on the first launch).
- A repo's `.thegn.toml` overlay is read by whichever instance opens the repo.

## Which instance is this?

The masthead shows the instance after the brand. When `THEGN_CONFIG_DIR` moves
the config dir away from `<XDG_CONFIG_HOME>/thegn`, it shows `[<basename of the
config dir>]`, so the dev instance shows `[thegn-dev]`. Nothing shows for the
default instance, under the e2e freeze or in tests.

`thegn doctor` prints an instance block: run it in a pane to see the
binary, config dir, state dir, `THEGN_DIR`, channel and daemon socket that pane belongs to.
The block also prints the runtime dir, the sandbox controller id (sandboxes this
instance created carry it), the addresses `thegn serve` and the model proxy
would bind, what is shared with other instances (the `thegn.slice` cap and the
keyring service), a "runtime exposure" row, a "share teardowns pending" row
(both read-only), and a note when `WAYLAND_DISPLAY` is relative and not under
`XDG_RUNTIME_DIR`, which breaks `wl-copy` and `wl-paste`; start the instance
with `just devel`.

## CLI calls against the dev instance

`just devel-env` prints `export` lines for the dev instance and changes nothing:

```
eval "$(just devel-env)"
```

After that, `thegn` commands in that shell target the dev instance. See also
[[release-channels]] and [[upgrading-and-uninstalling]].
