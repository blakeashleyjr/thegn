# THE-81 — Ignored-result allowlist audit: design

Audit and burn-down of `test/ignored-result-ratchet.txt` (325 pinned files,
2042 matching sites) per THE-77 checklist item 9 and the CLAUDE.md rule:

> **Ignored `Result`s must be deliberate.** … Anywhere the ignore isn't
> obviously one of those, add a short `// best-effort: <why>` comment — and
> never swallow errors on the primary path of a user-invoked action (surface
> those via `model.status`, `msg`, or `tracing`).

ARCHITECTURE.md §9 names the same list as the gate for the state invariant
("Ignored `Result`s on cache writes are the sanctioned best-effort pattern and
are marked `// best-effort:`"). The ratchet proves the _list_ only shrinks; it
does not prove anything about the sites inside the pinned files. This change
audits every site, annotates the sanctioned ones, surfaces the primary-path
ones, and shrinks the list by exactly the files whose sites are all handled.

## Method

Everything below was measured on this tree (`d361b60a`), not inferred:

- `git grep -InE 'let _ = |let _ =[[:space:]]*$|\.ok\(\);' -- crates` (the
  exact pattern `justfile:253/576` runs), minus ratchet.sh's comment-only-line
  filter → **2042 sites in 325 files**.
- Per-crate split: thegn-host 1144 sites / 183 files; thegn-core 603 / 84;
  thegn-svc 254 / 36; media 16, metrics 12, proxy 7, tg-kit 3, gtui-app 3
  (25 files).
- Shape triage (keyword heuristic over the 2042): ≈1122 sites sit in shapes the
  CLAUDE.md rule sanctions outright (waker pulses, channel sends, DB cache
  writes, file cleanup, terminal teardown) → mechanical annotation; ≈920 need
  a per-site read. The coders' rubric below is built to make that read fast.

## How the gate actually works (this drives everything)

`test/ratchet.sh ignored-result 'let _ = |let _ =[[:space:]]*$|\.ok\(\);' crates`
(justfile:576, in `just lint`; the `let _ =$` alternative catches the
rustfmt-wrapped swallow — see the justfile comment at :573-575):

1. It greps `crates/` for the pattern and **drops comment-only lines** — prose
   in a comment can never trip it.
2. A file is a "hit" if **any non-comment line** matches. A file leaves the
   allowlist only when **zero** non-comment lines match. `comm` then fails on
   new violations _and on stale entries_, so a released file must be deleted
   from the list in the same change or `just lint` goes red.
3. Therefore: **an annotation never releases a file.** Proof in-tree:
   `crates/thegn-core/src/heal.rs:29` carries
   `let _ = SCHEDULE.set(s); // best-effort: first-set-wins by design` and
   `heal.rs` is still pinned. Annotation is CLAUDE.md hygiene for ignores that
   _stay_; release requires the swallow itself to be gone.
4. The ratchet header's sentence "A file leaves this list when every ignore in
   it is either annotated that way or handled" is **wrong for the annotated
   branch** (see 3). Chunk 3 corrects that prose — it is the one hunk of the
   header any chunk may touch, and only chunk 3 touches it.
5. The pattern over-matches some non-ignores (`let name = x.ok();` binds and
   _uses_ the Option — 29 such sites; `let _ = cfg;` non-Result discards).
   **We keep the pattern anyway** (precedent `d4f3aeb9` fixed code, not the
   gate): the over-match is the price of also catching silent-fallback
   `.ok()`s (`let db = Db::open().ok();` — the error _is_ swallowed even
   though a value flows on), which are exactly the class this audit exists
   to surface. Bound-`.ok()` sites are classified like any other (below).

## Classification rubric (per site)

| Class                                         | Shape (with in-tree evidence)                                                                                                                                                                                                                                                                                                                                                                                              | Action                                                                                                                                                                |
| --------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **A1 waker pulse**                            | `TerminalWaker::wake()` / `wk.wake()` — `run.rs:326,874,1906,2171`; `cmd/attach.rs:142`                                                                                                                                                                                                                                                                                                                                    | annotate (a)                                                                                                                                                          |
| **A2 send to possibly-gone consumer**         | `tx.send(..)` / `blocking_send` off-loop — `run.rs:850,2170,2223`; `cmd/attach.rs:123-194`                                                                                                                                                                                                                                                                                                                                 | annotate (a)                                                                                                                                                          |
| **A3 DB cache/resurrection write**            | `db.put_* / del_* / set_* / update_* / persist / write_layout` where the DB is downstream of truth (git/forge) — `cmd/ci.rs:173,184`; `run.rs:1610,1674-1680`; `cmd/open.rs:64-65` (already annotated)                                                                                                                                                                                                                     | annotate (a)                                                                                                                                                          |
| **A4 cleanup / teardown**                     | `remove_file / remove_dir_all`, terminal teardown (`exit_alternate_screen`, `set_cooked_mode`, `flush`), tmp drops, child reaping — `run.rs:1100-1111,1893,1913`; `activity.rs:897` (already annotated); `account.rs:511,553,609`                                                                                                                                                                                          | annotate (a)                                                                                                                                                          |
| **A5 optional-input Option-shaping**          | `std::env::var(X).ok()` for an optional knob (absence is not an error); reading an optional file that legitimately may not exist — `cmd/mod.rs:110`; `daemon/mod.rs:64`; `hydrate.rs:1015`; `config.rs:5725-5793` _if_ `config validate` already reports the invalid value                                                                                                                                                 | annotate (a), why says "optional by contract"                                                                                                                         |
| **B1 silent-fallback binding**                | `let x = ….ok();` where the `None` path silently degrades a user-visible capability — `Db::open().ok()` before DB-backed reads: `cmd/doctor.rs:1432` (Hosts section renders "none" with no reason), `actions.rs:725-726` (`forge.conversation()/pr_diff()` → PR panel opens silently empty), `run.rs:1821` (delete-flow env resolution silently skips DB hosts — the comment right there explains it caused a sprite leak) | **handle**: surface (`tracing::warn` / `model.status` / `?` per idioms) **and rewrite the site so the pattern no longer matches**                                     |
| **B2 swallowed output on a CLI primary path** | the command's own result dropped — the class `cb775a7b` fixed (`cmd/search.rs` `emit_json` → `?`)                                                                                                                                                                                                                                                                                                                          | **handle**: propagate `?`, or annotate (a) if the swallow is genuinely benign (e.g. `cmd/diff.rs:89` stdout `write_all` — EPIPE on a closed `\| head` pipe is normal) |
| **B3 test-masking ignore**                    | a test's `let _ =` hides a setup failure that would make the assertion vacuous                                                                                                                                                                                                                                                                                                                                             | **(c)**: leave pinned, flag for reviewer                                                                                                                              |
| **C unsure**                                  | anything the coder can't pin to a class in one read                                                                                                                                                                                                                                                                                                                                                                        | leave pinned + untouched, list file:line + question in the done report                                                                                                |

Default bias: when torn between (a) and (b), choose (c). A wrong annotation
launders a real swallow; a wrong "fix" is a behaviour change.

## Handling idioms (what "surface" means, per context)

1. **CLI verb primary path, fn already returns `Result`** → propagate `?`
   (precedent: `cb775a7b`, `cmd/search.rs::print_query`). Keep cascades local:
   if threading the error past the immediate caller would cascade signatures
   deeper than one hop, use idiom 2 instead and say so in the done report.
2. **Background / off-loop worker** (spawn_blocking, watcher, daemon task) →
   `tracing::warn!(target: "thegn::<area>", error = %e, "<what> failed")`
   (precedents: `handlers/plugins.rs:383`, `handlers/adopt.rs:275`,
   `session.rs:124` `.inspect_err(\|e\| tracing::warn!(…))`). Never panic: a
   `unwrap`/`expect` takes down the compositor, which is strictly worse than
   the swallow.
3. **Loop-side user action** (handlers/) → `model.status` / `msg!` with a short
   reason (precedent: `handlers/attention.rs:336`), falling back to idiom 2
   when no model is at hand.
4. **Releasing a pinned file**: the site must stop matching the pattern —
   `let _ = f();` → handle via 1-3; `let x = f().ok();` →
   `match f() { Ok(x) => …, Err(e) => { warn…; … } }` or an `if let Ok` —
   **not** `.inspect_err(…).ok()`, which surfaces the error but still matches
   `\.ok\(\);` and keeps the pin. Non-Result discards (`let _ = cfg;`,
   `cmd/disk.rs:178`, `cmd/pr_queue.rs:152`) may become `drop(cfg);` when that
   alone releases a file; otherwise list them as pattern false-positives in
   the done report and leave pinned.

Annotation format: `// best-effort: <why>` — trailing on one-line lets, on the
line above otherwise. The `<why>` names the class (cache / waker / consumer /
cleanup / optional-input). Comments only: **zero code movement** for (a) sites.
Existing `// best-effort (see …)` prose that names its why (e.g.
`file_manager/yazi.rs:168-379`) satisfies the intent — do not mass-rewrite;
normalize the colon form only when already editing that line's vicinity.

## Containment (the issue's "no behaviour change beyond surfacing errors")

- Handling edits are limited to: the ignore site itself, the immediate
  receiver of the surfaced error (log line / status / `?`), and at most one
  hop of signature threading (idiom 1). Anything bigger → (c).
- No `unwrap`/`expect`/panic may be introduced anywhere, ever (0%-idle and
  crash-report invariants, ARCHITECTURE.md §2).
- No signature changes in `thegn-core` public API beyond one-hop `Result`
  propagation; `thegn-core` stays substrate-free; the 95%-line coverage gate
  (`just coverage`) is not run per-chunk but `-p thegn-core` tests must stay
  green because pre-push runs them.
- No test is rewritten to "handle" errors (tests stay simple): test-file sites
  are (a) annotated or (c) flagged, never (b).
- **Gate hygiene:** no chunk may edit `justfile` (the pattern stays as-is),
  `test/ratchet.sh`, or any allowlist other than deleting its own crate's
  lines from `test/ignored-result-ratchet.txt`. `RATCHET_UPDATE=1` on the
  ignored-result ratchet is **forbidden** during this audit (a whole-file
  rewrite from a parallel chunk's mid-state would resurrect or clobber other
  chunks' deletions). Release = surgical line deletion.

## Chunking (3 chunks, file-disjoint by crate)

| Chunk | Scope                                                          | Files | Sites | Tests (scoped)                                                                                                              |
| ----- | -------------------------------------------------------------- | ----- | ----- | --------------------------------------------------------------------------------------------------------------------------- |
| 1     | `crates/thegn-core/` (src + tests/)                            | 84    | 603   | `just quick thegn-core`; `cargo nextest run -p thegn-core`                                                                  |
| 2     | `crates/thegn-host/` (src + examples/)                         | 183   | 1144  | `just quick thegn-host`; `cargo nextest run -p thegn-host`                                                                  |
| 3     | `thegn-svc` + leaves (media, metrics, proxy, tg-kit, gtui-app) | 58    | 295   | `just quick thegn-svc`; `cargo nextest run -p` each of thegn-svc, thegn-media, thegn-metrics, thegn-proxy, gtui-app, tg-kit |

- **Independence:** fully file-disjoint; the Lead may run all three in
  parallel. The only shared file is `test/ignored-result-ratchet.txt`, where
  each chunk deletes only its own crate's lines — disjoint line ranges merge
  cleanly. Chunk 3 additionally edits the header prose (one hunk at the top;
  chunks 1-2 must not touch the header). No chunk depends on another's output.
- **Serial alternative:** if the Lead prefers serial execution, order 1 → 2 → 3
  (core first: its API is upstream of the other two; leaf crates last, with
  chunk 3 also finalizing the allowlist state).
- Verification per chunk (scoped, cheap): `bash test/ratchet.sh ignored-result
'let _ = |let _ =[[:space:]]*$|\.ok\(\);' crates/<crate>` must be **clean**
  after the chunk's deletions — it proves (i) every remaining pinned file in
  the crate still matches and (ii) every file the chunk released is off the
  list. The full-`crates/` run happens once at fold time (pre-push `just lint`).

## Expected outcome (honest sizing)

Annotation (a) dominates by count, and annotations do **not** release files —
so the list shrinks by exactly the files whose every site is handled or
rewritten. The mechanical-shape analysis predicts the bulk of B-class finds in
the ~920 needs-review sites, concentrated in the hot files each chunk lists.
The deliverable that matters is not the delta count: it is that **every
remaining site is either annotated with a why, or surfaced, or explicitly
listed as unsure for the reviewer** — which is what THE-77 item 9 found
missing. The ratchet header gains a corrected release rule so the next sweep
starts from true mechanics.

## Evidence appendix

- Pinned files: 325 (`grep -cE '^crates/' test/ignored-result-ratchet.txt`);
  byte-sorted _approximately_ (grouped, `db.rs` sits after `db_*.rs`) — hence
  deletions only, never insertions.
- Sites: 2042 total — 138 are `.ok();` (85 bare statements, 29 bound and used,
  24 other bindings), the rest `let _ =` (including the rustfmt-wrapped
  `let _ =$` form the justfile comment warns about).
- Prior art: `cb775a7b` (the last burn-down: `?`-propagation + signature-local
  handling across ~40 files, released `cmd/search.rs`), `d4f3aeb9` (false
  positive cleared by making code explicit, gate untouched), the THE-83 /
  THE-85 pin comments in the ratchet header (test-file annotation convention).
- Ratchet is clean at base: `bash test/ratchet.sh ignored-result …` →
  `clean (325 pinned)`.
