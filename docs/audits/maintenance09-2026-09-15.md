# Maintenance queue drain — 2026-09-15

Source `e72d7fcec31224dd9aa463743bc0f370d09770ba` delivers the bounded output and diagnostic portion of
THE-325. Existing tunnel clients retain their authentication, environment,
file materialization, and provider configuration behavior. No new issue was
admitted after the user's drain instruction.

## Delivered behavior and evidence

Provider lines have a 4096-byte input and encoded-output limit. Oversized lines
are discarded through newline. Diagnostic buffering has 64 entries and URL
discovery has a separate one-entry signal. Readers retry interrupted reads,
preserve unterminated EOF addresses, recheck the address before reporting
diagnostic disconnection, and continue draining after startup returns.
Plan arguments/environment values, URL-rule values and public addresses are
redacted from Debug. Early-exit errors omit raw provider output. On startup
disconnection the owned parent is killed before waiting; this is not a
process-tree settlement or bounded reaper guarantee.

Primary and independent source/delta reviews approved the patch. Scoped strict
Clippy passed. One warm service test harness compilation used one job, disabled
sccache and a verified one-CPU quota. All **35 selected share tests
passed** in isolated temporary XDG state with one test thread. These cover
provider plans/configuration plus interrupted/oversized reads, full queues,
EOF startup, redaction, and continued pipe draining. No live provider, live
database, host restart, release build or performance measurement was performed.
The native runner is not a hostile hung-descendant cleanup proof.

[Evidence manifest](maintenance09-2026-09-15/manifest.json) records exact source
and artifact hashes. Raw evidence is stored losslessly as gzip to preserve
historical output without treating it as authored source. Final formatting, architecture/delivery ratchets, and all 170 OpenSpec items
passed. Their raw results and the final independent evidence review are included.

## Work held outside main

| Issue | Disposition |
|---|---|
| THE-220 | Claim APIs, host/process custody and schema integration remain unfinished. Clock correction has no standalone production caller. |
| THE-324 | Authority owner, writer/recovery, consumers and schema upgrade/verifier integration remain unfinished. |
| THE-327 | Lease/manifest substrate remains unwired; generation retirement needs positive settlement, selected runtime and conditional DB reservation. |
| THE-328 | Protected credential materialization remains dependent on unfinished custody/lifecycle integration. |
| THE-325 remainder | Credential transport, environment admission and selected-runtime/custody integration remain open. The full candidate would disable configured authentication and was not merged. |
| THE-319, THE-330 | Admitted before the freeze; no implementation entered this delivery. Further investigation stopped for the drain. |
| THE-630, THE-631, THE-632 | Performance evidence remains HOLD; the candidate regressed measured timings. No repeat-to-green or new performance build. |

THE-220, THE-324, THE-325 and THE-327 private candidates are preserved in local
`maintenance-held-20260915-the-*` branches, with exact refs/commits in the held
branch receipt. Held work is not merged, fixed, or marked Done. Completed
THE-321/THE-322 and the earlier maintenance batches remain on main.

This closes the reviewable merge queue for the requested rebuild; it does not
close all outstanding Linear issues. No push or live application restart is
part of this delivery. The user's dirty justfile and original untracked audit
files must be verified unchanged before and after the local fast-forward.
