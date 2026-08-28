# THE-77 — security/test/bug review verdict

PASS

## What this review did

- **`git merge main` first** (binding addendum): main had moved since the
  previous reviewer's merge — THE-85 (`tg/the-85-attach-live-session`, +2185
  lines: `worktree_attach.rs`, `pty_drain.rs`, `panes.rs`, `handlers/*`) landed
  after `a2174af7`. Merged as `e19e9983`, no conflicts. All checks below are
  **post-merge**.
- Read the full lane docs: `architect/design.md` (F1–F7, P1–P5),
  `architect-review/verdict.md` (incl. its accepted deviations), all three chunk
  specs and every **"Unverified"** section of `chunk-{1,2,3}-done.md` — each
  re-verified independently rather than trusted.
- Read the full branch diff `git diff main...HEAD` (20 files). Code surface is
  exactly: `test/ratchet.sh` (+6), `crate_boundaries.rs` (+32),
  `thegn-proxy/Cargo.toml` (+3), four `Qos::Background` declarations,
  `platform_ratchet_tests.rs` (+20), `panel.md`/`pages.rs` (+docs/2 asserts),
  `justfile` (+2 comment lines), and the two intended ratchet-list changes.
  Nothing else.

## Live catch — the re-armed gate fired on the THE-85 merge (fixed)

The THE-85 merge introduced `crates/thegn-host/src/handlers/worktree_attach.rs`
with four unpinned `let _ =` sites, and the re-armed `ignored-result` ratchet
failed `just lint`'s gate **rc=1** immediately post-merge. Same scenario as the
architect review's config*source.rs catch: main moves, the gate proves it is
armed. All four sites are inside `#[cfg(test)] mod tests` (scratch temp-dir
teardown ×2, stale-socket cleanup, and a `let * = stream;`value-ignore that is
the wedged-daemon decoy's whole job) — legitimate best-effort, none on a
primary path. Fixed per the config_source precedent:`// best-effort:`comments
at all four sites + file pinned with reason → ratchet`clean (325 pinned)`.
Committed as `f0e1ddc1`.

## Gate-fire proofs (addendum requirement: every re-armed gate must be shown to fail)

| Gate                              | Violation introduced (scratch, then reverted)                                                                                                                           | Observed failure                                                                                                                                                                                                                                                                                                                           |
| --------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `test/ratchet.sh` byte order      | removed the `config_resolve.rs` pin, **and** ran the pre-fix script (`acd06415^`) A/B against the fixed one under `LANG=en_US.UTF-8`                                    | Old: 6× `comm: not in sorted order`, the real violation **plus a false "new violation in config.rs" plus the self-contradictory "stale entry config.rs — delete it"**. New: exactly one error (the real violation), zero `comm:` noise, no stale line. F1's failure mode reproduced and confirmed dead. Allowlist restored byte-identical. |
| gix owner row                     | added `gix.workspace = true` to `thegn-proxy/Cargo.toml`                                                                                                                | `substrates_are_only_used_by_their_owners` panics: `"thegn-proxy depends directly on `gix`; only [\"thegn-svc\"] may"`. Reverse direction (removed gix from thegn-svc): `"thegn-svc is listed as an owner of `gix` but no longer depends on it"`.                                                                                          |
| proxy `[workspace.lints]` opt-in  | deleted the `[lints]` block from the proxy manifest                                                                                                                     | `every_member_inherits_workspace_lints` panics naming exactly `["crates/thegn-proxy/Cargo.toml"]` with the fix recipe.                                                                                                                                                                                                                     |
| thread-QoS ratchet (3 directions) | (a) deleted `set_self` from `bridge_sup.rs`; (b) deleted the still-violating `share.rs` allowlist line; (c) appended `run.rs` (declares `Interactive`, doesn't violate) | (a) test fails; (b) `new violation in ["share.rs"]`; (c) `stale entries ["run.rs"] — the list is shrink-only`.                                                                                                                                                                                                                             |
| emptied help-context ratchet      | appended `panel:merge` (a claimed context) to the emptied file                                                                                                          | `every_panel_context_has_a_documentation_page` panics: `"context(s) now claimed but still allowlisted: [\"panel:merge\"]"`.                                                                                                                                                                                                                |

All violations were made in scratch copies / temporary edits and reverted; the
working tree is clean and every allowlist is byte-identical to HEAD.

## Byte-identity of ratchet lists vs main

`git diff main...HEAD -- test/` touches exactly four files:
`ratchet.sh` (the fix), `thread-qos-ratchet.txt` (new, intended),
`help-context-ratchet.txt` (emptied, intended), `ignored-result-ratchet.txt`
(+3: my live-catch pin, intended). **Every other `test/*-ratchet.txt` is
byte-identical to main** (exclusion diff = 0 lines). The justfile drives exactly
the five bash ratchets (the chunk-1 open question is confirmed closed).

## QoS placement — no class on a latency path

Branch touches exactly three QoS sites: `bridge-fswatch`, `podman-exec-events`,
`podman-net-events` (all `Background`, first statement of the thread body, with
reasons) plus `metrics.rs`'s `run_supervisor` head. None is synchronously
awaited or renders. The four latency-coupled files the design exempts
(`loading/ticker.rs`, `db_task.rs`, `frame_writer.rs`, and the
`thegn-metrics-collect` deadline case) have **zero** `set_self` calls and are
untouched by the branch. `pty_drain.rs` has no QoS references at all; the
loop's declaration is `Interactive` (`run.rs:391`); `run.rs:6676`'s Background
is the pre-existing `crash-scan` scanner, correctly classified. Verified
`qos.rs`: Linux imp is a no-op, macOS failure is best-effort (no panic path) —
the declarations cannot regress anything off-macOS.

## Chunk claims re-verified (the "Unverified" checklists)

- **Chunk 1**: all five bash ratchets clean post-merge with zero `comm:` lines
  (also proves THE-85's new code is otherwise clean against them); the
  "sixth ratchet?" question resolved by reading `justfile:571-581` (five).
  macOS/BSD `comm` remains unexercised — acceptable: `LC_ALL=C` is
  locale-independent by construction and GNU/BSD `comm` semantics agree on
  sorted input.
- **Chunk 2**: the `metrics.rs` trailing-note deviation and the
  `file_ratchet` header-preservation caveat are documented in the allowlist
  itself. Post-merge, `platform_ratchet` 6/6 — THE-85's new code added no
  undeclared `Builder` threads.
- **Chunk 3**: the rot-proofing claim **verified in code** —
  `HelpRegistry::build` emits `ValidationError::UnknownContext` for any
  out-of-vocabulary `contexts:` entry (`registry.rs:174`) and
  `registry_builds_cleanly_from_the_shipped_pages` asserts the error list is
  empty, so `panel:not-a-section` is permanently unclaimable and the probe
  cannot rot. `panel.md`'s db/debug wording matches `misc.rs`'s rendered
  strings exactly ("○ no database detected" / "db introspection not wired
  yet" / "○ no session" / "BREAKPOINTS"/"none set" / "debugger integration not
  wired yet"). Help suite 73/73 post-merge.

## Scoped test results (all green; no heavy gates per dev-loop policy)

- 5× `bash test/ratchet.sh …` — clean, `ignored-result` 325 pinned.
- `cargo nextest run -p thegn-core --test crate_boundaries` — 4/4.
- `cargo nextest run -p thegn-host platform_ratchet qos` — 6/6.
- `cargo nextest run -p thegn-host help` — 73/73.
- `cargo clippy -p thegn-core --tests -- -D warnings` and
  `cargo clippy -p thegn-host --all-targets -- -D warnings` — clean **inside
  `nix develop`** (see toolchain note below).
- `cargo clippy -p thegn-proxy --all-targets` — not re-run by this reviewer;
  unchanged since the architect review's clean 6m08s run and no change since
  touches the proxy.

**e2e / frames:** this branch changes no rendering code (comments, allowlists,
tests, help text, a QoS no-op call). No muse spec snapshots the `panel.md`
body (re-confirmed via chunk-3's static analysis + the 73/73 help suite); no
snapshot re-recording is needed for the THE-77 diff.

## Non-blocking observations

1. **The QoS ratchet's predicate keys on `thread::Builder::new()`** — unnamed
   `std::thread::spawn` threads escape it (e.g. `handlers/provision.rs:113`,
   a one-shot user-initiated sandbox-start thread; predates this branch). The
   Builder heuristic is a defensible line (the crate has ~100 spawn sites and
   the unnamed ones are mostly one-shot), but it is a gap worth a follow-up:
   either widen the predicate with a large initial pin, or document the
   boundary in the allowlist header. The `worktree_attach.rs` spawn I inspected
   is test-only.
2. **Toolchain drift trap:** system-toolchain clippy 1.96 flags
   `nonminimal_bool` in pre-existing `crates/thegn-core/tests/hm_module_drift.rs`
   that the gate's flake-pinned clippy does not. Not a branch defect — but a
   reminder that any "clean under clippy" claim is only meaningful inside
   `nix develop`.
3. Design follow-ups P1–P5 (startup git-heal carve-out, podman seam, QoS
   burn-down of the 12 pinned files, ignored-result content audit,
   panel.md full coverage) remain correctly open for the Lead to file.
4. The first future `just ratchet-update` may reorder pre-F1 allowlist lines
   into byte order with no content change — expected and documented in
   `chunk-1-done.md` / the allowlist headers.

## Commits by this review

| SHA        | Subject                                                                                       |
| ---------- | --------------------------------------------------------------------------------------------- |
| `e19e9983` | Merge branch 'main' into tg/the-77-arch-audit (main had moved: THE-85)                        |
| `f0e1ddc1` | fix(the-77): pin the THE-85 worktree_attach test ignores the re-armed ratchet caught (review) |

Verdict: the branch is ready for the merge queue. `thegn integrate` is the
Lead's call.
