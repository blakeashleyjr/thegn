## MODIFIED Requirements

### Requirement: Every configured provider reports a probe

Every provider implementation SHALL implement `Probe`, returning a `ProbeReport` with the seam name, provider id, availability, serialized caps and notes. A registry SHALL construct every provider the loaded config selects — covering ci, forge, issues, calendar, git, editor, sandbox and media — and collect their reports, and `thegn doctor` MUST print them as a "Providers" section in both text and `--json` (key `providers`) output.

#### Scenario: Doctor lists a reserved selection as unavailable

- **WHEN** config selects a reserved kind and `thegn doctor --json` runs
- **THEN** the `providers` array contains an entry for that seam whose availability is `Unavailable` with a reason naming the reserved kind

#### Scenario: Doctor lists a missing binary as unavailable

- **WHEN** the resolved provider needs a CLI binary that is not on `PATH`
- **THEN** its probe reports `Unavailable` naming the binary, and doctor's exit status is unaffected by that entry

#### Scenario: Editor probe names the winning layer

- **WHEN** the editor resolves through the environment layer
- **THEN** its report has seam `editor`, an id naming the layer (`template`/`tool`/`visual`/`env`/`vi`), and a note carrying `[editor] open_in` and line-jump capability
