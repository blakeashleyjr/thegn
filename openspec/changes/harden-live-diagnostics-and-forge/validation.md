# Validation checkpoint

## September 14 residual THE-621 acceptance

Source `5e2f5bf2bcb3b2473e48775850133983e1f7be31` repairs an additional
authority-confusion case found during final issue acceptance. The earlier host
helper could mistake `https://evil.example/foo@github.com/bar` for GitHub. Native
admission now obtains host, owner and name from the existing strict origin parser.
Unsupported origin forms use the established CLI fallback.

The private service-package native run passed 26/26 forge tests, including one
new actual-Git gate test with 18 rejected origins (zero token calls and the exact
origin-refusal reason) and four supported controls (one inert token callback and
the exact identity). No network request or real credential helper was invoked by
that new test. Strict `cargo clippy --offline --locked -p thegn-svc --all-targets
-- -D warnings` also passed. Both used two Cargo jobs, an empty Rust wrapper and
the private native target. Primary and independent adversarial source review
approved the repair. See `docs/audits/THE-621-native-origin-acceptance.md` for
hashed receipts and limits. Reviewed local-main landing completed at
`16288656262992c677e26ede4127a37bacb56fe1`; later metadata-only confirmation
does not change the tested Rust source.

General configured-forge host routing still uses the older helper and is tracked
separately as THE-642. This fix independently protects the native credential gate;
it does not claim to repair general forge routing.

## Earlier combined implementation checkpoint

All commands use isolated worktree/state. Cargo uses `RUSTC_WRAPPER=''`,
`CARGO_BUILD_JOBS=2`, `CARGO_TARGET_DIR=/tmp/thegn-audit-build-cache-20260913`,
`--offline`, and `--locked` after adding the existing tracing-subscriber
host dev-dependency edge. No crate versions changed.

- `cargo test -p thegn-core --lib connectivity::`: 13 passed.
- `cargo test -p thegn-core --lib config_diagnostics::`: 3 passed initially.
- `cargo test -p thegn-core --lib log_trace::`: 16 passed, including isolated
  subprocess cases for unchanged/changed config warnings, ordinary startup
  suppression, and explicit logging overrides.
- Final `cargo test -p thegn-svc --lib forge::`: 24 passed, including actual
  octocrab GraphQL envelopes over an in-process tower transport, ladder calls,
  typed HTTP/transport errors, public-host gating, helper timeout/cancellation,
  parent-exit with inherited pipe, capacity retention, injected thread-spawn
  failure, and unknown-wait reaper-only cleanup. This compiled final core code.
- `/tmp/thegn-audit-input-harness`: 13 passed against the actual `input.rs`
  module, cached termwiz/tracing dependencies, and minimal enclosing host types.
  Includes broad DEBUG/TRACE payload exclusion and a source guard over every
  compositor tracing target (including dormant-frame wakeup).
- `/tmp/thegn-audit-config-diagnostics-harness`: 4 passed against final actual
  cache source, including fixing then reintroducing the same warning and bounded
  source-revision storage. Only the warning sink is stubbed in this harness.
- All 14 noncompiling commands from the `ratchets` recipe passed. Delivery
  registration is managed by the root integration and was not claimed here.
- Strict OpenSpec change validation, changed-file rustfmt, host-manifest taplo,
  and `git diff --check` passed.

The final source-revision refinement and named-profile attribution were made
after the initial full core test binary run. The combined candidate should rerun
the config/logging subprocess cases and the host input tests. No standalone full
host build was run; root is consolidating that expensive compile after merging
reviewed branches. The reviewed build.rs metadata fix is copied identically from
the root worktree to keep the combined host test loop from rebuilding on each
invocation.

Root review identified and corrected: a second raw-key log under the frame
target; reaping the credential parent before inherited stdout completed;
thread creation panicking during Drop; uncertain wait ownership triggering
numeric group signalling; and unsanitized Git calls in new test fixtures.
Independent adversarial review and final combined-candidate validation remain
required before acceptance. No live process, configuration, credentials,
canonical checkout, or external repository/account was changed.

Independent review found that non-Unix taskkill has no deadline. The candidate now refuses credential-helper spawn when direct bounded group termination is unsupported; explicit environment tokens remain supported. Native Windows helper execution is not claimed.
