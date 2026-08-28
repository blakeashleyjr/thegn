# THE-80 architect review — verdict

- **Branch:** `tg/the-80-qos-sweep` (reviewed at `fac8dc52`)
- **Base:** `main` (`9715b74a`), after the binding merge (`54bed406`)
- **Reviewer:** architect stage (THE-80 lane)
- **Design:** `.thegn/pipeline/THE-80/architect/design.md` (chunks 1–2 +
  done reports read in full; every Unverified item dispositioned below)

## Verdict: **APPROVED**

No revision chunks. The one gap found was a pipeline-doc inaccuracy
(chunk-2-done.md's header-provenance claim), corrected here in `fac8dc52`
together with the same inconsistency in the chunk-2 spec itself. Code is
exactly per design; no behaviour change anywhere off macOS by construction.

## 1. The merge (LEAD addendum, first)

`git merge main` (→ `54bed406`) brought in main's 2 new commits (`tg/lint-main`:
treefmt drift, brand-guard now exempting `.thegn/pipeline/**`, THE-76 verdict
refresh). Clean merge — no overlap with this lane's files (the lane touches 8
source files + `test/thread-qos-ratchet.txt` + pipeline docs; main's move
touched docs and one shell script). Post-merge, `just quick thegn-host` is
clean and the mandated scoped suite is green (below).

## 2. Design conformance — verified, not taken on trust

The full branch diff (`git diff main...HEAD`) is 14 files, all in scope.

- **All 12 declarations, all 12 Builder sites.** The 8 declared files contain
  exactly 12 `thread::Builder::new()` sites and exactly 12 `qos::set_self`
  hits, 1:1 at the same lines. Every declaration is the **first statement** of
  its thread body (comment above, nothing before it), every class matches
  design §3a (9 `Utility` + 1 `Background` in chunk 1's six files; 2
  `Background` in chunk 2), every one carries the spec'd rationale comment.
  The four single-expression closures (`desktop-notify-drain`, `thegn-plugins`,
  `tgshare`, plus brace additions) were rewritten exactly per the chunk-1
  snippets — checked the surrounding logic survived (`while rx.recv().is_ok()
{}`, `supervise(...)`, `setup_and_schedule(...)` tails intact).
- **Ratchet final form.** `test/thread-qos-ratchet.txt` is now the consolidated
  header + exactly 4 entries (`db_task.rs`, `frame_writer.rs`,
  `loading/ticker.rs`, `pane_writer.rs`), byte-sorted, with the metrics.rs
  not-an-entry note. I spot-checked the header's factual claims against the
  code: `db_task::flush` really is awaited at `session.rs:820`
  (pre-resurrect barrier) and `main.rs:944` (clean exit); `metrics.rs:242`
  really does `recv_timeout(timeout)` on the collector. The §3c metrics.rs
  decision re-affirms correctly: the supervisor thread declares `Background`
  (`metrics.rs:90`, first statement of the thread body), which is what passes
  the file-level scan, and the child `thegn-metrics-collect` stays undeclared
  with the deadline rationale.
- **Regen-stable.** The committed file has one blank separator line between
  header and entries that the chunk-2 spec's text block omits. This is not a
  defect: `file_ratchet`'s regen (`test_support/ratchet.rs:150-164`) takes the
  leading empty-or-`#` run as the header and appends a blank line when the
  header's last line is a comment — so the committed form round-trips
  byte-identically (the done-note's `THEGN_RATCHET_UPDATE=1` round-trip, which
  I re-derived from the regen code) and the spec's exact text would not. The
  committed file is the correct normalization.
- **Ratchet semantics.** `long_lived_threads_declare_a_qos_class` scans all of
  `crates/thegn-host/src` (comment-stripped, per `code_only`), so its passing
  is a two-sided proof: no stale entries (each deleted pin's file really now
  declares) and no unpinned violators anywhere else in the crate. Note the
  scan is file-level — a file where one thread declares and another doesn't
  would pass silently; the design's claim that 12 declarations cover every
  Builder site in the 8 files is what closes that hole, and I verified it
  directly (12 = 12).
- **No `#[cfg]` in source.** The 7 `#[cfg]`-matching added lines in the diff
  are all prose inside `.thegn/pipeline/**`; `platform_cfgs_live_in_platform_
modules` passes. `qos::set_self` is the seam's public API, so the
  platform-cfg-host ratchet is untouched, as the design predicted.

## 3. Mandated gates (LEAD addendum) + scoped checks run here

- `cargo nextest run -p thegn-host -E 'test(platform_ratchet) | test(complete)
| test(help) | test(catalog_tests)'` — **88/88 passed**, including
  `long_lived_threads_declare_a_qos_class`, all platform ratchets, help
  ratchets, completion catalog.
- `just quick thegn-host` — clean (post-merge clippy on lib/bin).
- `rustfmt --edition 2024 --check` on the 8 touched source files — clean.

## 4. Unverified dispositions (the lane's open items)

- **macOS effect, both chunks** (`pthread_set_qos_class_self_np` actually
  steering core placement): unverifiable on this Linux box by construction —
  the call compiles to a no-op here (`platform/qos.rs` `imp::apply`). What can
  be checked is checked: compile-verified, call-pattern matches the existing
  `push_notify.rs`/`hydrate.rs` precedents, values are the public
  `<sys/qos.h>` ABI constants. Accepted as inherent to the seam, not a gap.
  The honest residual is that no Apple-silicon hardware has validated any QoS
  declaration in this repo, including THE-77's — a follow-up note for whoever
  next has a Mac, not a blocker.
- **Full gates** (`just test`/`lint`/`ci`): correctly deferred per the
  dev-loop policy; the mandated scoped suite above covers every ratchet these
  changes could plausibly trip. The pre-push hook remains the gate.
- **e2e**: not run, correctly — no render-path, chrome, glyph, color, or
  poll-site change anywhere in the diff, and QoS is a Linux no-op; no frame
  can differ.
- **`just ratchet-update` wrapper**: the underlying regen path was exercised
  (byte-identical round-trip) and I confirmed from the regen source that the
  committed form is what regen preserves. Accepted.

## 5. Corrections applied by this review (commit `fac8dc52`)

1. **chunk-2-done.md** claimed "Lines 1–17 of the previous header kept
   verbatim" — not true: lines 8–9 (dropping "burning battery for work nobody
   is waiting on", which duplicates qos.rs's module doc) and 13–16 were
   rewrapped to the spec §3 final form. Corrected with the accurate
   provenance plus the regen-stability explanation from §2 above.
2. **chunk-2.md** (the spec) contained the same internal contradiction — its
   parenthetical said "keeping the existing lines 1–17 header verbatim" above
   a text block that rewraps. Corrected to name the text block as
   authoritative. (The implementer followed the authoritative block; the spec
   bug was the architect's, and the done-note had already flagged the
   analogous chunk-1 done-criteria miscounts — "12 hits" vs. 10, "exactly 10
   non-comment lines" vs. the enumerated 6 — which were resolved correctly
   against design §6 and needed no further action.)

Both are pipeline-record fixes; no source file was touched by this review.

## 6. Gates still owed (standard pre-PR, not design gaps)

- Pre-push tier (`clippy` + `just test` + `just smoke`) on push — per the
  dev-loop policy these are hook-run, not per-edit.
- `just ci` once, when opening the PR (coverage/cross/openspec/nix-build are
  CI-side here since remote CI is dispatch-only).

Neither blocks the verdict: the branch is design-conformant, ratchet-honest,
and green on everything scoped to this lane.
