# Tasks

- [x] 1. Add the explicit `[sandbox]` compiler-cache `off`/`auto` policy,
     provenance, validation, example config, help, env overrides, and reload tests.
- [x] 2. Make explicit/default off clear inherited sccache wrapper/endpoint
     variables without rewriting unrelated custom wrappers.
- [x] 3. In auto mode, resolve the wrapper, data path, transport, and minimum
     grants, then probe reachability/writeability through the effective sandbox.
- [x] 4. Fall back to direct rustc with a durable degradation diagnostic when
     the contained cache probe fails; cache failure must not become a gate failure.
- [x] 5. Add doctor rows that distinguish host availability from contained
     reachability/writeability and name config provenance plus remediation.
- [x] 6. Add a real Linux/bwrap compile smoke for a reachable cache (including a
     repeat-build cache hit), an unreachable endpoint that compiles uncached, and
     explicit off behavior.
     The explicit ignored host smoke ran outside the outer sandbox and proved a
     real miss-then-hit, compiler-error preservation, broken-UDS direct fallback
     with a durable diagnostic, and explicit-off compilation.
- [x] 7. Add unsupported-backend reporting and run focused config/sandbox/doctor
     tests, strict OpenSpec validation, and the normal repository gate.
     Reporting, focused tests, strict validation, and the final 7,841-test
     workspace integration gate all passed.
