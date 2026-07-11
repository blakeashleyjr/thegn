# Tasks

## 1. Persistence: daemon + lease + pairing registry (thegn-core)

- [x] 1.1 — Add `daemons`, `session_leases`, `pairings` tables + CRUD
      (`put_daemon`/`daemons`/`del_daemon`/`touch_daemon_heartbeat`,
      `put_lease`/`leases`/`refresh_lease`/`reap_expired_leases`,
      `put_pairing`/`pairings`/`revoke_pairing`); `user_version` bump —
      **unit tests** (CRUD round-trip, lease-expiry reap, token stored hashed,
      isolated `XDG_STATE_HOME`).

## 2. Daemon owns PTYs + warm-reattach (host)

- [x] 2.1 — Move `portable-pty` pane ownership into a long-lived daemon that
      registers in the `daemons` table and keeps `PaneEmulator` state alive after
      a client detaches — **unit tests** (pane survives detach; registry entry +
      heartbeat written off-loop).
- [x] 2.2 — Warm-reattach: a reattaching client receives the current emulator
      screen snapshot then a live delta stream; deltas reach the UI via mpsc +
      `TerminalWaker` (no loop timeout) — **unit tests** (snapshot equals live
      state; reattach maps to `Panes`, not chrome recompose, via `render_plan`).

## 3. Control API + CLI drives a running instance (svc + host)

- [x] 3.1 — Daemon control API over AK 445/451/452: attach/detach, list
      sessions, send-to-terminal, snapshot, drive-browser, with SSE/WebSocket
      event feed and scoped-token auth — **unit tests** (scope enforcement;
      event-frame fan-out; off-loop transport).
- [x] 3.2 — `thegn` verbs that talk to a running instance (open worktree,
      send-to-terminal, snapshot, drive-browser) as thin API callers extending
      the 454 headless CLI; graceful no-daemon fallback — **unit tests** (verb →
      API request mapping; no-daemon degradation).

## 4. `serve` + pairing-URL thin clients (svc + host)

- [x] 4.1 — `thegn serve` advertises a pairing URL; clients pair (token in
      `pairings`, hashed) and attach over the control API; pairing/approval
      surfaces as a chrome overlay (maps to `Full`) — **unit tests** (pairing
      issue/redeem/revoke; scope binding).

## 5. Persistent remote-session relay (svc + host)

- [x] 5.1 — On last-client detach, open a grace-period lease keeping the remote
      PTY warm; reconnect within the lease resumes the same emulator state;
      expiry reaps the PTY — lease bookkeeping is off-loop, never a polling timer
      — **unit tests** (resume-before-expiry warm; reap-after-expiry; no loop
      timeout introduced).

## 6. Mobile companion contract (svc)

- [x] 6.1 — Read-mostly companion surface over AK 451 + AI 422/423 push: monitor
      agents/activity, stage/commit via the GitBackend seam, switch
      accounts/scopes — all through the scoped control API with no AI hard-dep —
      **unit tests** (read paths require only read scope; stage/commit routes
      through GitBackend; account/scope switch).

## Validate

- [x] Run `just ci` — all gates green individually: fmt-check, lint (clippy
      `-D warnings` --all-targets + shellcheck/yamllint/taplo + git guardrail +
      ratchet), build, check-cross, test (nextest + doctests), doc-check,
      openspec-validate, coverage (core ≥95%, proxy ≥88%), smoke (incl. the new
      control-plane section), sandbox-e2e-dns/-db, nix-build. Two caveats, both
      environmental/pre-existing: `deps-audit` needs a writable advisory-db
      path in this sandbox (`~/.cargo` is read-only — passed via a db-path
      override; advisories/bans/licenses/sources all ok), and the muse `e2e`
      visual suite fails on goldens that predate this change (they render a
      different app UI dated Jun 19 and embed live clock/stats text).
