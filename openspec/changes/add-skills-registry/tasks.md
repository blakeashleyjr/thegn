# Tasks

## 1. Registry model + store (superzej-core)

- [ ] 1.1 `SkillPackage` (name, semver `version`, body, metadata: `triggers`,
      `requires`, `harnesses`, `tools`) + `[skills]` config (sources, policy,
      per-scope enablement) in `config.rs` — **unit tests** for parse + semver
      ordering + invalid-package rejection.
- [ ] 1.2 `skill_packages` table + migration (`user_version` bump) with content
      hash, version pin, per-scope enablement in `ui_state` — **unit tests** for
      register / install-on-demand / pin-resolve round-trip and offline cache hit.

## 2. Selection + render seam (superzej-core)

- [ ] 2.1 `skills.rs`: `select_and_render(task_context, harness, policy)` —
      deterministic trigger match + scope filter + **stable (name, version)
      ordering** — **unit tests**: same context+policy ⇒ identical ordered block
      set; disabled scope excludes; passthrough returns empty.
- [ ] 2.2 Per-harness rendering reusing AR 570 translation (in-process termite
      vs ACP/MCP-over-ACP block shapes) — **unit tests** per harness shape.

## 3. Cache-aware injection (proxy path)

- [ ] 3.1 Inject rendered blocks behind a stable prefix with cache breakpoints
      placed **after** the injected blocks — **unit tests**: prefix bytes before
      the breakpoint are byte-identical across two requests that differ only in a
      newly selected skill (caching preserved).

## 4. Policy + AI-free no-op

- [ ] 4.1 Per-harness transparent-passthrough vs managed policy gate; registry
      absent/disabled and no proxy configured ⇒ inert no-op (not error) —
      **unit tests** for passthrough no-op and AI-off inertness.

## 5. Validate

- [ ] 5.1 Run `just ci`
