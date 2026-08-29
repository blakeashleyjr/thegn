# THE-81 chunk 3 — Audit every ignored Result in `thegn-svc` + the leaf crates; fix the ratchet header prose

**Read `.thegn/pipeline/THE-81/architect/design.md` first** — the classification
rubric, the handling idioms, and the containment rules there are binding for
this chunk. This file is the executable subset for thegn-svc and the leaf
crates (thegn-media, thegn-metrics, thegn-proxy, tg-kit, gtui-app), plus the
one allowlist-header correction only this chunk makes.

Goal: for every site in the pinned files below that matches
`let _ = `, `let _ =$` (rustfmt-wrapped), or `.ok();`, either (a) annotate the
sanctioned best-effort ones `// best-effort: <why>`, (b) surface the
primary-path ones (then delete the file's line from the allowlist), or (c)
leave untouched and list it for the reviewer.

## Files touched (exact)

Code scope — every pinned file under the five crates (58 files, 295 sites at
base `d361b60a`; the list below is the authority, taken from
`test/ignored-result-ratchet.txt`):

```
crates/gtui-app/src/app.rs
crates/gtui-app/src/engine.rs
crates/tg-kit/src/standalone.rs
crates/thegn-media/src/lib.rs
crates/thegn-media/src/mediaremote.rs
crates/thegn-media/src/mpd.rs
crates/thegn-media/src/smtc.rs
crates/thegn-metrics/src/battery.rs
crates/thegn-metrics/src/lib.rs
crates/thegn-proxy/src/lib.rs
crates/thegn-proxy/src/main.rs
crates/thegn-proxy/src/relay.rs
crates/thegn-proxy/src/router.rs
crates/thegn-svc/src/bin/fake_lsp.rs
crates/thegn-svc/src/bridge/mod.rs
crates/thegn-svc/src/calendar/tests.rs
crates/thegn-svc/src/ci.rs
crates/thegn-svc/src/control/client.rs
crates/thegn-svc/src/control/grpc.rs
crates/thegn-svc/src/control/http.rs
crates/thegn-svc/src/control/mod.rs
crates/thegn-svc/src/fly/mod.rs
crates/thegn-svc/src/git/branch.rs
crates/thegn-svc/src/git/mod.rs
crates/thegn-svc/src/git/patch.rs
crates/thegn-svc/src/git/undo.rs
crates/thegn-svc/src/host/cloud.rs
crates/thegn-svc/src/host/deliver.rs
crates/thegn-svc/src/host_discovery/mod.rs
crates/thegn-svc/src/host/mod.rs
crates/thegn-svc/src/host/retry.rs
crates/thegn-svc/src/ipc.rs
crates/thegn-svc/src/iroh_reach.rs
crates/thegn-svc/src/log/provider.rs
crates/thegn-svc/src/lsp/framing.rs
crates/thegn-svc/src/lsp/mod.rs
crates/thegn-svc/src/machine0/mcp.rs
crates/thegn-svc/src/machine0/mod.rs
crates/thegn-svc/src/plugin/proc.rs
crates/thegn-svc/src/plugin/session.rs
crates/thegn-svc/src/projection/mod.rs
crates/thegn-svc/src/provider.rs
crates/thegn-svc/src/revtunnel/mod.rs
crates/thegn-svc/src/seam/registry.rs
crates/thegn-svc/src/sessions.rs
crates/thegn-svc/src/share/mod.rs
crates/thegn-svc/src/share/tests.rs
crates/thegn-svc/src/usage.rs
crates/thegn-svc/src/vpn/mod.rs
crates/thegn-svc/src/vps/mod.rs
crates/thegn-svc/src/vps/registry.rs
crates/thegn-svc/src/vps/ssh_shim.rs
crates/thegn-svc/tests/fly_mock.rs
crates/thegn-svc/tests/machine0_live.rs
crates/thegn-svc/tests/sprites_live.rs
crates/thegn-svc/tests/sprites_mock.rs
crates/thegn-svc/tests/vps_do_mock.rs
crates/thegn-svc/tests/vps_mock.rs
```

Plus exactly three audit files:

- `test/ignored-result-ratchet.txt` — **delete the lines of files you
  released** (zero non-comment matches left) **and** fix the header prose
  (see Part 2 — only this chunk touches the header). Deletions only; never
  add a file line; never run `RATCHET_UPDATE=1`.
- `.thegn/pipeline/THE-81/code/chunk-3-done.md` — the report (format below).

## Overlap / dependency

**None — fully file-disjoint from chunks 1 and 2; the Lead may run all three
in parallel.** In the shared allowlist you delete only the leaf-crate lines
listed above; chunks 1/2 delete their own crates' lines (disjoint ranges,
merges cleanly). Your one extra hunk — the header prose correction — is at
the top of the file, a region chunks 1/2 are forbidden to touch, so the merge
stays clean. Do not edit `justfile`, `test/ratchet.sh`, or anything outside
the scopes above. If run serially, this chunk goes **last**.

## Part 1 — the per-site loop

Same loop as chunks 1-2. Evidence sampled at base (verified this turn):

| Hot file (sites)                                                                                                                    | Dominant shapes                                                                                                                                                                                                              | Provisional class                   | Watch for                                                                                                                                                            |
| ----------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `thegn-svc/src/bridge/mod.rs` (27)                                                                                                  | `tx.send(FsEvent/ProcEvent…)` to possibly-gone consumers (:582, :601, :623, :638, :654 — :654 is itself an _error report_ whose failure means nobody is listening), child `kill()`/`wait()` (:465-:466), OnceLock set (:496) | (a)                                 | the `kill`/`wait` pair: annotate "child already exited or reaped"                                                                                                    |
| `thegn-svc/src/host/mod.rs` (21), `git/mod.rs` (21)                                                                                 | subprocess teardown, probe writes, cache writes                                                                                                                                                                              | (a) mostly                          | a swallowed probe result that a _reported_ doctor surface depends on → (b)/(c)                                                                                       |
| `thegn-svc/src/control/*` (28 across 5 files)                                                                                       | response-channel sends, SSE/WS client drops                                                                                                                                                                                  | (a)                                 | a swallowed control-API write that the capability catalog reports as done → (b)/(c)                                                                                  |
| `thegn-svc/src/ipc.rs` (11), `revtunnel/mod.rs` (10), `vpn/mod.rs` (9), `lsp/mod.rs` (9), `host/deliver.rs` (9), `provider.rs` (13) | listener/conn teardown, sends, cleanup                                                                                                                                                                                       | (a) mostly                          | —                                                                                                                                                                    |
| `thegn-svc/tests/*.rs` (7 files)                                                                                                    | mock-server teardown, tmp dirs                                                                                                                                                                                               | (a) annotate                        | setup-masking ignores → (c)                                                                                                                                          |
| `thegn-media/src/smtc.rs` (9)                                                                                                       | `info.Controls().ok()` / `session.GetTimelineProperties().ok()` (:375, :396) — Windows SMTC state reads where absence is a normal condition                                                                                  | (a) — optional-input Option-shaping | —                                                                                                                                                                    |
| `thegn-media` rest (7), `thegn-metrics` (12), `thegn-proxy` (7), `tg-kit` (3), `gtui-app` (3)                                       | sampler/cache writes, teardown, awaited sends                                                                                                                                                                                | (a)                                 | `thegn-proxy`'s six sites are all `let _ = ….await;` mpsc sends (verified by THE-77 F3: `relay.rs:338,343`, `lib.rs:72,83`, `router.rs:290,466`) → (a) consumer-gone |

Release rule reminder: **an annotation never releases a file** — only sites
handled so the pattern no longer matches do (`match`/`if let Ok`, `?`, or
`drop(...)` for non-Result discards; **not** `.inspect_err(…).ok()`). After
handling a file's last matching site, delete its line from the allowlist.

## Part 2 — fix the ratchet header prose (this chunk only)

`test/ignored-result-ratchet.txt`'s header currently says:

> A file leaves this list when every ignore in it is either annotated that way
> or handled (surfaced via status/msg/tracing on a user-invoked path).

That is mechanically wrong for the annotated branch — the grep matches
`let _ = ` on the code line regardless of any comment (`heal.rs:29` is
annotated and still pinned; see design.md §"How the gate actually works").
Replace that sentence with the true rule, keeping the header a `#` block:

> A file leaves this list only when no non-comment line in it matches the
> pattern any more — every ignore handled or the swallow rewritten. A
> `// best-effort:` annotation is CLAUDE.md hygiene for an ignore that stays;
> it does not release the pin.

Touch nothing else in the header. This is the audit's gate-hygiene deliverable:
the next sweep must start from the real release rule.

## Tests to run (scoped; no full-workspace builds)

1. `just quick thegn-svc` — clippy on the one substantial crate here.
2. `cargo nextest run -p thegn-svc` (includes the svc platform-ratchet tests).
3. `cargo nextest run -p thegn-media`, `-p thegn-metrics`, `-p thegn-proxy`,
   `-p gtui-app`, `-p tg-kit` — leaf crates are tiny; no `just quick` needed
   for them (pre-push clippy covers the workspace).
4. `bash test/ratchet.sh ignored-result 'let _ = |let _ =[[:space:]]*$|\.ok\(\);' crates/thegn-svc crates/thegn-media crates/thegn-metrics crates/thegn-proxy crates/tg-kit crates/gtui-app`
   — must print `clean` with your deletions applied.

## Done-criteria

1. Every in-scope site is annotated (a), handled (b), or listed (c) — spot
   check: the same `git grep | grep -v 'best-effort'` as chunk 1, scoped to
   the five crates, returns only (b)-rewritten sites, (c) sites you list, and
   comment-only lines.
2. All test commands above green.
3. `test/ignored-result-ratchet.txt`: strictly fewer leaf-crate lines than at
   base, nothing added, and the header sentence replaced per Part 2.
4. `chunk-3-done.md` written with: a per-file table (file → sites → a/b/c
   counts → action taken), the **Unsure — for the reviewer** list (file:line +
   the question), and the ratchet command output snippet.
5. Commit the chunk with subject exactly:

   `fix(the-81): audit svc/leaf ignored-results — annotate best-effort, surface primary-path errors`
