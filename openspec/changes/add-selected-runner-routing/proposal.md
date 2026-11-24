# Selected runner routing (contract slice)

Linear: THE-2416

## Why

A project can already name a runner and a closed build/test recipe, but the
submission route must prove that the request reached exactly that runner and
that its receipt can be read back through the ordinary `JobResult`. Without a
typed seam, a selected job would either fabricate provider placement or leave
its outcome ambiguous.

This change is the contract slice of THE-2416. It does not route terminal
commands, read runner selection from config, guard against recursive dispatch,
or forward argv, cwd, stdio or per-step deadlines. Those belong to later work
and THE-2416 stays open.

## What changes

- Add a typed submission seam (`submit_selected`) that accepts only an
  explicitly named runner, an allowlisted `cargo-build`/`cargo-test` recipe and
  a pinned source digest.
- Forward the spec deadline to the selected runner and fail closed on any field
  the route cannot forward, rather than silently dropping it.
- Distinguish refuse-before-submit from lost-after-submit so a transport drop or
  unproven receipt reads back as indeterminate, never as rejected.
- Map success, nonzero exit, cancellation, deadline timeout and signal
  termination into the existing `JobResult`, and report a truncated log tail.
- Publish the corresponding `job-v0` schema descriptions for `runner`, `recipe`
  and `Recipe`.

## Impact

Roadmap BC 764 (bounded Jobs) owns the surrounding work. This increment is
limited to `crates/thegn-job-spec` and its published schema; no daemon, host or
provider reads the route yet. Terminal interception, config lookup, the
recursion guard and argv/cwd plumbing remain follow-up work.
