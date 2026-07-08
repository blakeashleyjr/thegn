# Tasks

## 1. Record the posture

- [ ] 1.1 Author the `agent-containment` capability spec (sandbox-by-default,
      no-telemetry, viewer/VCS scope) as ADDED policy requirements.
- [ ] 1.2 Cross-reference the affirmed items in the proposal Impact: AJ 778
      (this stance), affirming AJ 441 (no-telemetry) and AJ 443 (sandbox-by-default);
      relate the Bouncer agent and the LLM-proxy chokepoint.

## 2. Affirm against existing surfaces

- [ ] 2.1 Confirm `crates/superzej-core/src/sandbox.rs` backend selection
      (`podman` → `docker` → `bwrap` → `none`) and the `--no-sandbox` opt-out
      (item 362) match the sandbox-by-default requirement; note the explicit + logged
      opt-out path.
- [ ] 2.2 Confirm there is no off-machine telemetry transmit path in the binary
      (only user-configured endpoints: git remotes, GitHub, the LLM proxy).
- [ ] 2.3 Confirm editing is delegated to `$EDITOR` and that no embedded editor /
      browser / computer-use surface exists.

## 3. Guard tests

- [ ] 3.1 Add a guard test asserting an agent worktree spec is containerized unless
      `--no-sandbox` is explicitly set, and that the opt-out is logged.
- [ ] 3.2 Add a guard test asserting the absence of an off-machine telemetry
      transmit path (no unconfigured outbound endpoint).

## Validate

- [ ] Run `just ci`
