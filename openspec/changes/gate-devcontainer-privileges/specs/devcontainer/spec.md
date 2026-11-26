## MODIFIED Requirements

### Requirement: Unsafe, reserved, editor-only, and unknown fields have distinct outcomes

thegn MUST refuse `privileged`, `capAdd`, `securityOpt`, `runArgs`, and `init`
without creating an approval path or passing them to the CLI provider. It MUST
also refuse bind mounts whose resolved source equals, contains, or follows a
symlink to a container-engine socket or thegn control/state directory. Such a
mount MUST be omitted from the sandbox mount list and from the generic
`devcontainer.mounts` approval request, and the refusal MUST be reported with
text beginning `refused: engine-socket`. The refusal remains unconditional
until a per-repository privilege grant exists.

Thegn SHALL report recognized reserved repo fields, including
`hostRequirements`, port attributes, `waitFor`, `userEnvProbe`,
`shutdownAction`, remote-UID settings, `secrets`, `workspaceMount`,
`overrideCommand`, compose override, and user settings beyond feature-user
seeding. It SHALL ignore editor-only `customizations` without warning and SHALL
report any other unknown top-level key as unknown and reserved. This inventory
SHALL describe the fields known to this thegn version and MUST NOT promise
automatic parity with the complete or future containers.dev reference.

Image-label `devcontainer.metadata` inspection and merging SHALL remain
reserved. Because selection and doctor do not inspect images, thegn MUST NOT
claim to detect that label.

#### Scenario: Privileged cannot be approved

- **WHEN** a devcontainer declares `privileged: true`, even alongside approved
  categories
- **THEN** privileged mode is not applied or passed to the CLI and the field is
  reported as refused for weakening isolation

#### Scenario: Unknown fields do not silently execute

- **WHEN** a selected file contains an unrecognized top-level field
- **THEN** the field is not applied and is reported as unknown and reserved

#### Scenario: Editor customization stays silent

- **WHEN** a selected file contains `customizations.vscode`
- **THEN** it is ignored without a warning

#### Scenario: Protected bind is not covered by generic mount approval

- **WHEN** a devcontainer declares a bind of an engine socket, an ancestor of
  one, or thegn's control/state directory
- **THEN** the bind is omitted, excluded from `devcontainer.mounts`, and
  reported with text beginning `refused: engine-socket`
