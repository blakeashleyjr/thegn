# THE-322 v4 integrated independent adversarial source review

Date: 2026-09-15
Reviewer: Luna high
Scope: THE-322 identity, provider authority, plugin routing, control roundtrip, and Kaneo path integration.

## Verdict

**APPROVED — scoped source review of the frozen integrated THE-322 source.**

Reviewed the original baseline `3f365bfe56a7577dd8e051c56f5f0a4e81ade02e`, the private implementation `e93821e3e4983e96d886e9f7aadab1150ba19304`, and the final combined source `7ef9c3c6c0ab5637fc913e1b1b94a0d6a799cfa0` (tree `32c1ebc922c7f35bfee7bbc7c048d6ac35553a0b`). This review was source-only: no Cargo, build, test execution, native/provider execution, authenticated service, canonical, or Linear operation.

## Findings

- **Provider identity admission is closed at the relevant boundaries.** `identity.rs` separates delimiter-free plugin namespaces from opaque plugin keys; built-in IDs reject control, whitespace, traversal, option-like, and path/query delimiter forms; GitHub numbers and repository/host forms are bounded and canonical; Jira project/key and Kaneo IDs are checked. `validate_issue_identity` validates provider/id agreement, provider-specific number/key, project IDs, blocked links, and public URL authority before router/cache/panel use.
- **GitHub authority is admitted before subprocess effects in the final integration.** List and search derive the effective host from filter/configured repository scope before `gh`; scoped get/update IDs derive their host before `gh`. Response URLs are checked against that admitted host and exact repository issue/PR route. Query and fragment are retained as legitimate browse URL components, while userinfo, invalid schemes, ports, and foreign authorities are refused. Configured `extra_flags` are documented list/search flags and are applied in those operations.
- **Plugin namespace and opaque-key routing are coherent.** `PluginIssueBackend::new`, response validation, `IssueRouter::push_backend`, and identity parsing all reject a nested namespace such as `plugin:demo:extra` while preserving colon, slash, Unicode, and other non-control bytes in the opaque key. Registration rejects before insertion. The router test proves a key containing an additional colon routes to the complete registered namespace.
- **The control roundtrip is an actual router fixture.** `control/tests.rs:1449` uses the shipping client encoder, constructs requests, and sends them through the real Axum router and handlers with `oneshot`. It verifies GitHub and opaque plugin IDs survive one decode and reach the expected API operation; malformed encoded IDs return `BAD_REQUEST` with zero API calls.
- **Kaneo path construction is unified.** All `get`, JSON, and delete helpers call `api_path`; route components and query key/value components are checked before structured URL encoding. The shared HTTP URL join preserves an admitted base path and prevents origin escape. The producer task URL uses structured path segments/query pairs, and the actual task-to-domain fixture sends its query-bearing URL through `validate_issue_identity`.

## Coverage note

The Kaneo unit coverage checks `api_path` encoding/traversal and the shared HTTP client independently checks base-path joining; there is no single live fake-request assertion combining a Kaneo backend configured with a non-root base path and a Unicode/query-bearing endpoint. This is a useful additive fixture, but source review found no defect or unbounded path/authority path in the current implementation.

## Review limits

THE-321 owns shared HTTP transport limits and diagnostics; THE-314 owns `gh` subprocess resource limits; THE-324 owns account/generation authority. This report does not approve those scopes or any native/provider gate.

## Exact provenance

- Baseline: `3f365bfe56a7577dd8e051c56f5f0a4e81ade02e`
- Private THE-322 source: `e93821e3e4983e96d886e9f7aadab1150ba19304`, tree `4814b369ff6d9e5963e737bd7d2288d87a98d61c`
- Integrated final: `7ef9c3c6c0ab5637fc913e1b1b94a0d6a799cfa0`, tree `32c1ebc922c7f35bfee7bbc7c048d6ac35553a0b`
- Primary v3 review SHA-256: `2120c6a35415521a2a2204e542704cfd0890694889473d0e576f4caccb956817`
- Candidate v4 report SHA-256: `a7d794d609ba992a573ac55e630c0db1db794a43eb6998763e69236d1a158857`
- Approved plan SHA-256: `549382ff3b90df0e789bf8c86b0fa3c94e3c6b96276b774099da33f77e90ae6c`

Final integrated relevant-file SHA-256 values are in the accompanying JSON.
