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
| Launch      | `thegn`, or `just live` to update it                                | `just dev`                                 |
| Config dir  | `~/.config/thegn`                                                   | `~/.config/thegn-dev` (`THEGN_CONFIG_DIR`) |
| State       | the default state dir                                               | `~/.thegn-dev/state` (`XDG_STATE_HOME`)    |
| Runtime     | the default runtime dir                                             | `~/.thegn-dev/run` (`XDG_RUNTIME_DIR`)     |
| `THEGN_DIR` | `~/.thegn`                                                          | `~/.thegn-dev/home`                        |
| Channel     | stable                                                              | dev (`THEGN_CHANNEL=dev`)                  |

Each instance has its own daemon, database, worktrees and config.

## Set it up

1. Seed the dev config dir once. `just dev` refuses to start without it and
   never copies anything for you: `cp -r ~/.config/thegn ~/.config/thegn-dev`.
   Review the copy before you keep it: it may hold secrets.
2. Run `just dev`. It builds the debug binary and starts the dev instance,
   restarting only the previous dev instance.
3. Register the thegn repo in the dev instance only, so you edit thegn from
   there.
4. Update the stable instance with `just live` when you choose.

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
state, runtime and config directories that pane belongs to.

## CLI calls against the dev instance

`just dev-env` prints `export` lines for the dev instance and changes nothing:

```
eval "$(just dev-env)"
```

After that, `thegn` commands in that shell target the dev instance. See also
[[release-channels]] and [[upgrading-and-uninstalling]].
