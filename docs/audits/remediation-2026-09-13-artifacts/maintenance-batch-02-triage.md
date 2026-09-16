# Maintenance batch 02: ten existing defects

Triage read against private candidate `9fb68798`, the current runtime source,
available local branch references, and full Linear descriptions/relations on
September 13, 2026 (Pacific). This is the next implementation batch, not a
completion or landing claim. Batch 01 combined-host validation remains first.
All ten selected issues concern existing behavior; no new product features.

Priority order balances the user's reported defect, destructive/authorization
risks, and reachable parser/lifecycle failures. Every selected issue has no
machine-readable prerequisite in Linear at triage time. Internal sequencing
below coordinates overlapping code; it does not waive full issue acceptance.

| Order | Issue                                                                                   | Lane | Current evidence and bounded scope                                                                                                                                                                                                                                                                                                                                                                                    |
| ----- | --------------------------------------------------------------------------------------- | ---- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1     | [THE-628](https://linear.app/blakeashley/issue/THE-628) — process-view refresh          | A    | User-visible, reproduced by source investigation: `run.rs` model replacement drops sampler-owned `model.procs`; `metrics/procs.rs` top-N ties are unstable; `monitor.rs` retains a numeric cursor. Preserve the snapshot, deterministic ordering and selected PID/start-time identity; bind delayed destructive actions to that identity. No new cadence or live signals.                                             |
| 2     | [THE-484](https://linear.app/blakeashley/issue/THE-484) — duration/epoch policy         | A    | `fly_reaper.rs:69` and `vps_reaper.rs:109` still subtract signed timestamps and cast `max_lifetime_secs as i64`; extreme accepted values can authorize deletion. Repair shared duration/age conversion, configuration bounds, all narrowed duration call sites and explicit unknown-time handling. Test reaper decisions with fixture data only. Coordinate the primitive with THE-483 before editing callers.        |
| 3     | [THE-545](https://linear.app/blakeashley/issue/THE-545) — PR authorship gate            | A    | `pr_driver.rs:751` and `ci_autofix.rs:260` still infer authorship from URL namespace. Carry authenticated author identity through fetched/cached models and one shared dispatch guard; absent/legacy identity denies automation. Preserve selected forge/account authority. No actual PR writes or agent launches in tests.                                                                                           |
| 4     | [THE-343](https://linear.app/blakeashley/issue/THE-343) — OSC 52 set-only admission     | B    | `queries.rs:194` forwards every `52;` prefix, including clipboard reads. Parse complete bounded set syntax, selectors and base64; reject queries before writer admission. Current main lacks the candidate's stateful retry parser, so implement the required split-input and generation behavior explicitly without importing the whole terminal-writer branch.                                                      |
| 5     | [THE-377](https://linear.app/blakeashley/issue/THE-377) — custom-command interpolation  | A    | `custom_cmd.rs:110` still returns raw values for unfiltered placeholders. Establish inert argument expansion and explicit dangerous-raw syntax with deterministic migration diagnostics. Cover all placeholder families, shell contexts, output modes and transport builders; sentinel tests must prove repository text cannot execute. Quoting a value inside arbitrary pre-existing shell quotes is not sufficient. |
| 6     | [THE-154](https://linear.app/blakeashley/issue/THE-154) — resident-plugin I/O/lifecycle | B    | `plugin/session.rs:39` holds the stdin mutex across writes; EOF handling holds the child mutex during `wait`, and kill needs both. Introduce bounded ordered writes plus one owned process lifecycle; test stopped readers, stdout EOF with a live child, descendants and concurrent shutdown. Native Windows evidence remains required for a cross-platform completion claim.                                        |
| 7     | [THE-310](https://linear.app/blakeashley/issue/THE-310) — framed decoding               | B    | `lsp/framing.rs:36` appends without a header/buffer bound; `next_message` rescans and drains prefixes. Introduce typed terminal protocol errors, explicit header/body/buffer/work caps and amortized linear parsing. Update both bridge and LSP consumers and prove closure semantics; pure decoder tests alone do not finish the issue.                                                                              |
| 8     | [THE-483](https://linear.app/blakeashley/issue/THE-483) — shared cadence overflow       | A    | `hydrate.rs:415,589,602` and `ci_refresh.rs:21` still multiply arbitrary seconds by 1000. Use a shared nonzero checked conversion, matching validation/schema limits and observable failure. Establish this primitive before THE-484 caller edits; prove unrelated ticker work survives or configuration is rejected before spawn.                                                                                    |
| 9     | [THE-286](https://linear.app/blakeashley/issue/THE-286) — Kitty streaming parser        | B    | `kitty_relay.rs:82–114` repeatedly copies/rescans retained APC bytes. Current candidate is weaker than the issue's historical branch: it also lacks that branch's byte cap. Add a bounded streaming state machine with split-ST handling, discard-through-terminator recovery and deterministic scan-count tests. Do not claim THE-218 multi-command transfer correctness from this parser fix.                       |
| 10    | [THE-471](https://linear.app/blakeashley/issue/THE-471) — calendar display text         | B    | `detail/calendar/render.rs:326` passes raw event titles/calendar names into cells. Add one bounded display policy across agenda, headings, world-clock labels and reminders, while retaining raw semantic values separately. Measure and draw the same sanitized text; hostile fixtures must prove popup bounds and single-row reminder behavior.                                                                     |

Lane A owns process snapshots/selection, duration/cadence policy, PR authorization
and command templates. Start THE-628; agree the THE-483/THE-484 shared time API
before those edits; then complete the independent PR/template repairs. Lane B
owns terminal/protocol parsing, plugin I/O and calendar display sanitation.
Keep each issue reviewable separately. Both lanes may touch `run.rs` only through
small reviewed integration hunks; helpers belong in owning modules. Neither
lane changes the batch-01 telemetry clock or writer ownership casually.

The proposed list was revised rather than treating old urgent labels as proof
of a current defect:

- **THE-562 and THE-558 excluded from this batch:** they are regressions in an
  unlanded THE-153 candidate. The current tree has neither
  `report_launch_token_hash` nor `.github/workflows/security-linux.yml`.
  Keep them as THE-153 admission blockers; do not add the vulnerable feature
  merely to repair it. Their historical candidate snapshots must be reviewed
  when that work resumes.
- **THE-535 deferred:** its authoritative dependency is THE-505 configuration
  generation admission. Duplicate plugin launch remains real, but completing
  only a duplicate-ID check would not satisfy this issue's full contract.
- **THE-374 deferred:** its authoritative prerequisites THE-368 and THE-372
  are not in this batch. The current undo/discard fail-open paths merit urgent
  work in the subsequent Git-transaction batch; batch-01 cleanup checks do not
  establish the required general mutation transaction or typed execution seam.

No matching issue-fix commits were found by identifier in available refs; that
is not proof that uncommitted historical candidates do not exist. The checked
`origin/tg/terminal-landing` tip has the same affected parser blobs as main and
is not a ready parser repair. Reuse any subsequently discovered candidate only
after plan, source and regression review, with its provenance recorded.

Each issue needs its own investigated acceptance cases, primary plan approval,
reviewed implementation, focused tests and independent adversarial review.
Serialize shared Cargo work, then compile the assembled host graph once.
Sandbox socket/ownership or native-platform failures remain unresolved evidence,
not skipped passes. Local-main landing still requires writable canonical Git;
this triage does not change that execution restriction.
