# Tasks

- [x] Add bounded built-in and plugin identity admission helpers and focused
  compatibility tests.
- [x] Validate GitHub authorities, scoped/bare ids, and provider responses
  before `gh` arguments or domain conversion.
- [x] Validate Jira and Kaneo ids before REST path construction and response
  conversion while preserving existing endpoint shapes.
- [x] Encode complete issue identities once in control client paths, validate the
  decoded identity in server handlers, and reject plugin inputs/responses whose
  exact namespace does not match the bridge.
- [x] Exercise bounded zero-effect fake-provider routing, server-decoded control
  identity round-trip, strict response-field admission, GitHub flag/URL forms,
  and structured Kaneo path/query construction.
- [ ] Run the existing bounded fake process/request selectors, strict
  OpenSpec/delivery checks, and native provider matrix after source review.
