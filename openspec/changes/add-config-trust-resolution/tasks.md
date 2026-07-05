# Tasks

## 1. Pure engine (superzej-core)

- [x] 1.1 `config_resolve.rs`: `TrustLevel`, `MergeClass`, `RepoFieldRule`,
      restrictiveness lattices (Network/SandboxProfile/OnMissing/WarmDirenv +
      the FileAccess partial order), three-valued list intersection,
      `ResolutionTrace`/`ClampEvent`/`GatedRequest`/`Approvals` — **unit tests**.
- [x] 1.2 `classify_repo_overlay`: exhaustive `SandboxOverlay` destructure (no
      `..`) mapping every field to a rule; the `hostile_repo_cannot_escape` suite
      is the security regression gate.
- [x] 1.3 `dns_filter`: `*` universal block pattern (deny-all egress) + test.

## 2. Wire the clamp (the security fix)

- [x] 2.1 `Config::repo_sandbox` / `resolve_env` delegate to
      `config_resolve::resolve_repo_sandbox` / `resolve_environment`
      (fail-closed deny-all); `repo_sandbox_resolved` / `resolve_env_with` expose
      denials + pending. Legacy overlay tests rewritten to assert clamped output.
- [x] 2.2 Host: surface denials + pending on the launch path
      (`handlers/repo_trust::resolve_env_trusted`) via tracing + one deduped
      notification; never halt.

## 3. Trust-on-first-use

- [x] 3.1 `repo_trust.rs`: canonical-JSON identity (equality match, not hash);
      `request_id` display handle — **unit tests**.
- [x] 3.2 `repo_trust` table (v32) via additive schema + `RepoTrustStore`
      (`db_trust.rs`): decide/revoke/list/approved — **unit + migration tests**.
- [x] 3.3 Launch path loads approvals from the DB and applies approved gated
      requests; CLI `superzej repo-trust [path] [--approve <id>] [--revoke <id>]`.

## 4. Explain

- [x] 4.1 `config_resolve::explain`: cold-path layer replay (defaults → file →
      profile → env → `--set`), diff at the dotted key, report origin — **tests**.
- [x] 4.2 CLI `superzej config explain <key> [--repo <path>] [--json]` renders the
      value, origin trace, and (for `sandbox.*` with `--repo`) the clamp trace.

## 5. Docs + validate

- [x] 5.1 `config.toml.example`: repo-overlay-trust note.
- [ ] 5.2 `openspec validate --all --strict` green.
