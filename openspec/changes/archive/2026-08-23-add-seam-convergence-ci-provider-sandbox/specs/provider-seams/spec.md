## ADDED Requirements

### Requirement: The managed-provider vocabulary is one enum

The managed-sandbox provider kinds (`[env.<name>.provider] provider`) SHALL be declared once as `config_enum! EnvProviderKind`, and every "is this kind a VPS / native-exec / ssh-reached / scale-to-zero / self-suspending" question SHALL be a method on it; the host provider factory MUST match the enum exhaustively so a new kind without a factory arm fails to compile.

#### Scenario: New provider kind

- **WHEN** a variant is added to `EnvProviderKind`
- **THEN** `provider_for_named` fails to compile until it has an arm
