# Sandbox

## ADDED Requirements

### Requirement: Provider secrets resolve through a layered store

superzej SHALL resolve every provider token through a single `secret::resolve`
chokepoint that accepts a layered `SecretRef`: `keyring:<service>/<account>`
(OS keyring), `env:VAR`, `file:PATH` (a `0600` file), and a bare string treated
as `env:` for back-compat. A writer path SHALL persist a collected token —
preferring the OS keyring and falling back to a `0600` file under the config
dir — and return the ref to store in config. Resolution MUST degrade gracefully
(keyring → file → env) so a host with no Secret Service never wedges a launch,
and secrets MUST NOT be echoed or written into config in plaintext.

#### Scenario: A stored token launches a provider env without an exported var

- **WHEN** a token is stored via the writer path and its `SecretRef` is written
  into `[env.<name>.provider]`
- **THEN** a later provision resolves the token through `secret::resolve` and
  launches the env without the user exporting an environment variable

#### Scenario: Missing keyring falls back without wedging

- **WHEN** `secret::resolve` is asked for a `keyring:` ref on a host with no
  Secret Service
- **THEN** it degrades to the file/env layers (or returns none actionably)
  rather than blocking or crashing the launch

#### Scenario: Existing bare/env configs keep working

- **WHEN** an existing config names a token as a bare env-var (e.g.
  `api_key_env = "FLY_API_TOKEN"`)
- **THEN** `secret::resolve` treats it as `env:` and the env launches unchanged

### Requirement: Environments are authored without hand-editing TOML

superzej SHALL provide a write path that creates, edits, and removes
`[env.<name>]` / `[env.<name>.provider]` definitions with comments and
formatting preserved, plus a generic `config set <dotted.key> <value>`. Env
definitions SHALL be written only to global config; a repo `.superzej.toml` may
only _select_ an env (`env = "…"`), and the write path MUST refuse an env
definition in a repo scope (the trust-clamp model). A CLI (`superzej env
create`/`rm`/`test`, `config set`) SHALL back these operations and be usable
headlessly.

#### Scenario: `env create` writes config and stores the secret

- **WHEN** `superzej env create <name> --provider fly --token-file <path>` runs
- **THEN** the env is written to global config, the token is stored via the
  secret writer, and no secret is printed

#### Scenario: A repo file cannot define an env

- **WHEN** the write path is asked to define `[env.<name>]` in a repo
  `.superzej.toml`
- **THEN** it refuses, allowing only the `env = "…"` selection key

#### Scenario: `env test` verifies a token cheaply

- **WHEN** `superzej env test <name>` runs against a configured env
- **THEN** it builds the provider and performs a cheap `list()` call, reporting
  success or an actionable failure without provisioning anything

### Requirement: Environments are creatable and manageable from the TUI

superzej SHALL surface environment setup in the compositor: an "Add
environment" wizard (reached from the palette) that branches its fields by kind
(`local`/`ssh`/`fly`/`digitalocean`/`hetzner`/`daytona`), accepts a pasted
token, validates it off-loop, and on submit writes the env + stores the secret;
and a System-tab `Environments` panel section listing every configured `[env.*]`
with a token-status glyph and row actions to bind the env to the current
worktree, test it, remove it, and open the wizard. Off-loop validation MUST feed
back over the refresh channel and pulse the waker (no idle polling).

#### Scenario: Creating an env from the wizard binds it to a worktree

- **WHEN** the user opens the Add-environment wizard, picks a cloud kind, pastes
  a token, and submits with bind-to-current-worktree
- **THEN** the env is written, the token stored, and the current worktree is
  bound to that env

#### Scenario: The Environments panel row actions manage a live env

- **WHEN** the user selects an env row in the System ▸ Environments section
- **THEN** `enter` binds it to the current worktree, `t` tests it off-loop
  (status reported via a toast), `x` removes the env and forgets its secret, and
  `n` opens the wizard

#### Scenario: Panel token status reflects the secret store

- **WHEN** the Environments section renders a configured env
- **THEN** its glyph shows whether a token resolves (present) or is missing,
  computed off-loop during hydration
