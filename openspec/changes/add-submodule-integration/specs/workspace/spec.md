# Workspace

## ADDED Requirements

### Requirement: Workspace clones recurse submodules

Creating a workspace from a git URL SHALL clone with submodules recursed
when `[git] submodules = "auto"`, and the remote provision script SHALL
initialize submodules after its clone under the same setting, so a fresh
workspace on any host starts with a populated checkout. With `"off"`, both
paths clone exactly as today.

#### Scenario: URL clone arrives populated

- **WHEN** a workspace is created from a URL whose repo carries submodules
  under the default setting
- **THEN** the resulting checkout's submodules are initialized without a
  manual `submodule update` step

#### Scenario: Off restores plain clones

- **WHEN** `[git] submodules = "off"`
- **THEN** local and remote provisioning clone without any submodule step
