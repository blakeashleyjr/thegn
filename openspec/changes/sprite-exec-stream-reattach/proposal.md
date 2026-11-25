## Why

A live run on alpha.134 lost a three-hour `thegn sprite-exec --hold` after about
13 minutes: the exec stream ended without an exit status, the hold went with it,
and the sprite restarted under the user's build (THE-2710). The exec socket sent
a ping only at open and after typed input, so a quiet non-tty command left the
connection idle for minutes; the Sprites Go SDK pings every 15 s. The non-tty
bridge also had no reattach: one drop ended the run.

## What Changes

- Every Sprites exec socket sends a WebSocket ping every 15 s. A connection that
  has answered a ping and then sends nothing for 45 s is treated as dropped.
- Non-tty `sprite-exec` reattaches to the announced exec session after a drop,
  with the TTY path's backoff, resuming after the stdout and stderr bytes
  already received (`output_offset`). It never re-runs the command; stdin a
  dying session refused goes to the next one.
- `SpritesProvider::attach_exec_streaming` / `Provider::attach_exec_streaming`.
