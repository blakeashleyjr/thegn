# Plugin API — localization delta

## ADDED Requirements

### Requirement: Plugin-visible strings use a versioned localization contract

A plugin UI contribution SHALL identify user-visible text with a namespaced
stable key and fallback text through a versioned plugin API contract. The host
SHALL select the active locale and fallback; a plugin MUST NOT mutate the host's
global Fluent resources. The contract SHALL define compatibility, size/budget,
missing-key, and sanitization behavior before the surface is advertised as
supported.

#### Scenario: Translation is missing

- **WHEN** a plugin contribution has no value for the host's active locale
- **THEN** the host renders the registered fallback text within the surface's
  normal layout budget

#### Scenario: Plugin text cannot inject terminal control

- **WHEN** a plugin-provided localized value contains terminal escape or unsafe
  bidi control characters
- **THEN** the host sanitizes or rejects it before composing chrome
