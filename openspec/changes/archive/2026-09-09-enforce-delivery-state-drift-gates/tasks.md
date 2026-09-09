# Tasks

- [x] 1. Define and validate checked-in change-centric and issue-centric delivery
     schemas.
- [x] 2. Populate every active delivery change and reviewed active delivery issue
     with bidirectional Linear issue/project/change links or an explicit
     no-spec/no-project rationale.
- [x] 3. Add offline lifecycle checks for active, archived, missing, and stale
     references.
- [x] 4. Remove hard-coded live pass-count claims and reject new ones in roadmap
     and release-status documentation.
- [x] 5. Derive/ratchet plugin API version, accepted extension points, and
     control scopes into public documentation checks.
- [x] 6. Keep generated control schema and surface-gap ratchet verification in
     the normal nextest gate and link failures to the owning capability.
- [x] 7. Add the maintainer closure checklist and an offline/optional-Linear
     reconciliation report.
- [x] 8. Add fixtures for delivered-but-active, active-without-issue,
     issue-without-change, asymmetric issue/change ownership,
     archived-but-referenced, docs-version drift, credential-free operation,
     optional snapshot membership drift, active-issue-to-archived-change drift,
     and an overdue archive reconciliation window.
- [x] 9. Run strict OpenSpec validation and the normal repository gate.
  - [x] 9.1 Run strict validation for this change.
  - [x] 9.2 Run the normal nextest/repository integration gates (7,841
        workspace tests passed after the integration ratchets were reconciled).
