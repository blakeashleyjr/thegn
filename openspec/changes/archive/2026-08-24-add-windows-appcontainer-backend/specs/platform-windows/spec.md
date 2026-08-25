# Platform: Windows

## ADDED Requirements

### Requirement: Windows panes can be confined by an AppContainer

thegn SHALL offer a native Windows containment backend that runs a pane's shell
under an AppContainer token: its own container SID, deny-by-default access to the
filesystem and registry, and network reachable only through capability SIDs. It
MUST require no VM, no image, and no path translation.

The backend's identity SHALL be deterministic per worktree, so that creation and
teardown agree without a lookup, and MUST fit the 64-character limit Windows
imposes on a profile name without allowing two worktrees to collide.

The pane's network policy SHALL map to capability SIDs, where "no network" is the
absence of any capability rather than a flag.

#### Scenario: A contained pane runs the worktree's shell

- **WHEN** a pane resolves to the AppContainer backend
- **THEN** its shell starts under the container token and can read and write the
  pane's pseudoconsole

#### Scenario: Two worktrees never share a container

- **WHEN** two worktrees under one repository resolve to the AppContainer backend
- **THEN** they receive different profiles, even when their paths share a long
  prefix and the full name would exceed the length limit

#### Scenario: The backend may now report itself available

- **WHEN** `appcontainer` probes on a Windows build where pane spawn does
  assign the pane's process to an AppContainer
- **THEN** it reports `Present`, satisfying rather than contradicting the
  earlier requirement that a backend applying no containment probe `Absent` —
  that requirement is conditional on the containment not being applied, and this
  change is what makes the condition false. `jobobject` stays `Absent`: it is a
  limits layer beneath this backend, not a boundary of its own.

### Requirement: The container token is applied through a trampoline

Because the pane's ConPTY spawn already owns its process-thread attribute list,
the security-capabilities attribute cannot be attached to it. The pane SHALL
therefore be launched through a thegn subcommand that re-launches the real shell
under the container token, inheriting the console it was given.

That indirection MUST remain visible in the launch argv, so the truth check can
confirm the containment rather than assume it.

#### Scenario: The trampoline is present in a contained pane's argv

- **WHEN** the AppContainer backend composes a pane's argv
- **THEN** the argv names the trampoline subcommand and the worktree's profile,
  and the truth check reads it as the AppContainer backend

### Requirement: Grants are attempted, never forced, and always reported

Deny-by-default means a pane cannot reach its own worktree or its toolchain until
the container SID is granted access. thegn SHALL attempt those grants and SHALL
NOT elevate to force one through.

A grant that fails for the **worktree** MUST be fatal for that backend — a pane
that cannot read its own files is not a sandboxed pane — and selection MUST fall
through to the next backend rather than start it. A grant that fails for a
**toolchain** MUST be surfaced as a warning naming the directory, the consequence,
and the exact command the user can run themselves.

#### Scenario: An unreachable toolchain is reported, not hidden

- **WHEN** thegn cannot grant the container access to a toolchain directory
- **THEN** the pane still starts and a warning names the directory and the command
  that would fix it

#### Scenario: An unreachable worktree falls through

- **WHEN** thegn cannot grant the container access to the worktree
- **THEN** the AppContainer backend is not used for that pane and selection
  continues down the chain
