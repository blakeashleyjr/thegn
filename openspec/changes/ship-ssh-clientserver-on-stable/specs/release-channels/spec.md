## ADDED Requirements

### Requirement: SSH remote workspaces are available in the stable channel

In the `stable` channel the config clamp SHALL keep `[sandbox.remote]` and
every `[env.<name>]` whose `placement` is `ssh` (including the `mosh` and
`remote` aliases) and that names no dev-only provider. It SHALL still remove
every Kubernetes env. The `remote` feature SHALL remain experimental and name
the dev-only remainder (Kubernetes placement and direct iroh) in `thegn
doctor`. The New-environment wizard SHALL offer the `ssh` kind in the stable
channel.

#### Scenario: Stable keeps an SSH env and drops a Kubernetes env

- **WHEN** a stable binary loads `[env.box]` with `placement = "ssh"` and
  `[env.pod]` with `placement = "k8s"`
- **THEN** `box` remains configured, `pod` is removed and the clamp reports the
  `remote` feature as disabled

#### Scenario: Client/server mode is not gated

- **WHEN** the user runs `thegn serve` or `thegn pair` in the stable channel
- **THEN** the command runs; only `serve --iroh-direct` and `pair direct` are
  refused with the dev-channel error
