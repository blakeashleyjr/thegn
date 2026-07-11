# Native SSH-over-WSS shell for sprite (provider) environments

## Why

The sprite interactive pane attaches over the Sprites **WSS `/exec`** PTY API
(`crates/thegn-svc/src/provider.rs`). In practice it's laggy and drops input
characters — thegn hand-rolls the vt100-over-WebSocket relay, resize, and
flow control. Sprites expose **no direct SSH and no UDP** (so mosh is impossible),
but they _do_ expose a **raw-TCP-over-WebSocket proxy** (`/v1/sprites/{id}/proxy`):
after a JSON init it's a transparent TCP relay to any in-sprite `host:port`.
Sprites' own guidance is _"install an SSH server in the Sprite and tunnel through
the proxy."_

So the fix for the janky PTY is: run a real `sshd` inside the sprite and attach
the pane as a **local `ssh` client** whose transport is tunneled over the WSS
proxy. The pane becomes an ordinary local PTY running `ssh` — thegn's vt100
handles it natively (no custom WSS PTY relay), and ssh's mature PTY/flow-control
eliminates the lag. Bonus: unlocks `scp`/`sshfs`/agent-forwarding.

mosh stays available for real `[env] placement = "ssh"` boxes (already the default
transport) — this doc is only the sprite/provider path.

## Architecture

```
 pane (local PTY)            host                         sprite
 ┌──────────┐   stdio   ┌───────────────────┐  WSS    ┌──────────────┐
 │  ssh     │──────────▶│ thegn sprite-     │════════▶│ /proxy →     │
 │  client  │◀──────────│   proxy (relay)    │◀════════│ localhost:22 │
 └──────────┘           └───────────────────┘         │   sshd        │
   real ssh PTY,          ProxyCommand,                └──────────────┘
   resize, signals        raw TCP-over-WSS
```

1. **Provision (once, gated on `connect = "ssh"`):** install `openssh`, generate
   an sshd host key, write a minimal `sshd_config`, start `sshd` listening on
   `127.0.0.1:22` inside the sprite, and append a thegn-managed public key to
   `~/.ssh/authorized_keys`. Idempotent envplan steps; baked into the checkpoint.
2. **Transport:** a new provider method `open_proxy(id, host, port)` opens the
   WSS `/proxy` endpoint, sends `{"host","port"}`, and returns a raw byte stream
   (same channel shape as `ExecSession` minus the framing).
3. **`thegn sprite-proxy <worktree>` (hidden subcommand):** ssh's `ProxyCommand`.
   Resolves the env→sprite id, calls `open_proxy(id, "127.0.0.1", 22)`, and pumps
   host stdin↔stream↔host stdout until EOF. (Mirrors the resident-bridge stdio
   pump, but over `/proxy` instead of `/exec`.)
4. **Pane:** when `connect = "ssh"`, the interactive pane is a **local** PTY
   running `ssh -tt -o ProxyCommand="thegn sprite-proxy <wt>" -o
StrictHostKeyChecking=accept-new -o UserKnownHostsFile=<state> -i <managed
key> sprite@sprite -- 'cd <workdir>; exec <shell>'` — spawned via the normal
   `panes.spawn_argv_env` local path, not `spawn_native`.

## Config

`[env.<name>.provider] connect = "exec" | "ssh"` (default `exec` = today's WSS PTY).
Reuses the existing key-management dir under `$XDG_STATE_HOME/thegn/ssh/`.

## Build order

1. **Config knob** — `ProviderConnect` enum on `EnvProviderConfig`. (pure, tested)
2. **`/proxy` client** — `SpritesProvider::open_proxy` + a `Provider::open_proxy`
   dispatch; reuse the WSS handshake from `start_session`. Returns a duplex byte
   stream over channels. (tested with a mock/loopback)
3. **`sprite-proxy` subcommand** — stdio↔stream pump on a small runtime. (tested
   with a hand-built stream, like `relay_session`)
4. **Key mgmt** — generate/load an ed25519 keypair under XDG state. (tested)
5. **Provisioning** — `connect = "ssh"` adds the sshd/keys steps in
   `provision_provider_env` (or envplan). (script strings unit-tested)
6. **Pane wiring** — `spawn_worktree_shell_pane` builds the ssh argv when
   `connect = "ssh"`; spawned as a local PTY.

Steps 1–4 are pure/testable and land first. Steps 5–6 need **live sprite
iteration** (does the ssh handshake complete over `/proxy`, latency vs the exec
PTY) — verified by dogfooding, not unit tests.

## Non-goals / fallbacks

- No mosh to sprites (no UDP) — out of scope by construction.
- If `connect = "ssh"` but sshd/key setup fails, fall back to the exec PTY (warn).
- Host-parity dotfiles are orthogonal and compose (the ssh shell still sources
  the provisioned rc).
