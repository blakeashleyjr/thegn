# THE-76 chunk 2 — the run-completion contract: `dispatch verify`, `dispatch wait`, gated `done`

**Runs:** AFTER chunk 1 (needs `thegn_core::pipeline_run`).
**Overlap:** shares `crates/thegn-host/src/cmd/session.rs` and `test/smoke.sh`
with chunk 3 — **serial only, never in parallel with chunk 3.** Your edit to
`session.rs` is confined to the `cli_control_caps()` list at the bottom of the
file (`:505-561`); do not touch `SessionAction`, `run_async` or `session_line`.
**Read first:** `.thegn/pipeline/THE-76/architect/design.md` §2 D2/D3, §3 items 1
and 5.

## Files touched (exact)

1. `crates/thegn-core/src/control.rs` — two new `Verb`s + scope folds
2. `crates/thegn-core/src/capability.rs` — two new CLI-only catalog rows
3. `crates/thegn-host/src/cmd/session.rs` — **only** `cli_control_caps()`
4. `crates/thegn-host/src/cmd/dispatch.rs` — the two verbs + the gate
5. `test/smoke.sh` — dispatch coverage

## 1. Catalog (`control.rs` + `capability.rs`)

Two verbs, beside the existing dispatch ones (`control.rs:335-339`):

```rust
/// Report whether a roster row's handoff artifact is real — exists in the
/// worktree and is tracked by git. Observes only.
DispatchesVerify,
/// Block until an active roster row's session exits. Observes only.
DispatchesWait,
```

Add both to the `Verb` enumeration list (`control.rs:436-438` region) and to the
**read** scope fold (`control.rs:483`, where `DispatchesList` sits) — neither
mutates anything. Add both to the exhaustive verb test list at `control.rs:811`.

Catalog rows beside `dispatches.set_status` (`capability.rs:584-588`), following
the `search.query` precedent for a CLI-only row (`capability.rs:596-608` — read
its comment and write an equivalent one):

```rust
cap(
    "dispatches.verify",
    Verb::DispatchesVerify,
    SurfaceSet::of(&[Surface::Cli]),
    "Report whether a roster row's artifact exists and is tracked by git",
),
cap(
    "dispatches.wait",
    Verb::DispatchesWait,
    SurfaceSet::of(&[Surface::Cli]),
    "Block until an active roster row's session exits (the supervisor wake primitive)",
),
```

Both read the local worktree/roster (`verify`) or compose the already-routed
`sessions.wait` (`wait`), so neither wants an HTTP route. Narrowing `surfaces` is
how a CLI-only capability is expressed — **do not add a `SURFACE_GAPS` excuse**
(`docs/ARCHITECTURE.md` §6).

Then declare them on the CLI surface in `cmd/session.rs::cli_control_caps()`,
in a new `v.extend([...])` block with a comment in the style of the ones already
there (`:518-557`):

```rust
// Pipeline run-completion verbs (THE-76): local `thegn dispatch verify|wait`
// — the first reads the worktree + roster directly, the second composes the
// routed `sessions.wait`. Neither is a control route, so they cover the CLI
// surface here rather than through `API_CALLS`.
v.extend(["dispatches.verify", "dispatches.wait"]);
```

## 2. `thegn dispatch verify <row> [--json]`

New `Action::Verify { id: i64, json: bool }` in `cmd/dispatch.rs:24-70`.

Gather the facts for a row (a helper `fn verify_facts(row: &AgentDispatch) ->
VerifyFacts`, so both `verify` and the gate use one implementation):

| fact       | how                                                                                                                        |
| ---------- | -------------------------------------------------------------------------------------------------------------------------- |
| `artifact` | `row.artifact_path.clone()` (trimmed; blank ⇒ `None`)                                                                      |
| `exists`   | `Path::new(&row.worktree_path).join(a).is_file()`                                                                          |
| `tracked`  | `util::git_ok(&wt, &["ls-files", "--error-unmatch", "--", a])` (`util.rs:905-911`)                                         |
| `dirty`    | `util::git_out(&wt, &["status", "--porcelain"]).is_some()` — `git_out` returns `None` for empty output (`util.rs:770-777`) |

`false` for `exists`/`tracked` when there is no artifact; skip the git calls
entirely in that case (no subprocess for a row that isn't gated).

Then `pipeline_run::verify_report(&facts)`:

- `--json` ⇒ `super::emit_json(&report)` plus the row id (`{"id":N, …report}`).
  Build one `serde_json::Value` so the shape is flat and greppable.
- human ⇒ one line per fact + the reasons, e.g.

```
dispatch 12  artifact .thegn/pipeline/THE-76/code/12.md  exists=yes tracked=no dirty=yes
  - artifact ".thegn/pipeline/THE-76/code/12.md" exists but git does not track it — commit it
```

- **Exit code:** `0` when `report.ok`, else
  `std::process::exit(crate::cmd::EXIT_RETRYABLE)` (= 2), matching
  `session wait`'s "not yet" convention (`cmd/session.rs:391-393`,
  `cmd/mod.rs:70`). Print the report **before** exiting, in both modes.

Unknown row id ⇒ `anyhow::bail!` naming the id, exactly as `set_status` already
does (`dispatch.rs:167-169`).

## 3. The `set-status done` gate

Add `#[arg(long)] force: bool` to `Action::SetStatus` (`dispatch.rs:61-69`) and
extend `set_status` (`:155-176`):

- after the existing status parse and row lookup, when
  `parsed == AgentDispatchStatus::Done && !force`, run the same
  `verify_facts` → `verify_report` and, if `!ok`, `bail!` with the reasons plus
  the pointer:

```
dispatch 12 is not verifiably finished:
  - artifact "…" does not exist under the worktree
run `thegn dispatch verify 12` for detail, or `--force` to record it anyway
```

- **Only `done` is gated.** `failed`, `abandoned`, `merged`, and every active
  status stay unconditional — a supervisor must always be able to record a bad
  outcome. Write that as a comment; it is the rule a future edit will erode.
- A row with no `artifact_path` passes the gate by construction
  (`verify_report`'s first rule) — no special case here.
- When `--force` is used, say so in the output and include `"forced": true` in
  the JSON, so a forced completion is never invisible.

## 4. `thegn dispatch wait [--any | --row N] [--timeout <ms>] [--json]`

New `Action::Wait { row: Option<i64>, any: bool, timeout: Option<i64>, json: bool }`.
`--row` and `--any` are mutually exclusive (`#[arg(long, conflicts_with = "row")]`
on `any`); with neither, default to the `--any` behaviour.

This is the first `dispatch` verb that needs the daemon. Structure it like
`cmd/session.rs::run`: build a `tokio::runtime::Runtime` and `block_on` an async
body, using `crate::cmd::session::connect(cfg)` (`cmd/session.rs:195-210`, already
`pub(crate)`) so the no-daemon message and the `{"error":"no_daemon"}` JSON
degradation match every other daemon-touching verb. Keep the roster read (`Db`,
`list_dispatches`) synchronous and _before_ the connect — a selection error
should not require a running daemon.

1. `let rows = db.list_dispatches()?;`
2. `pipeline_run::wait_candidates(&rows, row)` — `bail!` with the error's
   `Display` on `Err`.
3. `connect(cfg).await?`
4. For each target, spawn `client.wait(&sid, serde_json::json!({"kind":"exited"}), timeout)`
   into a `tokio::task::JoinSet`. Take the first completion with
   `join_next().await`; drop the set to cancel the rest. Prefer `JoinSet` over a
   `futures` combinator — tokio is already the runtime here and the cancel-on-drop
   semantics are the ones we want. (`ControlClient` is cheap to clone/`Arc`; if it
   is not `Clone`, wrap it in an `Arc` for the tasks.)
5. Outcomes:
   - the daemon's `WaitOutcome` with `matched: true` ⇒ that row woke; report
     `exit_code` from the outcome.
   - a daemon **error** for one target (its session aged past the 10-minute
     tombstone TTL — `daemon/tombstone.rs:41`) ⇒ treat it as a wake with
     `"gone": true, "exit_code": null`. Do **not** fail the whole call: one
     reaped session must not make `--any` unusable.
   - every target reported `matched: false` (timeout) ⇒ timeout.
   - An already-dead session answers immediately from its tombstone
     (`daemon/service.rs:764-784`) — that is the wake a late-polling supervisor
     wants, not a bug.
6. Output — `--json`:
   `{"row":12,"session":"…","stage":"code","issue":"linear:THE-76","exit_code":0,"matched":true}`
   (add `"gone":true` in the reaped case); human:
   `dispatch 12 (code) exited 0` / `dispatch 12 (code) session is gone`.
   On timeout: `{"matched":false}` / `timeout waiting on 3 dispatch(es)` and
   `std::process::exit(crate::cmd::EXIT_RETRYABLE)`.

## 5. `test/smoke.sh`

Extend the existing dispatch block (`test/smoke.sh:1101-1122`) — same `check`
style, same isolated `XDG_STATE_HOME`:

- `dispatch verify` on a row with **no** artifact reports `ok` and exits 0.
- `dispatch verify` on a row whose artifact does not exist exits non-zero and
  names the artifact.
- `dispatch set-status <row-with-missing-artifact> done` fails, and the same
  call with `--force` succeeds.
- `dispatch set-status <row-with-no-artifact> done` succeeds ungated.
- `dispatch verify 999999` (unknown id) exits non-zero.
- `dispatch wait --any` with an empty roster exits non-zero with the
  nothing-active message and **without** needing a daemon.
- `dispatch wait --row 999999` exits non-zero naming the id.

Use `dispatch put … --artifact` to build the fixture rows (`put` already accepts
`--artifact`, `dispatch.rs:53-55`). Keep every check offline — no daemon is
started in this section.

## Tests to run (scoped)

```sh
just quick thegn-core
just quick thegn-host
cargo nextest run -p thegn-core capability
cargo nextest run -p thegn-core control
cargo nextest run -p thegn-host dispatch
cargo nextest run -p thegn-host catalog_tests
shellcheck test/smoke.sh
```

Do **not** run `just test`, `just ci`, `just coverage`, `just smoke`, or e2e, and
do not start any full-workspace compile. (`test/smoke.sh` needs a built binary;
leave running it to the pre-push gate — `shellcheck` is the check you owe here.)

## Done criteria

- [ ] `dispatches.verify` / `dispatches.wait` exist as `Verb`s, catalog rows
      (CLI-only surface), read-scope entries, and `cli_control_caps()` members;
      `cli_control_verbs_cover_catalog` is green and **no** `SURFACE_GAPS` line
      was added.
- [ ] `thegn dispatch verify` prints the report and exits 0/2 correctly, in both
      human and `--json` mode.
- [ ] `set-status … done` is refused for a row whose artifact is missing or
      untracked, passes for a row with no artifact, and `--force` overrides and
      is visible in the output.
- [ ] `dispatch wait --any` / `--row N` block on the daemon, wake on the first
      exit, survive a reaped session, and exit 2 on timeout.
- [ ] Unit tests in `cmd/dispatch.rs`'s `mod tests` cover the gate decision and
      the fact-gathering helper against a `tempfile` git repo (the module already
      has an isolated-`Db` harness at `:183-187` — reuse it).
- [ ] No new `let _ =` / `.ok()` without a `// best-effort:` reason.
- [ ] Scoped tests above are green.

**Commit subject (exact):**

```
feat(dispatch): verify + wait verbs and a gated set-status done (THE-76)
```

Also write your summary to the artifact path your roster row carries and commit
it in the same commit.
