# macOS app launcher

## ADDED Requirements

### Requirement: macOS offers a rehearsed batteries-included artifact

The macOS batteries artifact SHALL compose the Thegn binary, an explicitly
selected terminal, Fira Code Nerd Font (or a documented equivalent), and a
generated writable terminal configuration into one launcher/app flow. The
launcher SHALL use THE-52's verified release binary, preserve existing terminal
preferences, and pass a clean-host launch rehearsal before publication.

#### Scenario: Finder launch uses bundled components

- **WHEN** a user opens the batteries app on a clean supported macOS host
- **THEN** its launcher starts the bundled/configured terminal with the
  generated config and runs the verified Thegn binary without requiring prior
  terminal or font installation

#### Scenario: Existing Alacritty config is preserved

- **WHEN** the user already has an Alacritty configuration
- **THEN** the artifact uses its own writable generated copy and does not
  overwrite the user's file
