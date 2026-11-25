## 1. Hidden paths

- [x] 1.1 Add `sandbox_hidden::standard_hidden_paths`: credential locations,
      non-public `~/.ssh` entries and sibling git repositories, canonical and
      de-duplicated; never home, its ancestors or the Nix store.
- [x] 1.2 Add `SandboxSpec::hidden_paths`, filled by the resolver for local
      `standard` specs on bwrap, OCI and systemd with worktree file access.
- [x] 1.3 Mask them in the bwrap argv and the local podman/docker argv
      (tmpfs or `/dev/null` bind, explicit mounts honoured) and with
      `InaccessiblePaths=` on systemd.
- [x] 1.4 Fingerprint the list in the OCI policy fingerprint.
- [x] 1.5 Stop binding `~/.ssh` identity-key targets for `standard`.

## 2. Agent settings read-only

- [x] 2.1 Carve only agent runtime state (`agent_runtime_state_paths`) in the
      default writable carve-outs and the bundle's unmanaged-agent path.

## 3. Explain the boundary

- [x] 3.1 Update the `standard` explanation used by the new-worktree dialog
      and show the selected profile's explanation on the first-run sandbox
      step.
- [x] 3.2 Update `sandboxing`, `sandbox-profiles`, `sandbox-security-model`
      and `known-limitations`.

## 4. Verification

- [x] 4.1 Unit tests: hidden-path planning, mask translation, systemd
      properties, fingerprint, runtime-state carve, explanation text.
- [ ] 4.2 Run the `thegn-core` and `thegn-host` test suites and `just ci` on
      the remote builder.
- [ ] 4.3 Live check on bwrap: `cat ~/.aws/credentials`, `ls ~/.ssh` and a
      sibling repository in a standard pane show nothing; git over ssh works
      through the agent; `claude` still writes its todos.
