# THE-505 handoff — Claude batch worker, 2026-09-18

verdict: implementation-ready (chunks 4-fixes, 5, most of 6, part of 7).
Not issue-complete: see "Open" below.

## What changed

1. Provider in-flight review (primary-provider-inflight-review.md, moved
   here) — all seven items addressed in `aee70ba1`-equivalent commit
   `fix(provider): enforce structural provider grammars…`:
   compile fix; real grammars (RFC 1123 names, lowercase slugs, OCI image
   refs, VPS slug / `snapshot:<positive>`, `image:` refused for VPS);
   endpoint parser with Ipv4Addr/Ipv6Addr, bounded nonzero port, RFC 3986
   path, userinfo/query/fragment/percent-host/backslash refused for API and
   GraphQL; structural SSH public-key parsing (canonical base64, wire type,
   per-algorithm sizes, no trailing bytes) with real throwaway fixture keys;
   Fly iroh injection validated + bound to sandbox name + redacted, machine
   body rendered before ledger/app/IPv4; reqwest errors replaced by
   value-free categories (Display, `{:#}`, `{:?}` tested with a URL
   canary); response bodies no longer dumped; fake-transport tables drive
   every invalid field category through `RemoteProvider::create` with zero
   requests and zero ledger rows (checked under the mutated ledger key and
   whole-ledger emptiness).

2. `thegn_core::config_admission_store::AdmissionStore` — one mutex over
   current snapshot + generation + failure epoch; CAS publication; typed
   exhaustion; failed reload = display-only last-good, `authorize` refuses
   everything until a publication; identical failures coalesce.
   `ConfigRevision` remains the single generation token (generation +
   admitted digest); unpublished candidates carry generation 0. This is the
   boundary THE-515/THE-516 should consume: hold the `ConfigRevision` you
   observed and call `store.authorize(&rev)` immediately before each
   authority side effect. It is a check, not a lock.

3. Startup/CLI/reload wiring (`crates/thegn-host/src/config_startup.rs`):
   capture once after profile reroot → strict non-DB admission (no DB
   touched on failure) → admitted `[database]` migration policy → `Db::open`
   (legitimate older schema upgrades here — this is the resolution of
   config-upgrade-coordination.md for THE-516: host schema is read AFTER
   migration, strictly, at `SCHEMA_VERSION`) → strict `host_defs_checked` →
   final admission → publish generation 1 → install runtime effects.
   Interactive launch and `open` admit before the runtime/alt screen;
   configured CLI verbs run on the admitted config; source-inspection and
   recovery verbs keep the tolerant _display_ projection. Hot reload
   re-admits off-loop from the frozen capture and publishes by CAS;
   hydration reads the published snapshot; daemon agent/tool/fork launches
   refuse on a failed reload; wizard host-add republishes.

4. Readers: macOS/Windows resolve a final config symlink with the OS
   resolver and re-check after the no-follow read (Nix/home-manager
   configs keep working — deliberate deviation from the "retain final-link
   refusal" instruction, because startup is now gated on it). Linux falls
   back to the shared Unix no-follow reader ONLY for filesystems outside the
   O_PATH reader's allowlist (ZFS/NFS/overlay/FUSE homes would otherwise be
   unable to start). The old "/dev/shm world-writable ancestor" test was
   false on this box (`/dev` is tmpfs here; the reader has no ancestor
   permission check, only an fs-type allowlist) and was replaced by a
   deterministic procfs test.

5. `config_resolve::explain` returns `Result`; a hidden read/normalize/
   schema/profile failure is an error, never a Builtin origin.

6. Cleanup/teardown (VPN deregistration, provider sync-back, close
   checkpoint, landed-worktree removal, worktree destroy) use
   `config_startup::cleanup_config()` = the last admitted generation (even
   while degraded), never a default. The daemon MCP-proxy reload re-admits;
   a failed reload keeps the baseline so nothing is reconciled.

## Reviewer focus

- `main.rs` intent split: `CommandIntent::Configured` → strict; everything
  else legacy display. Doctor is Configured, so doctor refuses on a broken
  config (use `config validate`).
- Newer-schema DB now refuses startup with a typed message instead of
  launching chrome with a banner (strict hosts cannot be admitted).
- `THEGN_ALLOW_OLD_BUILD` override + newer DB: strict host read refuses.
- Linux fallback reader and macOS/Windows link resolution (unverified on
  those OSes).
- Every stdio bridge / `notify push` now requires an admissible config.

## Open (not done)

- Chunk 6 remainder: repo-trust/switch-cache actions use the display snapshot without
  `authorize`; provider constructors are not yet given a `ConfigRevision`
  to `authorize` before their first remote call (THE-515/516 boundary).
- Chunk 7: `config get/show`, doctor/bundle/config_health, completion and
  MCP docs still reparse; explain is not yet a projection of the admitted
  trace (only made fail-closed).
- Chunk 8: `complete.rs` (display-only) and the legacy fallback inside
  `cleanup_config`/`config_source`/hydration for admission-less test
  processes still call the legacy loader; `Config::load_layered` is not
  removed and there is no fallback-removal ratchet yet. Cleanup paths use
  the whole last-admitted config, not a narrowed held provider handle.
- Chunk 9: fuzz/property and process-level (actual binary) startup tests.
