# Primary review — THE-670 revision 1 (reviewing rows 643 and 653)

The swap itself is good and measured better than predicted: `cargo nextest run -p
thegn-svc` on the native/forge paths is **48/48 green**, clippy is clean, and the
real resolve gives **947 → 912 crates (−35**, against the issue's predicted −31)
with octocrab's rsa/jwt chain gone. Row 653 reviewed it `source-review-clear`.

**But the land gate went red, and the cause is a scope condition I set
explicitly.**

## REQUIRED — restore `json()`'s existing behaviour

```
FAIL crates/thegn-svc/src/issue/http.rs:728
  assert!(matches!(mime.get::<serde_json::Value>("/bad-mime").await,
          Err(IssueError::Policy("tracker JSON content type refused"))))
```

The diff splits the single MIME policy error into **three** — "content type
missing", "content type invalid", "content type refused" — and also edits the
shared fake server's route table. So an existing tracker test, on a path this
change was not supposed to touch, now gets a different error than it did before.

My greenlight condition was: _"`json()`'s behaviour does not change, and no
existing tracker caller (Jira, Linear, the rest) changes behaviour. Add beside it;
do not refactor it."_ That is the condition on which I widened the scope to include
`issue/http.rs` at all, because the shared client backs every tracker.

Required:

1. **Leave `json()`'s MIME handling exactly as it was**, including its single
   `Policy("tracker JSON content type refused")` for a wrong or absent type. Do not
   re-tier it.
2. Put the finer distinctions on the **new envelope operation only**, if it needs
   them. A new operation may have a richer error set; the existing one may not
   change its.
3. **Revert the shared fake server's existing routes to their prior behaviour** and
   add any new route (`/rate-limit`, and whatever the envelope tests need) as a
   _new_ route. Changing what `/bad-mime` returns silently changes what every test
   using it asserts.
4. Re-run `cargo nextest run -p thegn-svc` **whole** — not filtered to
   `test(native)`. My own scoped filter is what let this reach the gate; the tests
   of the file you touch are the ones to run.

## Confirmed — keep all of this

- The error-classification table preserved row for row, including `403` with "rate
  limit" → `RateLimited` and "any other error is not-reached but does **not**
  increment the circuit".
- The **10-second** ceiling from the branch, not `TrackerHttpClient`'s 20-second
  default.
- `TrackerHttpBudget::process()`, the existing no-redirect client, response-size
  enforcement and the per-operation deadline reused — no parallel semaphore, no
  second client policy.
- `GithubNative` still synchronous on its current-thread runtime; the three pure
  parsers untouched.
- The RUSTSEC waiver removed only after the dependency-tree check, and the audit
  doc recording the **measured** −35.
- Honest reporting that a real TLS-handshake stall is not reproducible in a
  no-network fixture and the connect timeout was confirmed by inspection.

## Scope

`issue/http.rs` (restoring the existing path, keeping the new operation),
`forge/native.rs` and its fixtures. Nothing else.

## Validation

`nix develop --command cargo nextest run -p thegn-svc` — **the whole crate**. Also
`cargo check -p thegn-svc --all-targets`. **The pipeline sandbox mounts
`/nix/store` read-only, so this usually fails outright** — say exactly that and
stop if it does. The primary re-runs the whole crate and clippy regardless.

Never report a verdict for code you could not compile; state what you could not
run.
