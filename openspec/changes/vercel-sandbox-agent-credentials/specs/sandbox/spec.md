# Sandbox

## ADDED Requirements

### Requirement: Model credentials default to none

thegn SHALL send no model credential into a Vercel sandbox unless the user's
own config sets `model_credentials = "upload"` for that env.

#### Scenario: Default sends nothing

- **WHEN** a vercel env does not set `model_credentials`
- **THEN** no login file is uploaded and doctor prints the `none` disclosure

### Requirement: Upload is narrow and explicit

In `upload` mode thegn MUST upload only in a terminal or headless session
opened by a user action, only after egress is narrowed to the model hosts and
the env's allowed domains, only to a tmpfs directory, with dir mode 0700 and
file mode 0600, and MUST NOT upload in capture mode, the reaper, provisioning,
doctor, fleet or any read.

#### Scenario: Egress narrowing fails

- **WHEN** the egress policy cannot be applied
- **THEN** nothing is uploaded

#### Scenario: Not a tmpfs

- **WHEN** `/dev/shm` is not a tmpfs or a file mode check fails
- **THEN** the upload is refused and the directory is removed

### Requirement: Unsupported modes are refused

thegn MUST refuse `proxy` and `inject` for vercel with distinct messages, and
MUST refuse any value but `none` on a non-vercel kind.

#### Scenario: Proxy refused

- **WHEN** a vercel env sets `model_credentials = "proxy"`
- **THEN** provider admission refuses it

### Requirement: Orphan reap is a dry run by default

`thegn env reap --provider vercel` SHALL list orphans without deleting, and
`--apply` MUST delete only sandboxes tagged for this controller, with no ledger
record, that pass the tag proof, and MUST refuse on a partial listing.

#### Scenario: Apply without a tty

- **WHEN** `--apply` runs with no tty and no `--yes`
- **THEN** it refuses

### Requirement: Unpulled marker

thegn SHALL write an unpulled marker when a terminal session ends and MUST
clear it on a successful `thegn env pull`.

#### Scenario: Pull clears it

- **WHEN** `thegn env pull` succeeds for a marked sandbox
- **THEN** the marker is removed and `results not pulled` disappears
