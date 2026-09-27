# Primary review — THE-670 revision 2 (reviewing row 656)

**You restored `json()` correctly and I verified it.** The single
`Policy("tracker JSON content type refused")` is back for both an absent and a
wrong type, the three-way split is confined to the new envelope path, and
`/bad-mime` serves `text/plain` again. The whole crate is now **903/904**, up from
a red gate. The dependency closure is confirmed independently by
`test/the-198-rsa-policy.sh`: octocrab, jsonwebtoken and rsa all absent, 947 → 912.

One failure left, and it is a design question rather than a slip.

## DECISION — MIME is not a gate on a non-2xx envelope

```
FAIL issue::http::tests::fake_http_enforces_redirect_encoding_and_stream_caps
  http.rs:714  envelope_missing_mime.json_envelope(POST, "/envelope-missing-mime", …)
               expected Err(Policy("tracker JSON content type missing"))
```

Your fixture serves `/envelope-missing-mime` as **403 with a JSON body and no
Content-Type**, and expects a MIME rejection. But that fights the purpose of the
envelope: it exists so `native` can tell `403 + "rate limit"` from an ordinary
`403`, which means a non-2xx response's **status and body must reach the
caller**. If a provider returns an error body without a content-type and we turn
that into "MIME missing", we have destroyed exactly the classification this change
was made to preserve — and the old octocrab path did not behave that way.

So:

1. **Validate MIME only when the envelope's body will be parsed as JSON** — i.e.
   on the success path. A non-2xx envelope is returned with its status and raw
   body, unvalidated for content type, for the caller to classify.
2. **Move the two MIME cases onto a 2xx route.** `/envelope-bad-mime` and
   `/envelope-missing-mime` should be `200` with a wrong / absent content type
   respectively; that tests the property without contradicting (1).
3. **Add a case that pins the decision**: a `403` with **no** content-type must
   still classify as `RateLimited` when its body says rate limit, and as
   `NotAuthenticated` otherwise — never as a MIME error. That is the assertion
   that stops this being re-broken.

Note `/envelope-bad-mime` currently passes only because it carries `text/plain`;
once it is a 2xx the same assertion still holds, so this is a fixture change plus
one guard, not a redesign.

## Confirmed — keep all of it

- `json()`'s MIME behaviour and `/bad-mime` exactly as restored.
- The error-classification table row for row, including `403` + "rate limit" →
  `RateLimited` and "any other error is not-reached but does **not** increment the
  circuit".
- The **10-second** ceiling from the branch, not the client's 20-second default.
- `TrackerHttpBudget::process()`, the no-redirect client, response-size
  enforcement, the per-operation deadline — no parallel semaphore, no second
  client policy.
- `GithubNative` synchronous on its current-thread runtime; the three pure parsers
  untouched.
- The RUSTSEC waiver removed only after the tree check; the audit doc recording the
  **measured** −35.
- Your honesty that a real TLS-handshake stall is not reproducible in a no-network
  fixture.

## Fixed by the primary already — do not redo

`client_with_delay` is `pub(super)` in the regression-test module and the global
connectivity test used it without importing it (E0425); the import is added. An
unused `Response` import is dropped. `Cargo.lock` is committed from a real
resolve rather than the hand edit.

## Validation

`nix develop --command cargo nextest run -p thegn-svc` — **the whole crate**, plus
`cargo check -p thegn-svc --all-targets`. **The pipeline sandbox mounts
`/nix/store` read-only, so this usually fails outright** — say exactly that and
stop if it does. The primary re-runs the whole crate and clippy regardless.

Never report a verdict for code you could not compile; state what you could not
run.
