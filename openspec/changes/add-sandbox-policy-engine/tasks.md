# Tasks

## 1. Core config (superzej-core)

- [ ] 1.1 `PolicyConfig` (`default_read`/`default_write`/`default_shell` +
      `rules: Vec<PolicyRule>`, all `serde(default)`); `PolicyAction`
      (`Allow|Deny|Ask`) via `config_enum!`; layer via `effective_policy(repo_root)`
      (global → profile → workspace → repo) — **unit tests**: defaults parse, empty
      policy = permissive-under-worktree default, precedence + every-field apply.

## 2. Core decision (superzej-core)

- [ ] 2.1 `policy.rs`: `Operation`, `PolicyDecision`, `PolicyCtx`, `decide()` with
      ordered first-match, `under:`/`outside:` worktree-relative globs, command
      regex, `stop` — **unit tests**: each selector, allow/deny/ask actions,
      default fall-through, `stop` short-circuit.
- [ ] 2.2 Built-in hard denies (`.git/config`, `~/.ssh`, `~/.gnupg`, secret paths)
      evaluated first and non-overridable — **unit tests**: user `allow` cannot
      override a hard deny.

## 3. Enforcement seam (superzej-host / bouncer)

- [ ] 3.1 Call `policy::decide` before the bouncer services each brokered
      `fs`/`edit`/`write`/`terminal` op: `Allow` services silently, `Deny` refuses
      with reason (ACP `reject_once`), `Ask` raises the existing permission overlay.
- [ ] 3.2 Persist `allow_always`/`reject_always` overlay choices as appended policy
      rules so the next identical op is decided without a prompt.

## 4. Docs + validate

- [ ] 4.1 Document `[policy]` (defaults, rule selectors/actions, `under:`/`outside:`,
      hard denies) in `config/config.toml.example`.
- [ ] 4.2 Run `just ci` (fmt-check + lint + build + test + coverage ≥95% core +
      smoke + nix-build + `openspec validate --all --strict`).
