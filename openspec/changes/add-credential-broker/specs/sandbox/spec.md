# Sandbox

## MODIFIED Requirements

### Requirement: Provider secrets resolve through a layered store

thegn SHALL resolve every provider token through the credential broker's
single chokepoint, which accepts a typed `SecretRef`: `keyring:<account>` (OS
keyring), `env:VAR`, `file:PATH` (a `0600` file), and a bare string treated as
`env:` for back-compat on `api_key_env`-family fields. The broker's backends
are a `SecretStore` provider seam with per-backend probes in `thegn doctor`,
and each resolution emits a value-free audit event naming the consumer. A
writer path SHALL persist a collected token — preferring the OS keyring and
falling back to a `0600` file under the config dir — and return the ref to
store in config. Resolution MUST degrade gracefully (keyring → file → env) so
a host with no Secret Service never wedges a launch, and secrets MUST NOT be
echoed or written into config in plaintext.

#### Scenario: A stored token launches a provider env without an exported var

- **WHEN** a token is stored via the writer path and its `SecretRef` is written
  into `[env.<name>.provider]`
- **THEN** a later provision resolves the token through the broker chokepoint
  and launches the env without the user exporting an environment variable

#### Scenario: Missing keyring falls back without wedging

- **WHEN** the broker is asked for a `keyring:` ref on a host with no Secret
  Service
- **THEN** it degrades to the file/env layers (or returns none actionably)
  rather than blocking or crashing the launch

#### Scenario: Existing bare/env configs keep working

- **WHEN** an existing config names a token as a bare env-var (e.g.
  `api_key_env = "FLY_API_TOKEN"`)
- **THEN** the broker treats it as `env:` and the env launches unchanged

#### Scenario: Provider resolution is audited

- **WHEN** a provider env resolves its token during provisioning with tracing
  enabled
- **THEN** an audit event records the ref name, backend, and provider consumer
  tag, and contains no token bytes
