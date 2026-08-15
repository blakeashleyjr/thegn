# LLM Proxy

## ADDED Requirements

### Requirement: Provider metadata comes from a single declarative registry

The proxy SHALL resolve every backend's base URL, auth environment variable,
default model, and request transforms from a single declarative `ProviderInfo`
registry, and both the configuration surface and the runtime client construction
MUST derive from that same registry so they cannot disagree. Every registry entry
MUST have a unique id and a non-empty base URL and auth env var; a lookup of an
unknown provider id MUST error rather than silently misconfigure.

#### Scenario: A route inherits its provider's defaults

- **WHEN** a proxy route names a provider without specifying a model or base URL
- **THEN** it inherits the provider's default model and base URL from the registry

#### Scenario: An explicit override wins over the registry default

- **WHEN** a route names a provider but specifies its own model
- **THEN** the route's model is used instead of the registry default

#### Scenario: An unknown provider is rejected

- **WHEN** a route names a provider id not present in the registry
- **THEN** configuration resolution errors instead of constructing a client

#### Scenario: Config and client read the same source

- **WHEN** a provider's registry entry defines its base URL and transforms
- **THEN** both the config validation and the constructed backend client use that
  entry's values
