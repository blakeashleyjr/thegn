# Tasks

## 1. Data model (thegn-core)

- [x] 1.1 `zones` table + `workspaces.zone_id` (schema v33, additive) +
      `ZoneStore` (`db_zones.rs`): create/rename/list, `delete_zone(force)`
      (refuses non-empty unless forced), assign, `zone_of_worktree` — **unit +
      migration tests**.
- [x] 1.2 `zone.rs`: `ZoneConfig`/`ZoneBudget`, `Config.zone` map, `Bundle.zone`
      field; `config_issues` — (config-issue warnings deferred).

## 2. Ceilings + resolution (thegn-core)

- [x] 2.1 `apply_zone_ceilings` (egress intersect-down with drop reporting, block
      union, sandbox-profile floor) + `bundle_visible` — **unit tests**.
- [x] 2.2 Launch path applies the worktree's zone ceilings to the resolved
      sandbox and surfaces dropped egress (`handlers/repo_trust`).

## 3. Bundle sub-vault (thegn-core)

- [x] 3.1 Compose fold denies a zone-owned bundle to a foreign/unzoned worktree
      (direct, global, `extends`); `ResolvedEnv.denied` + surfacing — **tests**
      cover each binding path.

## 4. Budget rollup (thegn-proxy)

- [x] 4.1 `Identity.zone` resolved per request from the shared DB; `check_budget`
      and `record_spend` iterate scope → zone → global — **tests** (zone cap
      refuses a member under its own cap; triple attribution).
- [x] 4.2 `zone::sync_budget_caps` pushes `[zone.<name>.budget]` into proxy
      budget rows (spend preserved) — **test**.

## 5. UX

- [x] 5.1 CLI `thegn zone list|create|rm [--force]|assign <zone> [repo]`.
- [ ] 5.2 Palette actions + sidebar zone chip + detail zone status (deferred; CLI
      covers management).

## 6. Docs + validate

- [x] 6.1 `config.toml.example`: `[zone.<name>]` + `[bundle.<n>] zone`.
- [ ] 6.2 `openspec validate --all --strict` green.
