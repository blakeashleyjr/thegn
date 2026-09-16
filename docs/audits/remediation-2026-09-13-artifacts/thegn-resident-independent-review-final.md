# THE-154 independent scoped review

Final reviewed source: `53502768`, following the original `af56ce47` candidate.
No remaining source blocker found for the scoped Linux resident liveness and
resource-custody repair. Landing remains conditional on the integration owner's
final assembled host/service gates; this is not approval to close full THE-154.

Reviewed bounded FIFO/count/byte admission, delimiter serialization allocation,
close-latch priority, originating-session provider correlation, final-response
drain, tracked owner/held custody, cancellation-safe joins, one shutdown deadline,
Unix readiness and signal/wait ordering, finite Windows pipe worker ownership,
and host reload/exit wiring. No new resident idle timer was added. Native Windows,
non-Linux execution and enforceable escaped-descendant tree containment remain
explicitly unproved.

The review reproduced a delimiter allocation defect using actual extracted
BoundedFrame code and ordinary provider-call JSON with 300000/300044-character
parameter strings. Appending newline grew capacity from 600108 to 1200216,
exceeding the one-megabyte bound. The accepted `finish` revision uses exact
reservation; the same independently run fixture now reports wire length and
capacity 600109 and round-trips the JSON. The production path and repository test
exercise this finish helper. Harnesses: `/tmp/thegn-resident-newline-cap.rs` and
`/tmp/thegn-resident-newline-fixed.rs`.

Also reviewed final revisions prohibiting observation/consuming waits after
identity loss, the syscall-count regression, and exact-owned-child termination
after original-group success or ESRCH. The fixture uses an owned shell builtin,
reaps that child, and checks that later termination cannot reach the signal seam.
No changed-group discovery or post-reap numeric fallback was introduced.

## Darwin source check

Independently passed at exact final Unix source `53502768`:

```
cargo check --offline --locked --manifest-path /tmp/thegn-resident-darwin-check-20260913/Cargo.toml --target aarch64-apple-darwin --tests
```

Private target `/tmp/thegn-resident-darwin-target-20260913`, jobs1, empty
RUSTC_WRAPPER. The harness imports the actual repository Unix platform file and
its embedded tests; dependencies are exact cached Tokio1.53.1, libc0.2.189 and
tracing0.1.44. Initial check took23s; final cached recheck took0.23s. Final log:
`/tmp/thegn-resident-darwin-platform-check-final.log`. No source edits, shared Cargo
cache, network/dependency install, linking, native macOS execution or whole-graph
Darwin claim. This checks the Darwin SIGCHLD/waitid branch, not Linux pidfd code.

Author evidence records native plugin46/46 before the last two new regressions;
root owns the final expected48-test assembled rerun. The author's exact-source
Unix platform4/4 and Windows service crosscheck are separate evidence, not tests
run by this reviewer. Full native/platform/tree guarantees remain outstanding.
