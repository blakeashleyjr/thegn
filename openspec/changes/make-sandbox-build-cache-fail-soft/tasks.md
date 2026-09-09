# Tasks

- [ ] 1. Add the explicit `[sandbox]` compiler-cache `off`/`auto` policy,
     provenance, validation, example config, help, env overrides, and reload tests.
- [ ] 2. Make explicit/default off clear inherited sccache wrapper/endpoint
     variables without rewriting unrelated custom wrappers.
- [ ] 3. In auto mode, resolve the wrapper, data path, transport, and minimum
     grants, then probe reachability/writeability through the effective sandbox.
- [ ] 4. Fall back to direct rustc with a durable degradation diagnostic when
     the contained cache probe fails; cache failure must not become a gate failure.
- [ ] 5. Add doctor rows that distinguish host availability from contained
     reachability/writeability and name config provenance plus remediation.
- [ ] 6. Add a real Linux/bwrap compile smoke for a reachable cache (including a
     repeat-build cache hit), an unreachable endpoint that compiles uncached, and
     explicit off behavior.
- [ ] 7. Add unsupported-backend reporting and run focused config/sandbox/doctor
     tests, strict OpenSpec validation, and the normal repository gate.
