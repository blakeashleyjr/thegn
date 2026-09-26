# Primary review + greenlight — THE-198

Reviewing row 596's investigation. **APPROVED in part — the reachability finding
is accepted, the removal plan is REFUSED.**

## Accepted, and verified independently by the primary

Your dependency chain is right: `octocrab 0.54.1 → jsonwebtoken 10.4.0 →
rsa 0.9.10`, and RustSec has no patched `rsa`.

Your reachability claim is also right, and the primary confirmed it:

- `crates/thegn-svc/src/forge/native.rs:598-599` builds the client with
  `.personal_token(token)` — a bearer token, no asymmetric signing.
- A repo-wide search for `app_auth` / `AppAuth` / `jsonwebtoken` /
  `private_key` / `RS256` / `app_id` in `crates/` finds **no** call site. The
  only `private_key` hits are redaction key-name lists.

So the vulnerable RSA private-key operation is linked but **never invoked**. That
is the finding that decides this issue.

## REFUSED: do not remove octocrab

You describe it as "the unused native octocrab layer". **It is not unused.**
`crates/thegn-svc/src/forge/mod.rs:242` documents it as _"The GitHub ladder:
native octocrab reads over the `gh` CLI"_ — native is the **preferred** rung,
with `gh` as fallback. `forge/mod.rs:8` re-exports `GithubNative`.

Removing it would delete a working feature, and:

- **THE-670** ("Replace octocrab with the existing generic tracker HTTP client")
  owns that work, and its point is to _keep_ the native layer on a different
  client — not to drop the rung.
- THE-621 and THE-622 (both landed) hardened this exact native path.
- THE-170 changed `native.rs` earlier in this same batch.

Deleting it here would silently undo all of that under the cover of an advisory
fix. Do not touch octocrab, `jsonwebtoken`, or the ladder.

## What to implement — option 3 from the coordination brief

1. **Narrow the `deny.toml` ignore** from a global entry to the exact package and
   version, carrying: an owner, an expiry or review trigger, a link to
   RUSTSEC-2023-0071, and a one-line statement of _why_ it is accepted — the
   reachability argument above, not "local only".
2. **Add the guard.** A test must fail if an App/JWT/private-key auth path
   becomes reachable, so the acceptance expires automatically rather than
   silently. Assert on the absence of those construction paths, or on the
   dependency feature set — whichever is robust rather than a grep of source
   text that a rename defeats.
3. **Write the reachability evidence into the artifact**, call-site to trust
   boundary, with the exact resolved versions. That evidence _is_ the risk
   acceptance; a future reader must be able to re-check it without redoing your
   work.
4. Record "remove octocrab to drop this advisory entirely" as a follow-up
   **on THE-670**, noting it would also retire this ignore.

Change nothing else in `deny.toml`, and do not add ignores for other advisories
you happen to notice — report those separately.

## Validation

`cargo deny check advisories` is the meaningful one here. Attempt it; **if
`nix develop` fails inside your sandbox** (the pipeline env mounts `/nix/store`
read-only) say so and stop — the primary runs it.
