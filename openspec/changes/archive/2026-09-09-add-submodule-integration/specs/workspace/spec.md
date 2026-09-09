# Workspace

## ADDED Requirements

### Requirement: Workspace clone initializes submodules through the shared seam

Workspace creation SHALL perform its ordinary superproject clone and then
invoke the shared strict, configuration-controlled, trust-gated recursive
submodule initializer. It MUST NOT bypass that trust/failure contract by
implicitly recursing repo-controlled submodule URLs during the clone itself.
Remote/provider creation SHALL provide the same accepted/pending/degraded
outcome semantics.

#### Scenario: Clone succeeds while init degrades

- **WHEN** the superproject clone succeeds but an approved recursive submodule
  update fails
- **THEN** the workspace remains registered and the initialization failure is
  surfaced for recovery
