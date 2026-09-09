# Rendering — schema refusal truthfulness delta

## ADDED Requirements

### Requirement: Runtime schema refusal remains visible and preserves known state

The compositor SHALL propagate a typed shared-DB schema refusal from hydration,
retain its last successfully hydrated model, disable incompatible mutations, and
render a persistent actionable indicator naming the observed and build schema
versions plus the rebuild/reinstall and controller-restart remedy. It SHALL NOT
replace known state with an unlabeled empty/fallback model. With no prior model,
it SHALL render explicit unavailability rather than an apparently successful
empty state. Generic database failures SHALL remain distinguishable from schema
refusals. The indicator SHALL clear only after a successful compatible
hydration.

#### Scenario: The schema changes beneath a running compositor

- **WHEN** periodic hydration receives a schema-policy or compatibility refusal
- **THEN** the previous model remains visible and a sticky indicator names the mismatch and recovery action

#### Scenario: A controller restores compatibility

- **WHEN** a later hydration opens the DB compatibly and rebuilds the model
- **THEN** the compositor applies the new model, re-enables valid mutations, and clears the indicator

#### Scenario: Startup has no last-known model

- **WHEN** the first hydration returns a typed schema refusal
- **THEN** the view labels state as unavailable and shows the persistent schema remedy instead of presenting an unexplained empty sidebar
