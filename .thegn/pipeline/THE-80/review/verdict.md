# THE-80 — security/test/bug review verdict

- **Branch:** `tg/the-80-qos-sweep` (reviewed at `4acc129a`)
- **Base:** `main` (`9715b74a`); binding merge already present (`54bed406`, `git rev-list --count HEAD..main` = 0 — re-verified, nothing to merge)
- **Role:** SECURITY/TEST/BUG review · **Lane docs read:** architect/design.md, chunk-1/2.md + done reports, architect-review/verdict.md (all "Unverified" items dispositioned below)

PASS

## 1. The distinctive risk surface — per-site class audit

The only way this lane can hurt is a demoted class on a thread that feeds a
frame, input, PTY drain, or the daemon control socket. Every touched spawn
site, its consumer, and its declared class:

| Thread (file)                                     | Consumer                                                                                                                                                              | Class        | Verdict                                                                   |
| ------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------ | ------------------------------------------------------------------------- |
| `desktop-notify-drain` (desktop_notify.rs:27)     | nobody — parks on `rx.recv()` dropping toasts when notify disabled                                                                                                    | `Background` | correct (pure housekeeping; parks, never spins)                           |
| `desktop-notify` (desktop_notify.rs:37)           | user notices toast; loop never blocks (off-loop by design)                                                                                                            | `Utility`    | correct                                                                   |
| `thegn-plugin-feed` (handlers/plugins.rs:730)     | forwards control event feed to a visible plugin                                                                                                                       | `Utility`    | correct                                                                   |
| `thegn-plugin-dispatch` (handlers/plugins.rs:819) | plugin `host.call` round-trips → visible content                                                                                                                      | `Utility`    | correct                                                                   |
| `mcp-up-{name}` (mcp_proxy/upstream.rs:89)        | deadline-enforced MCP tool calls (breaker-feeding transport); wired via `cmd/mcp.rs` proxy shim, **not** the daemon socket                                            | `Utility`    | correct (rubric refinement #2: deadline ⇒ Utility)                        |
| `notify-sound` (notify.rs:345)                    | audible cue, subprocess, off-loop                                                                                                                                     | `Utility`    | correct                                                                   |
| `thegn-plugins` (plugins.rs:100)                  | resident plugin contributions render as UI content                                                                                                                    | `Utility`    | correct                                                                   |
| `thegn-plugin-respawn` (plugins.rs:122)           | restores visibly-missing plugin content                                                                                                                               | `Utility`    | correct                                                                   |
| `thegn-plugin-once` (plugins.rs:158)              | user-invoked one-shot                                                                                                                                                 | `Utility`    | correct                                                                   |
| `tgshare` (share.rs:211)                          | user watching for share URL                                                                                                                                           | `Utility`    | correct                                                                   |
| `tgforward` (forward.rs:290)                      | `ForwardEvent`s on an unbounded channel + waker; port forwards land at the poll/backoff cadence — same shape as the proc/metrics samplers; no deadline, no sync await | `Background` | correct (cadence dominates; demotion yields later forwards, not failures) |
| `thegn-bench-window` (perf.rs:182)                | bench-only one-shot (armed solely by `THEGN_BENCH_RUN_MS`, called only from `run.rs:1033`); sets shutdown flag + single wake                                          | `Background` | correct (bench-only; verified the gate myself)                            |

**No `Background` anywhere near the frame, input, PTY-drain, or daemon-socket
path.** The event loop (`run.rs:391`) still declares `Interactive` — untouched.

## 2. Coverage — no missed or double-declared site

- **12 `thread::Builder::new()` sites = 12 `qos::set_self` hits**, 1:1, each
  the first statement of its thread body (verified line-adjacent in all 8
  files; no site declared twice, none in a helper outside the closure).
- **Independent crate-wide scan** (not the ratchet's own logic): every
  `*.rs` under `crates/thegn-host/src` containing a Builder site and no
  `set_self` → exactly `db_task.rs`, `frame_writer.rs`, `loading/ticker.rs`,
  `pane_writer.rs` — the 4 deliberate `Interactive` pins. Nothing else.
- The `platform/` exclusion hides nothing: **zero** Builder sites exist there.
- Each declaration is `set_self(qos)` (returns `()`, best-effort by contract —
  **no swallowed `Result` introduced**); the pre-existing `.ok()`s on spawn
  are unchanged sanctioned best-effort.

## 3. Ratchet list diff is exactly the touched sites

Old file (main): 12 entries → new: 4. Deleted: `desktop_notify.rs`,
`forward.rs`, `handlers/plugins.rs`, `mcp_proxy/upstream.rs`, `notify.rs`,
`perf.rs`, `plugins.rs`, `share.rs` — **exactly the 8 touched source files,
byte-for-byte, nothing else moved**. Survivors' reasons re-verified against
code: `db_task::flush` is awaited at `session.rs:820` (300 ms resurrect
barrier) and `main.rs:944` (2 s clean-exit drain); metrics supervisor
(`metrics.rs:90` declares `Background`) wraps the `recv_timeout` child at
`metrics.rs:242`. Header factual claims hold.

## 4. Tests run (scoped per policy)

- **Mandated:** `cargo nextest run -p thegn-host -E 'test(platform_ratchet) |
test(complete) | test(help) | test(catalog_tests)'` → **88/88 passed**
  (incl. `long_lived_threads_declare_a_qos_class`).
- Scoped behavior: `test(desktop_notify) | test(plugins) | test(share) |
test(notify) | test(forward) | test(perf) | test(mcp)` → **64/64 passed**.
- `cargo clippy -p thegn-host --tests` → clean.
- `rustfmt --edition 2024 --check` on all 8 touched files → clean.
- **Regen round-trip re-verified independently:** `THEGN_RATCHET_UPDATE=1`
  run → `diff` vs copy → **byte-identical**; working tree left clean.

## 5. e2e / frames

No frame-affecting change: no draw site, chrome, glyph, color, or poll-site is
touched; QoS is a compiled no-op off macOS. **No snapshot needs re-recording.**

## 6. Findings

None blocking. Residuals (inherited, non-defects):

1. **macOS effect unverifiable by construction** on this Linux box — the
   call-pattern matches the repo's existing precedents and is compile-checked;
   same residual the architect review accepted. Whoever next has Apple
   silicon should confirm `thread_policy_set` classes appear (e.g. via
   `powermetrics`/Instruments) — a follow-up note, not a gate.
2. The file-level ratchet's known blind spot (one undeclared thread in an
   otherwise-declared file passes silently) is closed for this lane by the
   12=12 count, which I verified independently (§2) — not by trusting the
   done-reports.
3. `mcp_proxy` threads are reachable via `thegn mcp proxy`/doctor; even if
   unreached in a given session, `Utility` is the safe direction (it cannot
   starve anything user-visible).

## 7. Gates still owed (standard, not review gaps)

Pre-push tier (`clippy` + `just test` + `just smoke`) at push; `just ci` once
at PR time. Neither blocks this verdict.
