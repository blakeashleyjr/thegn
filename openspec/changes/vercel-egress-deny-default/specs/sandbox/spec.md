# Sandbox

## ADDED Requirements

### Requirement: Vercel sandboxes deny egress by default

A Vercel sandbox SHALL be created with a deny-all network policy unless the
user's own config sets `egress = "registries"` or `egress = "allow-all"` on its
`[env.<n>.provider]`, and thegn MUST apply the configured policy again each time
the sandbox resumes. `registries` allows only the built-in package-registry and
git-host list; `egress_allow` entries add hosts and MUST NOT accept `*` or a
port suffix. A repo overlay MUST NOT be able to set these keys.

#### Scenario: Default denies

- **WHEN** a vercel env sets neither `egress` nor `egress_allow`
- **THEN** the create request carries `networkPolicy` mode `deny-all`

#### Scenario: Resume re-applies the policy

- **WHEN** a stopped sandbox is resumed
- **THEN** the configured policy is sent before the session is handed out, and
  a failure to send it is an error
