Primary foundation review, after native implementation 783cc550

Scope is only THE-505 chunks 1–2. Do not approve or close the full issue: capture, provider validation, startup/CLI adoption, atomic store/reload/authority fencing, explain/health, teardown, and cross-cutting migration remain pending.

Primary changes committed with this review:

- Executed the exact standalone scanner and seven worker regression tests without Cargo. Four initially failed: physical-line test assumed newline counting; member-cap fixture exceeded line cap first; node-cap fixture exceeded member cap first; array depth fixture omitted the enclosing key level. Corrected each fixture to isolate its intended boundary. All seven now pass in the standalone extraction. Receipt: primary batch config-budget-primary-tests.txt; original failure receipt config-budget-exact-tests.txt. This is not a whole-crate test result.
- Replaced compatibility TOML encoded-size depth multiplier with full inherited path charging at each table/array node. Parent names repeat with descendant count, not merely nesting depth. Added wide/deep/escaped and long-parent/wide-table regressions. These Rust regressions have not yet been run.
- Fixed two AdmissionInputs test initializers missing the new paths field.
- Refused non-UTF-8 captured HOME before lossy normalization/digesting; existing Config path fields are strings. Added a Unix regression; this must remain a typed refusal rather than silently selecting a different OS path. SourceInput host capture still needs lossless identity in later chunks.

Independent reviewer instructions when dispatched:
Review the complete foundation delta from 0be575ce, all prior primary instructions, and these fixes. Focus on scanner correctness before TOML allocation (quoted dotted keys, nested scopes, multiline 4/5 quote endings, member/node counts), bounded compatibility serialization (ancestor paths and escaping), schema/semantic validation before warn/default coercion, captured deterministic normalization, redaction, preserved existing 4 MiB/depth32/container1024 host bounds, and absence of unreviewed runtime effects. Identify concrete bypasses and compatibility failures. Do not approve unwired API additions as a finished bug fix.

No Cargo/builds for reviewer; primary centrally schedules focused tests in the stable checkout after the running calendar gate. Commit review findings and optional tests, never production edits or main changes. Use source-review-clear/revisions-needed; explicitly record validation pending. No merge, push, issue close, or child agents.
