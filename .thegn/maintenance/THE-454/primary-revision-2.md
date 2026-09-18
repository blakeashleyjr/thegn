# Primary review of a3887a9e: second revision required

The literal URL and webcal repairs are accepted by source review. Do not reopen those designs. Fix the following concrete defects and weak tests, then hand off again. No issue closure or gate claim.

## 1. Buffer capacity fix is still incorrect (executed reproducer)

Vec::try_reserve_exact takes additional bytes relative to LENGTH, not CAPACITY. Your helper returns new_len - capacity. In the exact current source, len6/cap8/chunk4/limit10 => helper2; reserve_exact2 does nothing; extend grows cap to16 despite limit10. Primary compiled a standalone extraction of the unchanged helper and same operations and observed this. The test asserting bounded_reserve_needed(7,8,2,16)==1 merely mirrors the bug.

Use bounded geometric target capacity (no larger than limit), then reserve_exact(target_capacity - len), so growth remains amortized instead of reallocating on every network chunk. Check arithmetic and fallible reserve. Test real Vec append with spare capacity and assert capacity never exceeds limit after extension, plus exact cap, cap+1 and overflow. The standalone reproduction is below:

```rust
let mut body = Vec::<u8>::with_capacity(8);
body.extend_from_slice(&[0;6]);
let extra = bounded_reserve_needed(body.len(),body.capacity(),4,10).unwrap();
body.try_reserve_exact(extra).unwrap();
body.extend_from_slice(&[1;4]);
assert!(body.capacity() <= 10); // currently cap=16
```

## 2. Address policy

IANA's enclosing IPv6 protocol-assignment range is 2001::/23, not /29. Current /29 plus ORCHID checks leaves unassigned/reserved portions admitted as global. Use /23 conservative exclusion with any consciously reviewed public exceptions documented, or an equally complete precise policy. Correct reversed ORCHID/ORCHIDv2 comments. Add an address such as2001:100::1 and a normal public boundary example. Keep numeric mapped policy and existing local opt-in.

## 3. Required fixtures still do not prove their names/claims

- DNS fixture uses http://calendar.test/feed while resolver returns random listener port. reqwest connection port follows URL; put the actual test port in URL. Test repeated resolution of the SAME client with forced closed connections and a changed answer set; currently the 'rebind' case constructs a new client with one denied answer. Assert the resolver call count and zero forbidden target hits.
- Proxy child inherits NO_PROXY/no_proxy and lowercase \*\_proxy; a loopback target may bypass proxy even if .no_proxy is removed. Remove those bypass variables in the child, set uppercase/lowercase proxy variables consistently, use a proxy listener counter or failing proxy address, and prove direct request succeeds with proxy hits zero.
- total_and_idle_deadlines test only exercises absolute deadlines; 5-second read timeout is never reached. Add a quick internal injected read timeout fixture with an idle body and a separate dripping body whose absolute deadline expires. CalDAV 409 AND507 fallback must consume one shared deadline, with no third request; currently neither deadline recovery case is tested. Exercise actual backend invocation through a private test constructor or shared injection seam. Never-ending error body also needs the same bounded deadline test.
- media_encoding_and_shared_pool fixture does NOT count connections and does NOT inspect ETags/query credentials/sync tokens. Add accepted-connection counting across reconstructed routers/backends and record those request-local values for two accounts. The name and artifact must only claim what assertions prove.
- Oversize tests accept BodyLimit OR Policy OR Network, allowing unrelated breakage to pass. Assert exact expected variant for each fixture and validate success with actual valid ICS at the cap (padding via bounded comments is fine), plus cap+1, unknown/chunked length. Use raw HTTP for lying Content-Length if axum/hyper fixes or rejects the intended fixture; differentiate protocol error from our limit.
- All documented MIME variants and repeated Content-Type need focused coverage, and redirect fixtures need independent target listener/cross-origin and downgrade Location cases. Since Location is never followed, fixture can return those values without an external network call.

## 4. Dependency lock and hygiene

Added direct url dependency to thegn-svc requires Cargo.lock package dependency update. Explicit exception: you MAY run cargo metadata --offline --format-version 1 (save large output to a temp file) solely to resolve/update the existing lock without compiling. Do not run cargo update, build, tests, nextest or clippy; do not change unrelated locked versions. No full build in this worker.

Address the concrete code and test defects completely, commit, and report implementation-ready with actual source checks and unexecuted Rust tests. Primary will review and schedule focused compilation. Do not defer missing tests as merely unverified execution; the fixtures must exist.
