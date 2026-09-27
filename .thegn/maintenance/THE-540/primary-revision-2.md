# Primary review — THE-540 revision 2 (reviewing rows 652 and 654)

Revision 1 landed all four of the previous review's findings and the primary
verified it: **165/165 green** after one primary fix (`Command::get_envs` yields
`&OsStr` keys, which do not compare against `&str`). Row 654 then confirmed those
four are addressed in source — and found a fifth defect that is worse than any of
them.

**Both findings accepted. Finding 1 is required in full.**

## Finding 1 (High) — REQUIRED: bind every `gh` call to the parsed origin

This is a target-confusion bypass that identifier validation cannot close, and it
is the most serious defect found in this lane.

`gh` list, read, trigger, rerun and cancel calls are built with **no explicit
repository target**, and `GitLoc::gh_command` runs `gh` inside the worktree — so
the repository is chosen by ambient CLI context. An inherited
`GH_REPO=other-owner/other-repo`, or a directory default from `gh repo
set-default`, therefore points an **authenticated mutation** at a different
repository than the caller selected. Under `provider = auto` the parsed origin is
used only to pick the provider and is then discarded.

That defeats the acceptance criterion directly: the whole point is that the final
request retains the authorized origin. Every identifier in the argv can be
perfectly valid while the request goes to the wrong repo.

Required:

1. **Derive a canonical GitHub target from the worktree's parsed origin** — the
   same origin the provider decision came from — and pass it explicitly to every
   `gh` operation, target options before `--`.
2. **Prevent ambient overrides from retargeting the request.** An explicit target
   is the fix; do not rely on it alone if `GH_REPO` can still win for some
   subcommand — establish which takes precedence and make ours authoritative.
3. Keep enterprise-host support, and **reject unsupported or ambiguous remotes
   before any authenticated call**.
4. The regression the reviewer specifies: set `GH_REPO` to a different fixture repo
   **and** configure a differing `gh` default, capture all generated argv, and
   assert every list/read/mutation targets the parsed origin. Plus zero provider
   invocations for a malformed or unsupported origin.

## Finding 2 (Medium) — accepted, narrowed to where it matters

The reviewer is right that `rejected_identifiers_never_reach_request_construction`
only counts pure-builder `Ok`s and never instruments a provider call, so "zero
requests for rejected inputs" is not actually asserted at the boundary, and the
CLI entries for `view`/`logs`/`rerun`/`cancel`/`trigger` have no request-path
tests at all.

Add the capture seam, but **prioritise the three mutations** — `rerun`, `cancel`,
`trigger` — plus **one read** (`logs`, which the control API already touches).
Those are where a mistargeted or unvalidated request does damage; a read against
the wrong repo is a wasted call, a mutation against the wrong repo is an
incident. For each: exact argv for a valid input, and **an invocation count of
zero** for a malformed id, selector or ref.

If the seam makes the remaining two reads nearly free, do them too — but do not
let exhaustiveness delay the mutation coverage, and say in your report which
entries you covered and which you did not.

## Confirmed addressed — do not revisit

The reviewer verified in source: GitLab origin parsing rejects raw and encoded
traversal, errors no longer echo remote credentials, host and project share one
per-operation origin snapshot, refs are query-encoded, positive bounded numeric
IDs share one contract with provider JSON IDs, malformed cached run/log IDs are
filtered at reuse, and GitHub IDs and workflow selectors are validated with
arguments after `--`.

## Scope

`ci.rs`, `cmd/ci.rs`, the control CI entry, and their tests. **No real provider
call or mutation in any test.** Do not extend to THE-539/THE-322/THE-172/THE-538/
THE-114.

## Validation

Attempt `nix develop --command cargo check -p thegn-svc -p thegn-host
--all-targets` and `cargo nextest run -p thegn-svc ci`. **The pipeline sandbox
mounts `/nix/store` read-only, so this usually fails outright** — say exactly that
and stop if it does. The primary re-runs the CI set, clippy, and the ratchets
regardless.

Never report a verdict for code you could not compile; state what you could not
run.
