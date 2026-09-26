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

## Revision after adversarial review (review-the-505.md)

- **C1** A selected profile whose overlay file is absent is an empty layer
  (`explicit: false` in capture; core `Absent` non-explicit → no layer). An
  existing overlay that is unreadable/invalid stays `ProfileInvalid`. The
  selection is still bound into the base identity, so it changes the revision.
- **C2** The host layer is no longer a refusal. Policy-install failure,
  refused migration, newer schema, unopenable store and invalid stored rows
  all yield `HostLayer::Unavailable`: the generation is published host-less
  with `AdmissionHealth::HostsUnavailable`; the store never authorizes it
  (`StaleConfigReason::HostsUnavailable`), daemon launches refuse via
  `require_launchable`, and a persistent statusbar banner names the reason.
  Doctor, logs, debug, notify, bridge, bridge-revtunnel, `host list` and
  `host rm` are `Recovery` (tolerant display path). `THEGN_ALLOW_SCHEMA_
DOWNGRADE=1` now works: a newer-schema `Db` handle reads hosts through
  `host_db_snapshot::read_allowing_newer` (structure still checked,
  observed schema recorded in the revision).
- **H1** The migration policy is installed exactly once (startup); reload
  never re-installs or re-canonicalizes. A `[database]` change on reload
  publishes and emits one "takes effect after restart" warning.
- **H2** Refusals name source + key: `config_admission::rejection_detail`
  (config file / profile overlay via the `config validate` validators;
  env as `VAR has an invalid <kind> value`; `--set <key>`), carried in
  `CaptureFailure::Admission(error, detail)`. `config validate`/doctor/bundle
  run `config_startup::check_sources` (the same capture + non-DB admission)
  and report what startup would refuse. Previously-clamped display values
  (metrics/preview/clipboard/bars) clamp again, each named as a warning.
  `config set` still re-validates only the file (not addressed).
- **H3** `FrozenEnv::get` treats empty/whitespace values as unset.
- **M1** `loop_update`: a Superseded reload delivers the winning generation.
- **M2** Coalescing is by category + detail fingerprint; `FrameModel::
config_banner` is a persistent statusbar banner while degraded/host-less.
- **M3** Layers are admitted once (`admit_layers` → `with_hosts`); host
  composition is skipped for an empty snapshot; one redundant bounds pass
  removed; the candidate is bounds-checked and serialized once (reused for
  the digest); the full-candidate schema walk runs only when env/`--set`
  contributed. Measured through the lane (release, hyperfine; before = main
  f609a349, after = this branch; isolated XDG, THEGN_NO_MIGRATE=1):

  | case                                          | main          | branch        |
  | --------------------------------------------- | ------------- | ------------- |
  | `thegn --config config.toml.example recent 1` | 23.4 ± 0.5 ms | 38.7 ± 1.6 ms |
  | `thegn recent 1` (first run, no file)         | 19.4 ± 0.9 ms | 22.3 ± 2.2 ms |
  | first frame, example config (pty)             | 252 ± 30 ms   | 298 ± 15 ms   |

  Before the M3 cuts the branch measured 45.4 / 28.6 / 298 ms. The remaining
  ~12 ms user CPU on a 292 KB config is the strict work main never did: the
  one-time schemars schema generation plus the raw-schema walk, and the
  semantic validators. First-frame wall time is within noise of +45 ms
  (user CPU +11 ms). A build-time schema or a lazier walk is the next lever;
  not done here.

- **M4** unchanged (strict unknown keys) — pending the user's decision.
- **M5** `host list`/`host rm` are Recovery, and a bad row no longer
  bricks startup (host layer unavailable, banner).
- **M6 accepted decision:** the state DB is opened (and, for an authorized
  controller, migrated) _after_ every trusted file/env/CLI layer is admitted
  but _before_ the final host composition. A later host-layer problem
  therefore leaves an already-migrated shared DB. This is deliberate: a
  legitimate older schema must be able to reach its upgrade, the upgrade is
  the same one the controller performs on main, and no authority is
  published from a failed host layer (host-less generations never
  authorize). An invalid trusted layer never reaches the DB.

## Re-review round (review notes N1–N8 + the M4 user decision)

- **N1** `Command::Debug` is Configured again: `debug setup` installs a
  toolchain and `debug run/attach` exec-replace into a binary resolved from
  the config, so it is an executing verb, not diagnostic output.
- **N2** The host-less warning and banner now claim only what is refused:
  "stored hosts are missing; agent and tool launches are refused and env
  selections that relied on them will degrade". `require_launchable` still
  has exactly one caller (`config_source::fresh`, three daemon paths);
  extending it to the configured-verb entry stays chunk 6 work.
- **N3** `model.config_banner` is recomputed on every hydration instead of
  carried, so a store change from an in-process daemon launch or the
  wizard's host-add reload cannot leave a stale or missing banner.
- **N4** The post-processed (effective) candidate is validated again after
  `post_process_pure`, so tilde expansion, injected default agents/tools and
  clamps cannot produce a config the runtime uses and nothing validated.
- **N6** `current_dir` failing is no longer a lockout: `settle_cwd` falls
  back to the (absolute) profile root and refuses only when a relative
  `--config` genuinely needs the cwd. The startup reader also retries once
  on `Changed` (write-rename race).
- **N7** `config validate`/doctor pass this process's `--set` overrides
  (`config_source::overrides()`); the refusal detail is labelled "first
  likely cause" because each source is validated standalone; clamp/unknown
  warnings are selected by typed `DiagnosticKind`, not by substring.
- **M4 (user decision) + N8** Unknown keys: refuse under a security-relevant
  table, warn and ignore elsewhere. The policy is a per-call-site flag
  (`config_validate::UnknownKeys`) — only the admitted layers (main file,
  selected profile overlay, the gated typed walk, `--set` shape check) pass
  `RejectSecurityRelevant`; the repo overlay, `host_definition_snapshot`'s
  row decoder and every other schema consumer keep `Reject`, so a newer
  build's host row with an unknown field still fails `InvalidDefinition`.
  The table list lives in `SECURITY_RELEVANT_ROOTS` and is mirrored in
  `docs/help/configuration.md` and `docs/ARCHITECTURE.md`.
- **N5** noted, no action: first frame is at the 300 ms budget on a fast box
  with `THEGN_NO_MIGRATE=1` and no host rows; CLI verbs now ride every
  notify-push hook and bridge spawn. Build-time schema generation is the
  next lever; re-measure with a populated host table.

### Shared-lane incident (twice)

`cargo-lane.sh` kept the lock as fd 9, and any daemon started inside the
lane inherits that open file description: a `podman events` watcher spawned
by a benched TUI held the whole batch lane for ~30 min, and an sccache
server started at 19:12 held it again for ~2 h until killed. The lane script
now runs its command with `9>&-`, and the bench script closes fd 9 and reaps
its own leftovers.

### Round-3 validation (rebased onto main 18060ae8)

`cargo clippy --workspace --all-targets -D warnings`: clean (main's
`sidebar_mouse` unused-`mut` fix landed, so the gate is green on this branch
for the first time). `cargo nextest` over the config/admission/provider/host
surfaces **plus the ratchet tests** (`package(thegn-core) & test(/ratchet/)`)
and `workspace_overlay`: 529/529 passed. `just ratchets` (static) passes.

THE-515's `workspace_overlay::validate` runs inside `typed_semantic_errors`,
so admission already enforces it; `workspace` is in the security-relevant
root list, so an unknown key under `[workspace.<key>]` refuses.

No test asserts on process-global state (`just coverage` runs one process):
the startup tests drive `ProcessAdmission` directly instead of the process
`OnceLock`, and the config-health test no longer mutates the environment —
the environment-layer refusal is covered by the core `rejection_detail`
test, which uses an injected `EnvSource`.

## Round-4 (review F1–F4) and the accepted decisions

- **F1 (blocking) fixed.** `SECURITY_RELEVANT_ROOTS` listed `workspace`, a
  spelling that can never reach a schema walk: `config_compat::
normalize_project_tables` rewrites `[workspace.*]` → `[project.*]` and the
  field is `#[serde(rename = "project", alias = "workspace")]`, so the
  schema property is `project`. Every unknown key under the per-project
  overlay — accounts, hooks, sandbox mounts, both queues, ci, autopilot, the
  MCP scope ceiling, env bundles, git, editor — was only warned about. The
  protected name is now `project` (the legacy spelling is kept for callers
  validating a pre-normalization document), the docs are corrected, and two
  tests cover it: one asserts `is_security_relevant_path("project.x.hooks.
pre")` and admits real `[workspace.<slug>]` documents through
  normalization, the other asserts no listed root is dead in the schema so a
  future rename cannot repeat this silently.
- **F2 fixed, polarity kept.** Added mcp, mcp_proxy, mcp_servers,
  managed_tools, calendar, voice, pins, secrets, credentials, bundle, zone,
  observe, weather, usage, disk — plus drawer, media and stats, which the
  new rot guard found. The list stays a DENYLIST: the user's decision exists
  so a table a newer build adds does not brick an older build sharing one
  config, and an allowlist would refuse exactly that. The guard
  (`security_relevant_roots_cover_every_executing_or_credential_table`)
  derives the expectation from the generated schema — any root whose subtree
  carries a command/argv, a credential, or a URL/host field must be listed —
  so the constant cannot rot silently.
- **F3 fixed.** A typo'd top-level security table (`[sandboxx]`, `[metric]`)
  refuses via the walk's existing ≤2-edit nearest-key hint; a genuinely new
  table, never within that distance, still warns.
- **F4 fixed.** `config_resolve::explain` uses the layer policy, so it no
  longer refuses what admission admits and validate merely warns about.
  `cmd/config.rs`'s `config set` keeps `Reject` (refusing to WRITE a key
  this build does not know is deliberate, and it diffs against the prior
  file's errors so it cannot block an unrelated `config set`).

### Re-bench on this HEAD (required by the review)

Release builds through the lane; before = main `8883b940`, after = this
branch; hyperfine, isolated XDG, `THEGN_NO_MIGRATE=1`:

| case                                          | main            | branch          |
| --------------------------------------------- | --------------- | --------------- |
| `thegn --config config.toml.example recent 1` | 24.2 ± 1.1 ms   | 38.8 ± 1.0 ms   |
| `thegn recent 1` (first run, no file)         | 19.9 ± 0.6 ms   | 22.3 ± 0.6 ms   |
| first frame, example config (pty)             | 221.6 ± 26.1 ms | 257.6 ± 21.4 ms |

N4's second validation pass did not move the numbers (round 2 measured
23.4 → 38.7 / 19.4 → 22.3 on the same harness). **First frame stays under
the 300 ms invariant** (257.6 ± 21.4 ms), so no exception is needed — but the
caveats stand: a fast box, `THEGN_NO_MIGRATE=1`, and a state dir with no
host rows, so the real path costs somewhat more. Build-time schema
generation remains the next lever, and a re-measure with a populated host
table is still owed.

### Accepted decisions (no code)

- **`doctor` is Recovery and still execs config-derived binaries**
  (`cmd/doctor.rs:2319`, `:1101`) from an _unadmitted_ configuration. This
  is deliberate: doctor's job is to run when the configuration is what is
  broken, and gating it on admission would remove the tool the operator
  needs. The exposure is bounded — doctor probes binaries the same config
  would name anyway — and it already computes `check_sources`, so a later
  refinement is to skip those probes with "not probed: configuration
  refused" rather than to re-gate the verb.
- **`config_validate::validate_normalized` runs its semantics on the
  PRE-post-process candidate**, while admission now validates both the raw
  and the post-processed candidate (N4). `config_startup::check_sources` is
  therefore load-bearing: it is what keeps `config validate`/doctor in
  agreement with startup, since the file validators alone cannot see the
  effective candidate (or the environment layer).

## Approval fold-ins

1. The rot guard was assert-only-empty, so a schemars shape change that made
   `collect` stop matching would have left it passing and decorative. It now
   also asserts the flagged set CONTAINS known members (sandbox, project,
   agents, env, host, metrics) and has a plausible size (39 roots flagged
   when written; the test fails below 30).
2. `is_dangerous_field` used a command/argv-only vocabulary and so detected
   only 39 of the 65 listed roots. It now also names what this schema
   actually uses for the same things: `run` (`actions[].run`), `args` /
   `*_args` (`lsp.servers[].args`, `extra_args`), `*_bin`
   (`host_discovery.tailnet.tailscale_bin`), bare `path`
   (`managed_tools.<k>.path`, `calendar.accounts[].path`), `*_image`
   (`env.<k>.k8s.image`, `sidecar_image`), `socket` (`daemon.socket`),
   `proxy` (`mcp_servers.<k>.proxy`) and `*_dir` (`disk.sccache_dir`,
   `shared_target_dir`, `clipboard.remote_dir`). The doc comment now says
   the guard is a LOWER BOUND: roots whose danger is structural rather than
   lexical (`[database]` migration authority, `[placement]` lanes, `[disk]`
   reclamation) stay listed on human judgment.
3. The nearest-key hint comment claimed a new table is "never within that
   edit distance". That is false for this schema — `profile` and `profiles`
   differ by one edit and one is security-relevant — so the comment and
   `docs/help/configuration.md` now state the trade-off plainly: a new
   top-level table within ~two edits of a security-relevant name
   (`[hosts]`, `[bundles]`, `[zones]`, `[secret]`, `[agent]`, `[stat]`, …)
   is refused by a build that does not know it, and the remedy is to rename
   or remove the key. The behaviour is unchanged and the lever to drop it is
   isolated: delete the top-level (`None`) branch of `unknown_key_refuses`
   and keep the nested rule.

### Bench caveats (recorded at the reviewer's request)

The numbers above are a floor, not the real path: they run with
`THEGN_NO_MIGRATE=1` against an isolated EMPTY state dir, so they exclude
both the brand migration and the host-row read a real start pays, and the
box was fast and idle while the target machine runs agent fleets. On those
terms the branch takes ~16 % of the first-frame budget it did not take
before (221.6 → 257.6 ms against a 300 ms invariant), so the schemars
build-time cut is the next lever, not an optional one. The CLI delta
(+14.6 ms with the 292 KB example config) rides every notify-push hook and
every bridge spawn; the user's real config is ~46 KB, about 6× smaller, so
their hooks pay proportionally less than this table suggests.

### Follow-up filed, not fixed

`config_admission::rejection_detail` validates each source standalone and
returns the FIRST error it finds, so it can name an unknown non-security key
as the "first likely cause" of a refusal actually caused by something else.
Switching it to the layer policy (skipping keys the layer policy only warns
about) would tighten the hint; the refusal itself is unaffected.
