# Primary review — THE-540 revision 1 (reviewing rows 640 and 651)

Row 640's implementation is substantially right and the primary verified it:
`cargo nextest run -p thegn-svc -p thegn-host` on the CI paths is **163/163
green** after two primary fixes (below). Row 651's adversarial review then found
four defects. **All four are accepted. Fix all four.**

This is the best review of the batch: finding 1 was confirmed empirically against
a WHATWG URL implementation rather than asserted, and the reviewer added the
failing tests for findings 1 and 2 instead of only describing them. Those tests
are **expected to fail until you fix the code** — that is correct, do not weaken
them.

## Already fixed by the primary — do not redo

1. **`system_from_remote_host` admitted lookalike hosts.**
   `hostname.starts_with("github.")` matches `github.com.evil.test`, which your
   own test asserted must be `None`. The prefix rule cannot be made safe — a
   genuine self-hosted `github.mycorp.com` is structurally identical to the
   lookalike — so it is **deleted**; matching is now exact apex or subdomain only,
   and self-hosted instances need explicit configuration. Commit `57a4420b`.
2. **`ci_ids_have_one_bounded_canonical_spelling` asserted raw bytes.** The
   validator quotes its input with `{:?}`, which escapes control characters so a
   rejected id carrying a newline or tab cannot inject them into a log line. That
   is the right behaviour, so the **test** now asserts the escaped spelling.
3. Three `set_ci_detail` test call sites needed the new `discarded_jobs` argument.

## Finding 1 (High) — validate the RAW path, before normalization

Accepted, and this is the one to get right. `url::Url::parse` applies WHATWG
normalization, so `https://gitlab.com/%2e%2e/group/repo` has already become
`/group/repo` by the time you inspect `parsed.path()` — the traversal is erased
rather than rejected, and the operation silently targets a different project.

Validate the **raw** path text before it reaches the normalizing parser: reject
literal and percent-encoded dot segments and encoded delimiters, then derive the
project, then keep the final authority/project assertion at the request boundary.
The parser stays for authority extraction; it just stops being the thing that
decides the path is safe.

## Finding 2 (Medium) — complete the ref grammar, including the leading dot

Accepted. `.hidden` and `feature/.hidden` are not valid Git refs and currently
pass. Implement the `git-check-ref-format` component rules properly — no component
beginning with `.`, no `..`, no trailing `.lock`, no control characters, no
`~^:?*[`, no `@{`, not a bare `@` — **while still accepting query-reserved
characters as ref data**, which is the point of validating refs separately from
encoding them.

## Finding 3 (High) — never interpolate a raw remote into an error

Accepted, and this is the most serious finding because it leaks outward.
`https://user:token@gitlab.example/group/repo` fails to parse and the error
currently carries `user:token` into CLI output and provider logs.

Report a **safe reason** — the component that was wrong — or a redacted URL with
userinfo stripped. Never the whole raw remote. Add the regression that asserts the
secret does **not** appear in the emitted error, and while you are there check the
same pattern nowhere else in this diff interpolates a remote, a token or a URL
into a message.

## Finding 4 (Medium) — one snapshot per operation

Accepted. `project_seg` resolves and validates `origin`, then each `command(...)`
resolves `origin` again to pick `GITLAB_HOST`, so a remote that changes between
those subprocesses can pair project A with host B — sending a credential-bearing
request to a retargeted host. That is exactly the "assert the final request retains
the authorized origin/project" requirement failing.

Snapshot the parsed remote **once per operation** and pass that same host and
project to both endpoint construction and command environment setup. Test the
single-snapshot boundary deterministically — the reviewer is right that a timing
race is not a test.

## Scope

The four findings, in `ci.rs` and its tests. Nothing else — do not extend to
THE-539/THE-322/THE-172/THE-538/THE-114, and do not revisit what row 640 already
got right (the reviewer's coverage list confirms the ingress/cache/control
revalidation, the `u64` JSON parsing, the counted discards, and the `--`
placement all hold).

## Validation

Attempt `nix develop --command cargo check -p thegn-svc --all-targets` and
`cargo nextest run -p thegn-svc ci`. **The pipeline sandbox mounts `/nix/store`
read-only, so this usually fails outright** — say exactly that and stop if it
does. The primary re-runs the 163-test CI set and clippy regardless.

Never report a verdict for code you could not compile; state what you could not
run.
