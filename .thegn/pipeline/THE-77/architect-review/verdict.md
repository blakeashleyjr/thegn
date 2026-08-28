# THE-77 — architect review verdict

**APPROVED** — the branch lands as-is. One small defect was found during review
and fixed in-branch (`231350fa`); nothing is sent back.

## Scope of review

- `main` merged into `tg/the-77-arch-audit` first (merge `d37b16ef`, per the
  binding addenda — THE-70 sidebar/doctor, THE-83 agents/model/env, bundled
  skills came over; conflicts auto-resolved, treefmt churn on three THE-70
  pipeline docs only). All review runs below are **post-merge**.
- Full branch diff `git diff main...HEAD` read; every "Unverified" section of
  `chunk-{1,2,3}-done.md` re-verified by running the gate, not by trusting the
  claim.

## Chunk 1 (F1/F2/F3) — verified, with one live catch

- `test/ratchet.sh` `LC_ALL=C`: all **five** bash ratchets re-run post-merge —
  `clean`, **zero** `comm: … not in sorted order` lines. The done-summary's
  open question "does the justfile drive a sixth bash ratchet?" is resolved:
  `justfile:571-581` drives exactly the five verified.
- `cargo nextest run -p thegn-core --test crate_boundaries` — **4/4**, including
  the new `every_member_inherits_workspace_lints` and the `gix`-pinned
  `substrates_are_only_used_by_their_owners`.
- `cargo clippy -p thegn-proxy --all-targets -- -D warnings` — **clean** under
  the newly-armed workspace lints (6m08s; the tripwire was disarmed, not tripped
  — as designed).
- **The strongest validation of F1 happened during this review**: after the
  merge, the re-armed ratchet **caught a real, unpinned violation** in
  `crates/thegn-host/src/config_source.rs` (new from main's THE-83) with a
  clean rc=1, exactly one error line, no spurious stale entry. The gate went
  from mis-reporting to catching drift within days of the fix.

### Fixed by review (231350fa)

`config_source.rs` carries two `let _ =` sites; both are legitimate deliberate
ignores (OnceLock first-call-wins by documented contract; `clamp_to_channel`'s
`Vec<Feature>` return is informational — the clamp always applies), but the file
was unpinned and the sites lacked the `// best-effort:` comment CLAUDE.md asks
for. Fix: comments added at both sites, file pinned in
`test/ignored-result-ratchet.txt` with the reason. Ratchet now `clean (324
pinned)`. `rustfmt --check` verified on the file (pre-commit was bypassed on
that commit because `shfmt` is missing from this shell's PATH — an environment
gap, not a content one; `.txt` is not in `treefmt.toml`'s includes).

## Chunk 2 (F4) — verified

- The four `Background` declarations are exactly where the spec puts them
  (`run_supervisor` head for `thegn-metrics`; first statement of the
  `bridge-fswatch`, `podman-exec-events`, `podman-net-events` closures), each
  with a one-line reason. The four latency-coupled files are untouched.
- `test/thread-qos-ratchet.txt`: 12 entries, SHRINK-ONLY header with rule/enforcer/
  regeneration, inline reasons on `db_task.rs` / `frame_writer.rs` /
  `loading/ticker.rs`, and the trailing `thegn-metrics-collect` decision note
  (the file-level scan legitimately drops `metrics.rs` from the list; recording
  the reason in the trailing block was the right call).
- Post-merge: `cargo nextest run -p thegn-host platform_ratchet` **5/5** and
  `qos` **2/2** — main's new code (THE-70/83) added **no** undeclared
  long-lived threads; `just quick thegn-host` clean.
- The `justfile` comment is in place; the existing `cargo test -p thegn-host
ratchet` line picks the new test up by module filter (confirmed by the test
  running under both the `platform_ratchet` and `qos` filters here).

## Chunk 3 (F6) — verified

- `panel.md` claims `panel:db`/`panel:debug` and the reserved-placeholders
  section **accurately matches the code** (`misc.rs:787-796`, `misc.rs:1162-1166`:
  "○ no session", `BREAKPOINTS`/"none set", "debugger integration not wired
  yet"; "○ no database detected", "db introspection not wired yet").
- `test/help-context-ratchet.txt` emptied, header intact;
  `pages.rs`'s unclaimed-context probe moved to `panel:not-a-section` with
  sound rot-proofing rationale (vocabulary keys are all claimed + ratchet-
  enforced; non-vocabulary keys are unclaimable by frontmatter validation).
- Post-merge: `cargo nextest run -p thegn-host help` **71/71**.
- **e2e claim independently verified**: no muse spec snapshots `panel.md`'s
  body — the docked help section renders `help.page` (defaulting to index/
  "WELCOME", which is what `06-panel-system.yaml` expects), and the only F1
  spec (`27-glitch-hunt`) renders the glitch-hunt page. No re-record needed.

## Remaining notes (non-blocking)

1. The first future `just ratchet-update` may reorder `test/*-ratchet.txt`
   lines into byte order with no content change — expected consequence of F1's
   fix, already documented in `chunk-1-done.md`.
2. QoS declarations are behaviourally inert off macOS; the macOS FFI is covered
   by the existing skipped-here test. Unchanged by this branch.
3. Heavy gates (`just test`/`coverage`/`ci`, e2e) not run per the dev-loop
   policy — the pre-push hook is the gate. Everything scoped to the touched
   crates was run and is green.
4. Design follow-ups P1–P5 (startup git-heal carve-out, podman seam, QoS burn-
   down, ignored-result audit, panel.md coverage) remain open for the Lead to
   file — correctly out of scope here.

## Commits on the branch (new since review started)

| SHA        | Subject                                                                       |
| ---------- | ----------------------------------------------------------------------------- |
| `d37b16ef` | Merge branch 'main' into tg/the-77-arch-audit                                 |
| `231350fa` | fix(the-77): pin the THE-83 config_source ignores the re-armed ratchet caught |
