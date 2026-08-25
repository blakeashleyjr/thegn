# Tasks — agent harness seam

## 1. The seam (thegn-core)

- [ ] 1.1 New `thegn_core::harness`: object-safe `Harness` trait, `HarnessCaps`
      bitset, `HomeSpec`, `SessionLayout`, `SessionRecord`, closed `HARNESSES`
      registry with `harness(id)` lookup. No `async fn` (provider-trait
      ratchet); no I/O.
- [ ] 1.2 `claude` and `codex` impls absorbing the existing knowledge
      (`account::PROVIDERS` row, `headless_command` arm, `parse_claude_usage` /
      `parse_codex_rollup`, session-store layout); `aider` (headless only) and
      `antigravity` (usage only) with honest cap bits.
- [ ] 1.3 Cross-harness conformance tests (every RESUME impl quotes its id;
      every SESSIONS impl's globs match its fixture; caps ⇔ ops agreement) +
      per-impl unit tests moved with the code (95% core line gate).
- [ ] 1.4 Delegate the scattered sites through the seam, behavior-identical:
      `agent_task::headless_command`, `usage::parse_*`, and keep their existing
      tests green unchanged.

## 2. I/O drivers (thegn-svc / thegn-host)

- [ ] 2.1 Generic session-store walker in `thegn-svc` driven by
      `SessionLayout` (bounded file count, mtime-sorted); refactor the usage
      gather/token rollup walkers onto it.
- [ ] 2.2 `daemon/agent_open.rs::bare_provider` → registry lookup; sandbox
      login-carry reads the auth-critical list from `HomeSpec`.
- [ ] 2.3 `thegn doctor`: one probe row per configured harness (binary on
      PATH, home present, auth marker, session store found), following the
      Probe shape contract.

## 3. Session history + resume

- [ ] 3.1 Capability row `agent.sessions` (Read): `Verb` variant + `Verb::ALL`,
      `required_scope` arm, `cap(...)` row, `ControlApi` method, HTTP route —
      implement on HTTP/CLI/MCP; record a `SURFACE_GAPS` entry for gRPC/plugin
      if not mirrored (catalog tests enforce either way).
- [ ] 3.2 `thegn agent sessions [--worktree <w>] [--harness <id>] --json`
      following the one-emitter JSON convention.
- [ ] 3.3 `AgentLaunch.resume: Option<String>`; validate the id shape, resolve
      through `Harness::resume_command`, refresh the pinned `control_schema`
      snapshot.
- [ ] 3.4 Per-`[[agents]]` `resume = false` config key; resurrection
      auto-resume with cold-launch fallback; document the key in
      `config/config.toml.example`.
- [ ] 3.5 Unit tests: resume command resolution, invalid-id refusal,
      auto-resume fallback ordering (pure decision logic in core).

## 4. Scoped MCP control

- [ ] 4.1 Rebase on the in-flight MCP write-tools branch (scoped state tools);
      do not fork its `--scopes` machinery.
- [ ] 4.2 Config keys `[mcp.serve] scopes` + profile/workspace overlays;
      clamp-only resolution (pure, unit-tested: inner may only narrow;
      unparseable level contributes the empty set); `--scopes` intersects last.
- [ ] 4.3 Startup + `thegn doctor` print the effective scope set and the
      clamping level; document the keys in `config/config.toml.example`.

## 5. Docs + gates

- [ ] 5.1 Update `docs/help/ai-usage.md` / agent help pages for `agent
sessions`, `resume`, and the scoped-serve keys (help ratchet: any new
      action id must be claimed and mentioned).
- [ ] 5.2 `docs/extending/`: how to add a harness (the seam recipe).
- [ ] 5.3 Run `just ci` once at the end (includes openspec validate, catalog
      ratchets, control-schema snapshot, coverage).
