# THE-635 plain log field styling review — September 14, 2026

Final assembled source checkpoint: `4e212d09`.

## Reproduction and diagnosis

The first configured full workspace run stopped at
`log_trace::tests::text_formatter_covers_plain_timestamped_and_ansi_worktree_forms`:
the plain line contained generated ANSI escapes around its structured `count`
field. It stopped after 2292/8354 tests, with 2291 passes and one failure; the
6062 unrun tests were not passes.

Independent isolated reproduction used the already compiled core test binary,
private XDG state/config/data/cache directories, no live logging installation and
no Cargo build. The exact existing test failed with NO_COLOR absent and passed
with NO_COLOR=1. Logs:

- `/tmp/thegn-plain-ansi-unset-20260914.log`: exit 101, generated ANSI field style.
- `/tmp/thegn-plain-ansi-set-20260914.log`: exit 0, masking environment present.

This was a real formatter bug exposed by the isolated environment. `Brand.ansi`
controlled its own prefix, but field formatting inherited the outer layer's
separate writer ANSI flag. Production text-file and redirected CLI sinks also
constructed Brand with plain policy without disabling that inherited field flag.

## Approved change and regression scope

Only when Brand selects plain text, field formatting now borrows a
`Writer::new(&mut writer)` adapter. The cached tracing-subscriber API initializes
that adapter with `is_ansi=false`, as the existing JSON path already does. It
forwards directly, allocates no intermediate buffer and preserves formatting
errors. The ANSI branch and JSON implementation remain unchanged; this is not
post-processing that strips event payloads or changes filtering policy.

The formatter regression explicitly enables ANSI on the outer layer, then proves
both timestamped and untimestamped plain output preserves numeric/boolean fields
without generated styling. The colored positive control retains colored fields.
The JSON regression also forces outer ANSI and checks parsed fields remain plain.
The existing production-install child harness removes NO_COLOR only from its
private subprocess environment and checks actual text files, redirected stderr
and JSON with structured fields intact. It does not reconfigure live logging or
mutate parent process environment.

Primary approved the plan before source edits. The independent process/time-policy
reviewer inspected the final source and cached Writer implementation, and found
no scoped blocker. Rustfmt and diff checks passed. Root owns the Linear issue,
specification/delivery changes, compiled tests and final landing.

## Final execution gate

At this report checkpoint the rebuilt full workspace run with no fail-fast is
compiling/running. Its log is
`/tmp/thegn-rolling-full-workspace2-20260914.log`. Source approval and the old
NO_COLOR-masked pass are not final test passes. Exact formatter/install cases and
workspace completion must be inspected in that final receipt before closure.

## Final workspace execution observed

The no-fail-fast run at assembled source `4e212d09` completed in
`/tmp/thegn-rolling-full-workspace2-20260914.log`: 8354 executed, 8348 passed,
6 failed, 26 configured skips. THE635 exact formatter (line2482), structured JSON
(line2475), and real isolated production-install parent harness (line2490) all
passed. The 16 log_trace entries include one no-op child entry; the parent harness
actually exercises the file, redirected stderr, and JSON child modes. This is
15 regression entries plus a helper, not 16 independent native regressions.

The six workspace failures are outside this change: three config example/env
coverage checks, two Git fixture setup assumptions (THE637), and the watchdog
fixture's real sandbox side effects/private runner cleanup (THE636). The workspace
gate remains failed until those fixes and the final rerun pass.
