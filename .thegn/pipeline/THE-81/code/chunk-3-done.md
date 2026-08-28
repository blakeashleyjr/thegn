# THE-81 chunk 3 — completion report (thegn-svc + leaf crates)

Branch: `tg/the-81-ignored-result-audit` · Base: `d361b60a` · Design:
`.thegn/pipeline/THE-81/architect/design.md` · Spec: `chunk-3.md`.

**Result: 295 sites in 58 files — 269 annotated (a), 26 rewritten (b), 0 left
unhandled. 4 files released from the allowlist. Ratchet header prose fixed.**

## Per-file table (file → sites → a/b/c → action)

| File | Sites | a | b | c | Action taken |
|---|---|---|---|---|---|
| `thegn-svc/src/bridge/mod.rs` | 27 | 25 | 2 | 0 | (a) fs/proc event sends, kill/wait teardown, socket writes, test cleanup; (b) failed exec-worker thread spawn now answers the host with `resp_err` instead of hanging the request; `let _ = saw_end` (bool) → `drop()` |
| `thegn-svc/src/git/mod.rs` | 21 | 21 | 0 | 0 | (a) pipe reads (truncate), kill/wait, branch -D cleanup, test fixtures |
| `thegn-svc/src/host/mod.rs` | 21 | 20 | 1 | 0 | (a) kill/wait, pipe reads, joins that must not mask errors, tx sends; (b) failed volume-seed rollback now emits `tracing::warn!`; `let _ = image` (ref discard) annotated |
| `thegn-svc/tests/sprites_mock.rs` | 14 | 14 | 0 | 0 | (a) mock-server socket writes/reads — client may disconnect |
| `thegn-svc/src/provider.rs` | 13 | 13 | 0 | 0 | (a) WS frame sends (peer gone), control close, `create_dir_all` whose failure surfaces via the write below, test cleanup |
| `thegn-svc/src/revtunnel/mod.rs` | 10 | 10 | 0 | 0 | (a) frame/control sends, flush/shutdown, pump copies |
| `thegn-svc/src/vpn/mod.rs` | 9 | 9 | 0 | 0 | (a) dereg/kill during teardown, perms (0700), stdout read |
| `thegn-svc/src/lsp/mod.rs` | 9 | 9 | 0 | 0 | (a) set-once caps OnceLock, pending-requester sends on shutdown, kill/wait, diagnostics send |
| `thegn-svc/src/ipc.rs` | 11 | 9 | 2 | 0 | (a) stale-socket cleanup, perms, pre-created pipe server (failure surfaces via next accept's `?`), test cleanup; (b) cfg-stub params `path`/`sock` → `drop()`; borrowed `name` params annotated (clippy drop_ref) |
| `thegn-svc/src/host/deliver.rs` | 9 | 9 | 0 | 0 | (a) tmp partial cleanup, rm -f restarts, watchdog join, stderr reads |
| `thegn-media/src/smtc.rs` | 9 | 9 | 0 | 0 | (a) Windows SMTC event-token removal, handler sends, `.ok()` Option-shaping on optional reads (absence is normal) |
| `thegn-svc/src/control/mod.rs` | 14 | 0 | 14 | 0 | (b) all sites were non-Result discards of unused params in trait-default fallbacks → underscore-prefixed param names (`_session`, `_req`, …); file RELEASED |
| `thegn-svc/src/share/mod.rs` | 7 | 7 | 0 | 0 | (a) kill/wait in `stop()`, URL-line sends, perms |
| `thegn-svc/src/host_discovery/mod.rs` | 7 | 7 | 0 | 0 | (a) partial read (fails JSON parse below), kill/wait/joins on timeout |
| `thegn-svc/src/control/http.rs` | 7 | 7 | 0 | 0 | (a) WS hello/error frames, `send_input`/`resize` (session may be gone; loop exits next frame), detach on exit |
| `thegn-metrics/src/battery.rs` | 7 | 7 | 0 | 0 | (a) test fixture setup/teardown |
| `thegn-svc/tests/sprites_live.rs` | 5 | 5 | 0 | 0 | (a) clean-slate destroy + tmp dir cleanup |
| `thegn-svc/src/vps/ssh_shim.rs` | 5 | 5 | 0 | 0 | (a) stdin feeder (ssh exit surfaces via `wait_with_output`), socket-dir perms |
| `thegn-svc/src/usage.rs` | 5 | 5 | 0 | 0 | (a) test tmp cleanup `.ok()` |
| `thegn-svc/src/control/client.rs` | 5 | 5 | 0 | 0 | (a) greeting burst on fresh channel, WS close, conn task |
| `thegn-metrics/src/lib.rs` | 5 | 5 | 0 | 0 | (a) `sample()` prime-the-CPU-delta calls in tests |
| `thegn-svc/src/plugin/session.rs` | 4 | 4 | 0 | 0 | (a) kill in `kill()`, test event sends |
| `thegn-svc/src/log/provider.rs` | 4 | 4 | 0 | 0 | (a) seek-to-end (failure risks one-time backfill only), test cleanup, join |
| `thegn-svc/tests/vps_mock.rs` / `vps_do_mock.rs` / `fly_mock.rs` | 3+3+3 | 9 | 0 | 0 | (a) mock-server socket writes/reads |
| `thegn-svc/src/sessions.rs` | 3 | 3 | 0 | 0 | (a) test tmp cleanup |
| `thegn-svc/src/iroh_reach.rs` | 3 | 3 | 0 | 0 | (a) registered/exit frame sends, stream finish |
| `tg-kit/src/standalone.rs` | 3 | 3 | 0 | 0 | (a) change-hook send (closed = shutdown), panic-hook terminal restore |
| `thegn-svc/src/vps/registry.rs` | 2 | 2 | 0 | 0 | (a) idempotent record/known_hosts removal |
| `thegn-svc/src/vps/mod.rs` | 2 | 2 | 0 | 0 | (a) registry cache write (resurrection layer), advisory cloud-init wait |
| `thegn-svc/src/share/tests.rs` | 2 | 2 | 0 | 0 | (a) test tmp setup/teardown |
| `thegn-svc/src/projection/mod.rs` | 2 | 2 | 0 | 0 | (a) mountpoint mkdir — failure surfaces via the mount error below |
| `thegn-svc/src/plugin/proc.rs` | 2 | 2 | 0 | 0 | (a) kill_group wait, Windows taskkill |
| `thegn-svc/src/host/retry.rs` | 2 | 2 | 0 | 0 | (a) ControlMaster exit + socket unlink |
| `thegn-svc/src/host/cloud.rs` | 2 | 2 | 0 | 0 | (a) read-timeout on dying stream, response write |
| `thegn-svc/src/git/undo.rs` | 2 | 2 | 0 | 0 | (a) stash-pop caller-owned, conflicting test merge |
| `thegn-svc/src/fly/mod.rs` | 2 | 2 | 0 | 0 | (a) socket write/flush — client gone |
| `thegn-svc/src/control/grpc.rs` | 2 | 2 | 0 | 0 | (a) stream sends — client gone |
| `thegn-svc/src/calendar/tests.rs` | 2 | 2 | 0 | 0 | (a) test tmp cleanup (setup + Drop) |
| `thegn-svc/src/bin/fake_lsp.rs` | 2 | 2 | 0 | 0 | (a) fake-server writes — peer gone |
| `thegn-proxy/src/router.rs` | 2 | 2 | 0 | 0 | (a) `let _ = try_acquire(...)` — non-Result bool discard, value deliberately unused (comment above each) |
| `thegn-proxy/src/relay.rs` | 2 | 2 | 0 | 0 | (a) mpsc sends — consumer gone |
| `thegn-proxy/src/lib.rs` | 2 | 2 | 0 | 0 | (a) ctrl-c wait — signal error just ends the wait |
| `thegn-proxy/src/main.rs` | 1 | 1 | 0 | 0 | (a) `try_init` fails only when a subscriber is already set |
| `gtui-app/src/app.rs` | 2 | 2 | 0 | 0 | (a) engine cmd sends — engine may be shutting down |
| `gtui-app/src/engine.rs` | 1 | 1 | 0 | 0 | (a) panel-update send — UI may be gone |
| `thegn-svc/tests/machine0_live.rs` | 1 | 1 | 0 | 0 | (a) ensure-teardown destroy |
| `thegn-svc/src/machine0/mod.rs` | 1 | 1 | 0 | 0 | (a) annotated |
| `thegn-svc/src/machine0/mcp.rs` | 1 | 1 | 0 | 0 | (a) fire-and-forget `initialized` notification |
| `thegn-svc/src/lsp/framing.rs` | 1 | 1 | 0 | 0 | (a) `parse().ok()` Option-shaping — parse failure maps to None (absent header) |
| `thegn-svc/src/git/patch.rs` | 1 | 1 | 0 | 0 | (a) rebase_abort on the error path |
| `thegn-svc/src/git/branch.rs` | 1 | 1 | 0 | 0 | (a) conflicting test merge by design |
| `thegn-media/src/mediaremote.rs` | 1 | 1 | 0 | 0 | (a) `start_kill` — child may have exited |
| `thegn-media/src/lib.rs` | 5 | 1 | 4 | 0 | (b) 4 unsupported-default trait methods → underscore params; (a) `let _ = opts` platform fallback |
| `thegn-media/src/mpd.rs` | 1 | 0 | 1 | 0 | (b) `let _ = dial(..)?` → `dial(..)?;` — error already propagated; file RELEASED |
| `thegn-svc/src/ci.rs` | 1 | 0 | 1 | 0 | (b) `scope` param unused (both scopes map to retry) → `_scope`; file RELEASED |
| `thegn-svc/src/seam/registry.rs` | 1 | 0 | 1 | 0 | (b) `Some(path) => { let _ = path; … }` → `Some(_)`; file RELEASED |
| **Total** | **295** | **269** | **26** | **0** | |

## Allowlist changes (`test/ignored-result-ratchet.txt`)

- Deleted 4 released lines: `thegn-media/src/mpd.rs`, `thegn-svc/src/ci.rs`,
  `thegn-svc/src/control/mod.rs`, `thegn-svc/src/seam/registry.rs`.
  Leaf-crate lines: 58 → 54. Nothing added. `RATCHET_UPDATE=1` never run.
- Header prose corrected per Part 2: "A file leaves this list only when no
  non-comment line in it matches the pattern any more — every ignore handled
  or the swallow rewritten. A `// best-effort:` annotation is CLAUDE.md
  hygiene for an ignore that stays; it does not release the pin."

## Ratchet verification

Full-`crates` run (fold-time gate) with all three chunks' deletions applied:

```
$ bash test/ratchet.sh ignored-result 'let _ = |let _ =[[:space:]]*$|\.ok\(\);' crates
ratchet(ignored-result): clean (315 pinned)      # 325 at base → 315
```

Spot check (done-criteria 1) — unannotated matches left in the six crates:

```
$ git grep -InE 'let _ = |let _ =[[:space:]]*$|\.ok\(\);' -- crates/thegn-svc … | \
    grep -vE '^[^:]+:[0-9]+:[[:space:]]*//' | grep -cv best-effort
0
```

## Tests run (scoped per dev-loop policy)

- `just quick thegn-svc` — clippy -D warnings: **pass** (after fixing two
  clippy findings my rewrites had introduced, see below).
- `just quick thegn-media|thegn-proxy|gtui-app|thegn-metrics|tg-kit` — **pass**.
- `cargo nextest run -p thegn-svc` — **593 passed, 11 skipped**.
- `cargo nextest run -p thegn-media -p thegn-metrics -p thegn-proxy -p gtui-app -p tg-kit` —
  **141 passed, 0 skipped**.
- Full ratchet over `crates` — **clean (315 pinned)**.

Clippy corrections worth noting: `drop(x)` on borrowed/Copy values trips
`clippy::drop_ref` — the `drop(image)` / `drop(name)` sites were converted to
annotated `let _ =` instead; the Copy-tuple drops I first wrote in
`control/mod.rs` / `thegn-media/src/lib.rs` were replaced wholesale by
underscore-param renames before ever compiling.

## Behavior changes (for the reviewer)

1. `thegn-svc/src/bridge/mod.rs` — when the exec/exec.batch/proc.list worker
   thread fails to spawn, the server now writes an error response frame to the
   host instead of leaving the request unanswered. Error path only.
2. `thegn-svc/src/host/mod.rs` — a failed `volume rm -f` rollback after a bad
   seed now emits `tracing::warn!` (the primary error is still returned).

## Unsure — for the reviewer

- `thegn-proxy/src/router.rs:290, :466` — non-Result discard of a bool from
  `try_acquire`; I tagged it `best-effort` for mechanical consistency, but the
  honest label is "deliberately unused value" (the block comment above each
  site already explains). No error is swallowed.
- `thegn-svc/src/lsp/framing.rs:100` — `parse::<usize>().ok()` is
  Option-shaping (absent/invalid header → None), not a swallowed error; kept
  and annotated rather than rewritten into a noisier `match`.
- `thegn-svc/src/machine0/mcp.rs:177` — the `notifications/initialized` ack is
  fire-and-forget; a failed send leaves the MCP server possibly un-acked with
  no signal. Annotated (a); a reviewer may prefer a `tracing::debug!`.
- `thegn-svc/src/vps/mod.rs:616` — the 240 s cloud-init wait is fully ignored:
  a wedged first boot goes unnoticed (create still returns Ok). Annotated (a)
  per the code comment; a reviewer may want the wait failure surfaced.
- `thegn-svc/src/control/http.rs` / `grpc.rs` `send_input`/`resize` — failures
  are invisible to the remote user (no backchannel); annotated (a) since the
  attach loop exits on the next frame, but worth a reviewer glance.
- `thegn-svc/src/log/provider.rs:54` — a failed seek-to-end would silently
  backfill history (the thing the seek avoids); annotated (a) as extremely
  unlikely; flagging in case the reviewer prefers an error.

## Unverified

- Full-workspace `just lint` / `just test` / e2e were NOT run (dev-loop policy;
  pre-push hook is the gate). The scoped equivalents above are green.
- Cross-platform cfg-gated code (`#[cfg(windows)]` SMTC/taskkill/DACL,
  `#[cfg(not(unix))]` stubs) typechecks only via clippy on this Linux host
  (`just quick` covers the cfg branches syntactically; no Windows/macOS run).
- The scoped ratchet command exactly as written in the chunk spec (pathspec =
  the six crates) reports the still-pinned `thegn-core`/`thegn-host` lines as
  stale, because the allowlist comparison is global while the grep is scoped —
  an artifact of the scoped invocation, not a violation. The full `crates` run
  is clean (315 pinned) and is the gate `just lint` actually runs.
