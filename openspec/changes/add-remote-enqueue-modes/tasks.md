# Tasks

## 0. Prerequisite (landed separately)

- [x] 0.1 `resolve_worktree` ignores a `$THEGN_WORKTREE` that doesn't exist
      locally, so worktree-scoped commands work on a remote sprite (commit
      `0fc2ac66`).

## 1. Config

- [x] 1.1 `RemoteMode` enum (`route_to_host` default, `push`) + `merge_queue.
      remote_mode` field, default `RouteToHost`.
- [ ] 1.2 Document `remote_mode` in `config/config.toml.example`; keybindings /
      config-reference pages regenerate automatically.

## 2. Push mode (self-contained; testable on a single machine)

- [x] 2.1 In push mode, `drain`/`integrate` land the sprite's own clone even when
      the target reads off-host — bypass `remote_target_guard`
      (`cmd/integrate.rs`, `cmd/merge.rs::drain`).
- [x] 2.2 After a successful advance, `git push origin <target>`
      (`merge_ops::push_target`); push failure surfaces the reason and returns an
      error (no false success).
- [ ] 2.3 End-to-end test: a two-repo (origin + clone) fold→advance→push lands on
      origin. (Unit coverage in place; CLI e2e pending a binary build.)

## 3. Route-to-host — provisioning (NEEDS A SERVING HOST; not verifiable on one sprite)

- [ ] 3.1 Host mints a `MergeAdd`-scoped bearer token (pairing store) and injects
      `THEGN_CONTROL_URL` + `THEGN_CONTROL_TOKEN` into the sprite env alongside the
      proxy/iroh vars (`bouncer`/`iroh_home` injection sites). Requires the host in
      TCP **serve mode** (`thegn serve`) so there's an endpoint to inject.
- [ ] 3.2 The token is single-verb (`MergeAdd`) scoped and revocable; document it.

## 4. Route-to-host — client + routing

- [x] 4.1 `ControlClient::merge_add(worktree)` → `POST /v1/merge/add`
      (`thegn-svc control/client.rs`).
- [x] 4.2 `cmd::merge::add`: in `route_to_host` mode, when a sprite has
      `THEGN_CONTROL_URL`/`_TOKEN` injected, build `ControlAddr::Tcp` and send the
      **host-canonical** `$THEGN_WORKTREE`; failure surfaces, no local fallback
      ("queued <wt> on host"). Compile-verified; e2e needs a serving host.
- [ ] 4.3 Host `/v1/merge/add` enqueues for a **non-local** worktree (a true
      remote sprite): resolve branch + `location` from the host DB rather than
      requiring the worktree on the host FS (today's `enqueue_worktree` shells
      `main_checkout`/`branch_of` locally, so it only works when the worktree is
      on the host — e.g. a bind-mounted local sandbox).

## 5. Tests + validation

- [ ] 5.1 Unit: `RemoteMode` parse/alias/default; enqueue routing picks control
      path vs local by (mode, locality); `resolve_worktree` fallthrough.
- [ ] 5.2 Route-to-host end-to-end against a local serve-mode daemon (host on the
      same box, sprite path distinct) — enqueue lands in the daemon's DB.
- [ ] 5.3 `just ci` before opening the PR.
