# Primary review + greenlight — THE-540

Reviewing row 633. **APPROVED. Steps 1–4 stand as written. The compatibility
question you escalated is decided below — read it first, because it removes most
of the risk you were worried about.**

You did the right thing raising it instead of picking a bound and hoping. You also
correctly noted that the existing tests' integer IDs (GitHub 123/1/2; GitLab
1001/5001) prove nothing about live responses — that is exactly the reasoning gap
to refuse to paper over.

## DECISION 1 — the JSON path has no spelling problem, because it should not go through a string

Most of the compatibility worry dissolves with a typing change: **provider JSON
numeric IDs are parsed directly as `u64`, never as a string that is then
validated.** GitHub and GitLab both emit run/job IDs as JSON numbers, and serde
will reject `1.0`, `1e5`, `-1` and an over-range value on its own. There is no
"noncanonical numeric spelling" to accommodate, because we never see a spelling.

Today `id_str` accepts "arbitrary strings or any JSON number" — that is the bug.
Replace it with a typed numeric parse, not a stricter string grammar.

The string grammar is then needed only where a human or an API caller supplies an
ID: **CLI ingress and the control API.**

## DECISION 2 — the string grammar, fixed

For run and job IDs arriving as strings:

- ASCII decimal digits only, **no sign, no `+`, no whitespace, no separators, no
  dot segments, no percent-encoding, no fragment**.
- **No leading zeros** (IDs are ≥ 1, so `01` is malformed input, not a synonym).
- Must fit in `u64`; cap the length at **20 digits** so an absurd input is
  rejected before any numeric parse.

This accepts every ID either provider can currently emit — GitHub run IDs are
already 11 digits and growing, so do **not** pick a tighter numeric ceiling — while
rejecting `--help`, `-1`, `1.0`, `1e5`, `1/2`, `1%2f3`, `1#x` and a megabyte of
digits.

**If you find a real provider ID this rejects, stop and report it rather than
widening the grammar** — same instruction as before, now with a specific bound to
test against.

## DECISION 3 — a malformed row's fate depends on the context, and this is the important one

You asked whether invalid provider rows are omitted or make the parse fail. **Both,
by context, and conflating them would reintroduce the defect:**

- **List context** (fetching many runs or jobs): **discard the invalid row and
  count it.** One malformed entry must not blank the whole CI panel. But the count
  must be **surfaced**, not swallowed — a silent discard is precisely the
  "malformed provider output coerced into healthy empty data" failure that
  THE-542 exists for, and doing it here would create that bug rather than avoid it.
  A list that discarded rows is not a complete list and must not present as one.
- **Targeted context** (the caller named run X, or a mutation targets job Y):
  **fail closed with the specific error.** Never a silent no-op, never a success.
  A mutation whose target could not be validated has not happened, and must say so.

Make both behaviours explicit in tests, including the discarded-count assertion.

## Confirmed from your plan

- Pure validators and pure builders, **revalidated at the provider boundary before
  every `run_cli` / `run_bounded_log`** — validation at ingress alone is not enough
  when a value can arrive from a cache.
- **`--` is a belt, not the braces.** Options before it, positional selectors
  after, and the validator still mandatory. Do not let the terminator become the
  argument for skipping validation on any path.
- A **distinct workflow-selector / ref type** with its own name-or-path grammar.
  Applying the numeric ID grammar to a workflow name would break legitimate usage,
  and the issue calls that out as its own acceptance criterion.
- Structured query/component encoding for GitLab, and a **real remote
  authority/path parser** rather than slash replacement. Assert the final request
  retains the authorized origin and project.
- Errors name the **observed malformed component**, not the category.
- Mutation helpers propagate nonzero and error outcomes and never treat help or
  alternate-mode output as success.

## Constraints

- **Vendor CLIs only inside their implementation files** — there is a shrink-only
  ratchet on `gh` calls outside the forge impl; a new call site elsewhere fails
  the build.
- **No network and no real provider mutation in tests.** Assert on exact argv and
  exact URL, and assert **zero requests** for rejected inputs — that second half is
  the one that actually proves the validation runs before the call.
- THE-539, THE-322, THE-172, THE-538 and THE-114 are related only. Nothing from
  any of them enters this diff. THE-322 is the same defect in the tracker
  providers: **do not fix it here**, however shareable the grammar looks. Report it
  as a finding if you see a clean way to share later.
- If pure validators land in `thegn-core`, it is substrate-free (no HTTP types, no
  provider SDK) and gated at 95% lines.

## Validation

Attempt `nix develop --command cargo check -p thegn-svc -p thegn-host
--all-targets` and narrow `cargo nextest run -p thegn-svc ci`. **The pipeline
sandbox mounts `/nix/store` read-only, so this usually fails outright** — say
exactly that and stop if it does. The primary runs clippy, the full workspace
nextest and smoke centrally.

Never report a verdict for code you could not compile; state what you could not
run.
