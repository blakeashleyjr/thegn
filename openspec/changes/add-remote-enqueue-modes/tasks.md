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

- [ ] 2.1 In push mode, `drain`/`integrate` land the sprite's own clone even when
      the target reads off-host — bypass `remote_target_guard`.
- [ ] 2.2 After a successful advance, `git push origin <target>` (config the
      remote via existing git config; surface push failure via status, don't
      swallow it).
- [ ] 2.3 Tests: a two-repo (origin + clone) fold→advance→push lands on origin;
      push-failure defers with a reason.

## 3. Route-to-host — provisioning

- [ ] 3.1 Host mints a `MergeAdd`-scoped bearer token (pairing store) and injects
      `THEGN_CONTROL_URL` + `THEGN_CONTROL_TOKEN` into the sprite env alongside the
      proxy/iroh vars (Fly + OCI providers; `iroh_wire`/`bouncer` injection sites).
- [ ] 3.2 The token is single-verb scoped and revocable; document the surface.

## 4. Route-to-host — client + routing

- [ ] 4.1 `ControlClient::merge_add(worktree)` → `POST /v1/merge/add`, building
      `ControlAddr::Tcp{addr,token}` from `THEGN_CONTROL_URL`/`_TOKEN`.
- [ ] 4.2 `merge_ops`/`cmd::merge::add`: when the target is off-host and mode is
      route-to-host, route through the control client instead of the local DB;
      clear operator feedback ("queued on <host>").
- [ ] 4.3 Host `merge_add` records the caller's `location` on the row so the drain
      bundle-fetches the sprite's tip (pass/resolve the sprite `GitLoc`).

## 5. Tests + validation

- [ ] 5.1 Unit: `RemoteMode` parse/alias/default; enqueue routing picks control
      path vs local by (mode, locality); `resolve_worktree` fallthrough.
- [ ] 5.2 Route-to-host end-to-end against a local serve-mode daemon (host on the
      same box, sprite path distinct) — enqueue lands in the daemon's DB.
- [ ] 5.3 `just ci` before opening the PR.
