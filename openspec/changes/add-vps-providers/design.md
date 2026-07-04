# Design: commodity-VPS execution backends

## Decision 1: native REST, not Pulumi/OpenTofu

The lifecycle is one POST (create with cloud-init user-data + ssh key + labels)
plus a status poll and a DELETE. Pulumi brings a subprocess CLI, a language
host, per-provider plugins, and a second state store that can disagree with
superzej's own (ledger + pool rows + worktree bindings) — and has no Rust SDK.
The codebase already has the exact pattern: pure request-shaping functions +
thin reqwest wrappers (`DaytonaProvider`, `SpritesProvider`), offline
mock-tested. `vps/hetzner.rs` follows it verbatim.

## Decision 2: the `szhost vps-ssh` self-bridge instead of new attach paths

VPS vendors have no exec CLI and no WSS PTY API. Rather than teaching
`panes.rs`/`run.rs` a third attach path, the placement's control/interactive
prefix defaults to `szhost vps-ssh {id} --`
(`EnvProviderConfig::control_command_template`). That makes a VPS env exactly a
"CLI provider" to every existing consumer: pane spawn, `GitLoc` control-plane
reads, the persisted worktree location, and the warm-pool claim rebind (which
now uses the same template). The bridge resolves the instance IP from the
ledger (API fallback with re-persist = stale-IP self-heal) and **exec**s a real
`ssh` with the managed key, `-tt` when a PTY is present.

## Decision 3: ssh shim constraints

- Secrets never touch an argv (host or remote `ps`): the remote command is
  `/bin/sh -s`; the script — env exports included — streams over stdin. File
  writes (which need stdin for data) put only _paths_ on the argv.
- `ControlMaster=auto` + `ControlPersist=90` so the pipeline's dozens of execs
  share one handshake.
- `tokio::process` + `kill_on_drop`: the pipeline's per-step
  `tokio::time::timeout` actually kills a hung ssh.
- `StrictHostKeyChecking=accept-new` against a per-instance known_hosts file
  (fresh VPS = fresh host key), deleted with the instance.

## Decision 4: file-based ledger, not a DB table

The leak-safety ledger (`$XDG_STATE/superzej/vps/<name>.json`) is written by
the provider itself — intent (state `creating`) _before_ the POST, finalized
(instance id + IP) after — so no call site can forget the bookkeeping, and the
CLI bridge (a separate process) reads the same files without opening the DB.
The DB stays what it is (a cache); pool rows and worktree bindings are
unchanged. The reaper reconciles: label-scoped (`managed-by=superzej` +
`sz-host=<machine-id-hash>`, so two hosts sharing an account never reap each
other), it destroys unledgered instances older than 20 min, drops stale
`creating`/ghost `ready` records, and enforces `max_lifetime_secs`.
`max_instances` (default 5) is enforced at create from the ledger count.

## Decision 5: no checkpoints, by construction

`ProviderCaps { files: true, checkpoints: false }`. The plan's checkpoint step
is now gated on the capability (`pc.auto_checkpoint && caps().checkpoints`);
pool spares record `checkpoint_id = None`, so both recycle paths (stale
reconcile, worktree-delete) fall through to **destroy** — the correct idle
policy when stopped instances bill. The speed analog is `superzej env
image-bake`: a throwaway instance runs the repo-independent provision prefix
(`envplan::bake_scripts` — nix + direnv; docker rides cloud-init), powers off,
snapshots, is destroyed, and the env's `template = "snapshot:<id>"` makes cold
provisions ~30–90 s instead of ~2–4 min.

## Alternatives rejected

- **`placement = "ssh"` against a hand-created VPS** — works today for a
  static box but has no lifecycle (create/destroy per worktree), no pool, no
  leak safety; documented as the zero-code escape hatch, not the product.
- **Vendor snapshots as pool-recycle checkpoints** — minutes-slow restores, new
  IPs, snapshot storage churn; destroy + baked-image re-provision is simpler
  and comparable in wall-clock.
