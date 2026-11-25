# Sandbox

## ADDED Requirements

### Requirement: Provider provisioning overlaps the repository and Nix installs

When a provider env plan has a repository step (`bundle_seed` or `clone`)
followed by steps that only touch the checkout, and then the Nix and direnv
install steps, thegn SHALL run those two lanes at the same time. Each lane MUST
keep its own step order, and no later step MAY start until both lanes have
finished. A fatal failure in either lane MUST stop both lanes before their next
step and fail the provision. The overlap MUST NOT cross the split between
foreground and deferred steps.

#### Scenario: Seed and Nix install run together

- **WHEN** a seeded worktree with a flake dev shell is provisioned
- **THEN** the repository seed and the Nix install are both running, and the
  dev shell build starts only after the seed, local parity, Nix and direnv
  steps have finished

#### Scenario: A failed seed stops the Nix lane

- **WHEN** the repository seed fails while the Nix install is running
- **THEN** the Nix verify and direnv steps never start and the provision fails
  with the seed's error
