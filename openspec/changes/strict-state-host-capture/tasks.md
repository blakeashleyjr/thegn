## Source

- [x] Add static typed logically read-only WAL capture through THE-602's strict transaction and decoder, without exposing a connection.
- [x] Add the private Linux inspected-namespace helper and unsupported-platform refusal.
- [x] Retain absence witnesses, reverify before publication, and refuse orphan WAL/SHM/journal objects.
- [x] Author actual filesystem, permission-restoration, absence/race, special-file, WAL, schema, no-mutation and busy fixtures.
- [x] Document stable-namespace assumptions, permitted sidecar activity and unwired integration limits.

## Verification and delivery

- [ ] Complete current primary and independent source review and address findings.
- [ ] Run current focused core and Linux host fixtures plus THE-602 compatibility coverage through the coordinated native runner; report exact selectors and actual results.
- [ ] Run required strict lint, source/OpenSpec/delivery gates and component checks on the final source.
- [ ] Land the scoped implementation on local main and reconcile THE-603 evidence without closing THE-592 or claiming startup integration.

Historical tests of the retained candidate are not acceptance evidence for this
revised source. Native execution, current compile proof, and local landing are
pending. No live DB, startup, receiver, worker or launch path is activated.
A future PR must run `just ci` as required by the repository workflow; no PR or
full CI execution is claimed here. See the dated THE-603 audit for the exact
fixture boundary and remaining native/platform gates.
