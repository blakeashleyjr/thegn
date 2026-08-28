# THE-76 chunk 2 — done: `dispatch verify`, `dispatch wait`, gated `done`

Commit: `feat(dispatch): verify + wait verbs and a gated set-status done (THE-76)`
Branch: `tg/the-76-pipeline-v2`. Chunk spec: `.thegn/pipeline/THE-76/code/chunk-2.md`.

## What landed

Exactly the five files the spec allows:

1. **`crates/thegn-core/src/control.rs`** — `Verb::DispatchesVerify` +
   `Verb::DispatchesWait` (beside the other `Dispatches*` verbs), both in the
   `ALL_VERBS` enumeration list, both in the **read** fold of
   `required_scope` (beside `DispatchesList`), both in the exhaustive
   read-scope test array of `verb_scope_table_is_exhaustive_and_least_privilege`.
2. **`crates/thegn-core/src/capability.rs`** — catalog rows
   `dispatches.verify` / `dispatches.wait` beside `dispatches.set_status`,
   both `SurfaceSet::of(&[Surface::Cli])` (the `search.query` shape). The
   comment states why there is no route and that a CLI-only row is a narrowed
   `surfaces` set, **not** a `SURFACE_GAPS` excuse — `ratchet_pins_surface_gaps`
   passes with no new line (verified by the scoped `capability` run).
3. **`crates/thegn-host/src/cmd/session.rs`** — only `cli_control_caps()`:
   one `v.extend(["dispatches.verify", "dispatches.wait"])` block with a
   comment in the house style. `cli_control_verbs_cover_catalog` is green;
   `SessionAction`, `run_async`, `session_line` untouched.
4. **`crates/thegn-host/src/cmd/dispatch.rs`** — the verbs + the gate:
   - `Action::Verify { id, json }` and `Action::Wait { row, any, timeout, json }`
     (`any` has `conflicts_with = "row"`; with neither flag the behaviour is
     `--any`). `Action::SetStatus` gains `force: bool`.
   - `verify_facts(row) -> pipeline_run::VerifyFacts` — the single
     implementation shared by `verify` and the gate. Blank/absent artifact ⇒
     all-false facts and **no git subprocess**; otherwise
     `exists = wt.join(a).is_file()`, `tracked = git_ok(ls-files
--error-unmatch)`, `dirty = git_out(status --porcelain).is_some()`.
   - `done_gate(row)` — runs `verify_facts` → `pipeline_run::verify_report`;
     on refusal bails with the reasons plus the
     `run 'thegn dispatch verify N' … or '--force'` pointer. Called from
     `set_status` only when `parsed == Done && !force` (the only-gated-status
     rule is written as a comment in the code). A row with no artifact passes
     by construction; dirty is reported, never blocking.
   - `verify(id, json)` — prints the report (human: one fact line
     `dispatch N  artifact …  exists=… tracked=… dirty=…  ok=…` + reason
     lines; `--json`: one flat document, the serialized report with `id`
     spliced in), then exits `EXIT_RETRYABLE` (2) when `!ok`, in both modes,
     after printing. Unknown id bails exactly like `set_status`.
   - `wait` — roster read + `wait_candidates` selection stays synchronous and
     **before** `connect()`, so a selection error (nothing active, unknown
     row) is answerable without a daemon. Then a tokio runtime `block_on`s
     `wait_wake`, which connects via `crate::cmd::session::connect` (no-daemon
     JSON degradation `{"error":"no_daemon"}` matches the other session
     verbs), spawns one `client.wait(sid, {"kind":"exited"}, timeout)` per
     target into a `tokio::task::JoinSet` (client behind an `Arc`), and joins:
     the first wake (matched, or daemon-Err ⇒ `gone:true`/`exit_code:null`)
     wins and the drop of the set cancels the rest; a `matched:false`
     completion is not a wake, so the loop keeps listening until every target
     has answered — only then does it print the timeout and exit 2. Wake
     output: human `dispatch N (stage) exited <code>` /
     `dispatch N (stage) session is gone`; JSON flat
     `{row, session, stage, issue, exit_code, matched[, gone]}`; timeout
     `{"matched":false}` / `timeout waiting on the dispatch wake` + exit 2.
   - 4 new unit tests in `mod tests` (reusing the isolated-`Db` harness plus a
     new `tempfile`-git-repo harness): facts for a no-artifact row (git
     skipped), the missing/untracked/committed/dirty fact matrix, the gate
     refusing missing + untracked and passing committed + dirty-tree, and the
     no-artifact pass-by-construction.
5. **`test/smoke.sh`** — the dispatch block extended with 10 `check`s covering
   every bullet in spec §5 (wait --any nothing-active without a daemon and
   provably not reaching the no-daemon message, wait --row unknown id naming
   it, verify ok/exit-0 on a no-artifact row, verify exit-2 naming a missing
   artifact, set-status done refused (missing) + `--force` visible as
   `forced`, untracked-artifact refusal, gate passing once committed). All
   daemon-free, same isolated `XDG_STATE_HOME`, same `check` style.

## Verification performed

- `just quick thegn-core` — clean.
- `just quick thegn-host` — clean (after the fixes below).
- `cargo nextest run -p thegn-core capability` — 18/18 (includes
  `ratchet_pins_surface_gaps`).
- `cargo nextest run -p thegn-core control` — 49/49 (verb scope table
  exhaustive/least-privilege).
- `cargo nextest run -p thegn-core capability control` after formatting — 66/66.
- `cargo nextest run -p thegn-host dispatch` — 24/24 (incl. the 4 new tests).
- `cargo nextest run -p thegn-host catalog_tests` —
  `cli_control_verbs_cover_catalog` green.
- `shellcheck test/smoke.sh` — clean (the repo's shellcheck gate equivalent;
  fixed one SC1010 it found by quoting the `done` literals).
- rustfmt (1.97.1, the flake's version) + shfmt run on the touched files;
  the diff is pure additions.
- **Behavioural replay against the built binary** in throwaway isolated
  `$HOME`/`$XDG_STATE_HOME`/`$XDG_RUNTIME_DIR` (not `just smoke`):
  - all 10 new smoke-section checks pass in isolation;
  - `verify --json` is flat + carries `id`, exits 2 on !ok and 0 on ok;
  - `set-status done --force --json` emits `"forced":true`;
  - `wait --any`/`--row` conflicts error under clap;
  - with a **live daemon**: tombstone-immediate wake (`exited 0`, ~30 ms),
    mid-wait exit with exit-code propagation (`exited 7` in ~3 s), `--any`
    first-exit-wins with the second target cancelled, timeout path (`{"matched":false}`,
    exit 2) both modes, and the gone path (`dispatch N (-) session is gone` /
    `{"gone":true,…,"exit_code":null}`) exercised via a session id the daemon
    never knew — the same daemon-Err branch a tombstone-TTL reap takes;
  - no daemon socket is created by any of the daemon-free paths.

## Unverified (for the review stage)

- **`just smoke` / e2e not run** (dev-loop policy: left to the pre-push gate).
  The new smoke section was validated only by shellcheck + the isolated
  behavioural replay above, which exercises the same commands and assertions.
- **A genuine tombstone-TTL expiry (10 min) as the trigger for the gone wake**
  was not waited out; the gone branch was verified via the equivalent
  daemon-error path (unknown session id).
- **`just test` / `just coverage` / `just ci` not run** (heavy gates are
  pre-push/CI-only per policy). Concretely un-run: the coverage gate on the
  new `dispatch.rs` lines (host crate is not coverage-gated; core changes are
  two enum variants + folds + catalog rows, and the exhaustive tests that pin
  them pass), MSRV/cross/feature checks, `test-doc`, openspec-validate (this
  chunk adds no openspec artifacts — chunk 1 owns them).
- **`treefmt` was run per-formatter directly** (rustfmt 1.97.1 + shfmt from
  the same nix store paths the flake pins) rather than via `treefmt`/`nix fmt`
  in a dev shell; alejandra/taplo do not apply to the touched files. The
  pre-commit hook will re-run the real treefmt.
- **Help pages**: no `docs/help/` change was made. The help ratchet pins
  `ACTION_SPECS` (TUI) action ids only; `dispatch verify|wait` are CLI verbs
  with no TUI action ids, and no help-page claims were altered. If the review
  wants the CLI verb surfaced in help prose anyway, that is additive.

## Notes for review

- `wait` cancels the losing targets by dropping the `JoinSet` after the first
  wake; on the timeout path it drains every completion first, so a target that
  exits a beat after another target's daemon-side timeout fired still wakes
  the supervisor rather than being lost — `matched:false` is treated as
  "keep listening", never as a terminal answer.
- `any` exists purely for clap's `conflicts_with` UX (explicit `--any` is
  documented as the default); the pattern match skips it with `..`.
- Human `ok=` was appended to `verify`'s fact line (spec's example shows the
  fact fields only) so the smoke check has a greppable verdict token; the
  spec's example remains a prefix of the printed line.
