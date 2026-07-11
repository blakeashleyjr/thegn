# Opt-in atuin shell-history sync for sandboxes/sprites

**Status:** approved 2026-06-29 · **Scope:** small, additive, opt-in

## Problem

A worktree running in a managed sandbox (sprite) gets a fresh `$HOME`, so the
user's [atuin](https://atuin.sh) shell history isn't there and `atuin` isn't
logged in — Ctrl-R is empty and history doesn't sync back. The host is already
logged into atuin (its own `auto_sync` works); we just need the sandbox to join
the same sync so history flows host ↔ sprite ↔ sprite.

## Decision

**Carry the host's existing atuin credentials + config into the sandbox** (opt-in)
and let **atuin's own sync** (`auto_sync`, default 5 min) reconcile history via the
sync server. thegn does NOT run `atuin login` (the host is already logged in)
and does NOT copy the history DBs (the server syncs those).

Rejected alternatives:

- _General "carry credential bundles" registry_ (`carry = ["atuin","gh",…]`) —
  more surface for a single consumer (YAGNI; revisit if gh/aws/etc. are wanted).
- _Full history-DB copy_ — heavy (~30 MB `records.db`), point-in-time, redundant
  once server sync runs.

## Config surface

`[sandbox.home]`, default `false` (zero behavior change unless set), per-env
overridable via `HomeOverlay` (same as `strategy`/`tools`):

```toml
[sandbox.home]
atuin = true   # carry host atuin creds+config into each sandbox so history syncs
```

## What is carried

Host → sandbox `$HOME`, only when `atuin = true` **and** the source exists:

| Host path                      | Sandbox path                       | Note                                                                                                                        |
| ------------------------------ | ---------------------------------- | --------------------------------------------------------------------------------------------------------------------------- |
| `~/.config/atuin/config.toml`  | `$HOME/.config/atuin/config.toml`  | read **dereferenced** (the home-manager `/nix/store` symlink would dangle), write real bytes                                |
| `~/.local/share/atuin/key`     | `$HOME/.local/share/atuin/key`     | E2E encryption key                                                                                                          |
| `~/.local/share/atuin/session` | `$HOME/.local/share/atuin/session` | legacy server-auth file, **if present** (modern atuin no longer writes it)                                                  |
| `~/.local/share/atuin/meta.db` | `$HOME/.local/share/atuin/meta.db` | **server session token** (`hub_session` bearer) — atuin ≥18 keeps it here, not in `session`; the actual auth carrier. ~28K. |

Explicitly NOT carried: `history.db`, `records.db`, `kv.db`, `scripts.db` —
the heavy stores the sync server reconciles. (Earlier revisions also excluded
`meta.db`, but that left the sandbox **logged out** since the session token
lives there in atuin ≥18; it is now carried.)

After carrying the creds, the `atuin_sync` step runs a best-effort one-shot
`atuin sync -f` in the sandbox to prime history into the (otherwise empty) record
store **before the checkpoint**, so Ctrl-R is populated the instant the pane opens
rather than waiting for the first `auto_sync` tick.

## Data flow / ordering

- `envplan::plan()` emits an `atuin_sync` step **only when `opts.atuin`** is set,
  ordered **after** `tools` (atuin must be installed) and with the other
  personal-layer uploads, **before** `checkpoint` (so it bakes into the snapshot).
- The step is a host-executed `StepKind::AtuinSync` (reads host files, dereferences
  the config symlink, uploads via the existing `provider.write` path used by
  `upload_dotfiles`/`upload_agent_configs`). Parent dirs are created on the sandbox.
- The generated `~/.zshrc` already runs `atuin init zsh`, so Ctrl-R lights up;
  `auto_sync` then reconciles history.

## Failure modes

Best-effort (the non-fatal provisioning-step behavior): a missing config/key,
atuin not installed, or an upload error **warns on the loading screen and
continues** — the shell still comes up. If only `key` (no `session`) is present,
carry what exists and note sync may be inert until the host is logged in.

## Tests

- core (pure): `atuin = true` ⇒ `atuin_sync` step present, ordered after `tools` &
  before `checkpoint`; `atuin = false` ⇒ absent; `HomeOverlay.atuin` per-env merge
  (present overrides, absent inherits).
- host: `upload_atuin_creds` dereferences a symlinked config + skips a missing
  `session` (temp `$HOME` fixture, mirrors the dotfile-resolver test).
