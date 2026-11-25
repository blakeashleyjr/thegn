## MODIFIED Requirements

### Requirement: Sprites and Vercel environments are available in the stable channel

`thegn job` and `thegn fleet list|cost|inspect` SHALL NOT be refused in the
stable channel; `thegn fleet` SHALL read only Sprites and Vercel environments
there. `thegn host` and `thegn placement` SHALL remain refused. The "New
environment…" wizard SHALL offer only the kinds the current channel allows:
`local`, `ssh`, `sprites` and `vercel` on stable (SSH added by `ship-ssh-clientserver-on-stable`).

#### Scenario: Fleet runs on stable

- **WHEN** the user runs `thegn fleet list` in the stable channel
- **THEN** it lists Sprites and Vercel sandboxes and does not exit with the
  dev-channel error

#### Scenario: Wizard offers stable kinds only

- **WHEN** the user opens "New environment…" in the stable channel
- **THEN** the kind cycle contains only `local`, `ssh`, `sprites` and `vercel`
