# AI Gateway

## ADDED Requirements

### Requirement: Versioned skill packages register, install, and pin

The gateway SHALL maintain a registry of versioned `SKILL.md`-style packages where each package has a name, a semver version, an injectable body, and metadata, and it MUST support registering a package, installing it on demand, and resolving to a pinned version from a local cache so install-on-demand works offline.

#### Scenario: Install-on-demand resolves a pinned version

- **WHEN** a registered skill is requested by a task and is not yet installed
- **THEN** the gateway installs it, caches its body, and resolves to the pinned
  version on subsequent requests without re-fetching

#### Scenario: Invalid package is rejected

- **WHEN** a candidate package lacks a name or a valid semver version
- **THEN** registration fails and no entry is added to the registry

### Requirement: Skills are selected by task/context and injected per harness

Relevant skills SHALL be selected deterministically by matching a package's triggers against the task/context and filtering by per-scope enablement, and the selected blocks MUST be rendered per harness — in-process for the embedded termite harness, and over the ACP proxy plus MCP-over-ACP for foreign agents — so every harness inherits the same capability through one registry.

#### Scenario: Same context yields the same ordered selection

- **WHEN** `select_and_render` runs twice for the same task context, harness,
  and policy
- **THEN** it returns an identical, identically ordered set of capability blocks

#### Scenario: Foreign agent receives skills over its protocol

- **WHEN** a selected skill applies to a foreign agent reached over the ACP proxy
- **THEN** its block is rendered over ACP and any referenced MCP server is
  surfaced over MCP-over-ACP, not as an in-process block

### Requirement: Skill injection is cache-aware

Injected skill blocks SHALL ride a stable prefix ordering with cache breakpoints placed after the injected blocks, and selecting or adding a skill MUST NOT reorder the existing prompt prefix, so upstream prompt caching is preserved.

#### Scenario: Adding a skill preserves the cached prefix

- **WHEN** two requests differ only in that the second selects an additional
  skill
- **THEN** the prompt prefix bytes before the cache breakpoint are byte-identical
  across both requests

### Requirement: Injection is opt-in by policy and a no-op when AI is off

Skill injection SHALL be gated by a per-harness policy of transparent-passthrough versus managed, and the AI-free shell MUST build and run with the registry absent or disabled; registry and injection surfaces MUST be inert no-ops rather than errors when no agent or proxy is configured.

#### Scenario: Passthrough leaves the request untouched

- **WHEN** a harness's policy is transparent-passthrough
- **THEN** no skill block is injected and the model request is forwarded unchanged

#### Scenario: AI-off shell is unaffected

- **WHEN** the registry is disabled and no proxy is configured
- **THEN** the shell runs normally and registry/injection surfaces are inert
  rather than raising an error
