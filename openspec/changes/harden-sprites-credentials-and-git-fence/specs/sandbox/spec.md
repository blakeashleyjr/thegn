# Sandbox

## ADDED Requirements

### Requirement: Sprites model credentials are an explicit grant

thegn SHALL accept `model_credentials` of `none` or `upload` for Sprites and refuse
`proxy` and `inject`. Under `none` it MUST NOT send the fixed model credential variables
or agent login files into the sandbox. An env that leaves the key unset SHALL ask once per
project and provider on the first interactive launch, and forward only after approval. With
no UI and no remembered answer it MUST withhold, never block, and print the same one-line hint.

#### Scenario: Sprites none withholds

- **WHEN** a sprites env sets `model_credentials = "none"`
- **THEN** provisioning, pane and repo exec environments omit the model credential variables, the agent login step and resync are skipped, and one line names what was withheld

#### Scenario: Upload sends

- **WHEN** a sprites env sets `model_credentials = "upload"`
- **THEN** credentials and logins are forwarded as before

#### Scenario: Unset asks once per project

- **WHEN** a sprites env leaves `model_credentials` unset and no answer is remembered and a UI is active
- **THEN** the launch pauses and one menu names the agents and fixed model credential variables, without reading their values
- **AND** "forward and remember" forwards them and stores the answer, "not now" withholds for this launch only, and "never" withholds and stores that answer
- **AND** a later launch on the same project and provider reuses the stored answer without asking

#### Scenario: Unset headless withholds

- **WHEN** a sprites env leaves `model_credentials` unset and no answer is remembered and no UI is active
- **THEN** provisioning withholds the credentials, skips the login step and resync, and prints one line naming what was withheld and the `model_credentials` key

### Requirement: Sprites destroy is done only on proof

After an accepted DELETE thegn SHALL poll the sprite every 2 seconds for up to 30 seconds
until a definite 404 before revoking custody and forgetting the record.

#### Scenario: Destroy proof

- **WHEN** the sprite still answers after 30 seconds
- **THEN** the record is kept and destroy returns an error

### Requirement: Provider exec has an overall deadline

`run_exec` SHALL fail after 31 minutes and Sprites SHALL send `Close`.

#### Scenario: Exec deadline

- **WHEN** an exec runs past the deadline
- **THEN** it returns a timeout error and the session is closed

### Requirement: Close resolves before it forgets

Closing a worktree SHALL capture its location, repo root and env before deleting the row.

#### Scenario: Close snapshot

- **WHEN** a worktree is closed
- **THEN** the background close and auto-checkpoint use the captured snapshot

### Requirement: Search is literal by default

Search SHALL match literally with smart case, ranked by match quality then recency, and
fuzzy matching SHALL be toggled with `Alt+f`.

#### Scenario: Search literal

- **WHEN** the overlay opens and the user types a query
- **THEN** matches are literal substrings and the footer offers `Alt+f fuzzy`
