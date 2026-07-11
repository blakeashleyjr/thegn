# Tasks

## 1. Operating-agent definitions (thegn-core)

- [ ] 1.1 `operating_agent.rs`: parse a markdown+front-matter definition into
      `OperatingAgent { id, title, tools, model, system_prompt }`; resolve/layer
      `.thegn/agents/*.md` (repo) over the global agents dir by `id` —
      **unit tests**: front-matter parse, tool-set parse, missing-field defaults,
      repo-overrides-global precedence.
- [ ] 1.2 Ship built-in `executor` / `researcher` / `planner` defaults with their
      bounded tool sets — **unit tests**: each default's tool set is exactly as
      specified; a user file with the same `id` overrides it.

## 2. Skill definitions (thegn-core)

- [ ] 2.1 `skill.rs`: parse + validate a `SKILL.md` (name, description, optional
      params) resolved from `.thegn/skills/<name>/` + global —
      **unit tests**: valid parse, missing name/description rejected, param parse.

## 3. Tool restriction at the AR/ACP seam (thegn-host)

- [ ] 3.1 Intersect the advertised tool list with the active operating agent's
      bounded set at the gateway/ACP tool-filtering seam; refuse any tool call
      outside the set (composing with the policy engine + bouncer as concentric
      gates). Depends on the AR 541–551 / R 695 injection seam.
- [ ] 3.2 Operating-agent picker (which agent is active per worktree) via the
      palette + a statusbar chip — **render test**: chip update is a chrome repaint.

## 4. Skill injection wiring (thegn-host)

- [ ] 4.1 Feed parsed skills into the gateway capability-injection path (AR
      541–543) so they surface as advertised tools/commands, translated per harness
      (AR 570). This change supplies the definitions; the injection mechanism is
      AR's — wire the definitions into it.

## 5. Docs + validate

- [ ] 5.1 Document the agent-definition front-matter, the three built-ins, and the
      skill `SKILL.md` format in the agent doc section + `config.toml.example`.
- [ ] 5.2 Run `just ci` (fmt-check + lint + build + test + coverage ≥95% core +
      smoke + nix-build + `openspec validate --all --strict`).
