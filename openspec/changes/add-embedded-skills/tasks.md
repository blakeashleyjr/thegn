# Tasks — embedded skills

## 1. Core model (thegn-core)

- [ ] 1.1 `skills.rs`: embedded-skill model (name, description, body,
      version), frontmatter parsing/validation, path-safe name check.
- [ ] 1.2 Managed-marker + content-hash scheme; pure sync planner:
      (embedded set, on-disk survey) → write/replace/delete/skip-adopted
      plan honoring `exclude`.
- [ ] 1.3 Unit tests to the 95% gate: idempotence, upgrade replace,
      deprecation delete, unmarked-skip, hash-mismatch-adopt, exclude
      withhold/remove.

## 2. Curated content

- [ ] 2.1 Author the v1 set: worktrees (`wt` lifecycle), merge queue
      (graduate `extensions/skills/mq`), PR queue, self-docs
      (`thegn mcp serve` pointer). Review rule: no `--force`/`--yes` in
      recipe command lines without an explicit confirm-with-user step.
- [ ] 2.2 Embed via `include_str!`/`include_dir`; manifest test asserting
      frontmatter validity for every embedded skill.
- [ ] 2.3 Drift gate: extract fenced `thegn …` lines from bodies and check
      them against the live clap tree (placeholders skipped); include the
      force-flag lint from 2.1.

## 3. Sync + adapters (thegn-host)

- [ ] 3.1 Per-vendor target-dir adapters (claude, generic
      `~/.agents/skills/`; others `reserved`), defaults from `[[agents]]`;
      vendor paths only inside adapter impl files.
- [ ] 3.2 `thegn skills list|show|sync [--agent <kind>] [--remove]` executing
      the pure plan; `--json` on list.
- [ ] 3.3 Smoke coverage in `test/smoke.sh` (sync into a temp HOME, assert
      markers, re-sync no-op, `--remove`).

## 4. Config + startup

- [ ] 4.1 `[skills]` table (`enabled`, `auto_sync` default off, `exclude`);
      exhaustive destructure in validation; document in
      `config/config.toml.example`.
- [ ] 4.2 `auto_sync` hook: `spawn_blocking` after first frame; best-effort
      failure surfacing; no event-loop or render-plan involvement.

## 5. Doctor + docs

- [ ] 5.1 Doctor rows per agent kind: dir found, managed set
      current/stale/user-modified/absent.
- [ ] 5.2 `docs/cli.md` + `docs/help/` update for the `skills` namespace and
      `[skills]` config table (config-reference page is generated from doc
      comments).

## 6. Gate

- [ ] 6.1 Run `just ci` once at the end (includes openspec validate).
