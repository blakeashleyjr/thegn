# Design

## Trust ladder and where semantics engage

`TrustLevel` (most → least trusted): `Builtin → UserGlobal → Profile → Zone →
Workspace → Repo → Runtime`. Constraint semantics engage only _below_ Profile, so
the global/profile/env/`--set` layers behave byte-for-byte as before. `Runtime`
(env + `--set`) ranks at UserGlobal trust but max specificity — it keeps today's
last-writer-wins. In practice only the Repo layer (and the new Zone layer) change
behavior.

## Merge classes

- **override** — preferences; most-specific-wins (unchanged).
- **ceiling** — a more-trusted level sets the max reachable/spendable set; lower
  levels intersect only. Deny wins over allow.
- **floor** — a more-trusted level sets a minimum that can't be weakened.
- **accumulate** — union; additive entries from less-trusted levels are gated.

Three-valued list semantics at Zone/Repo: `None` = inherit, `Some([])` =
deny-all, `Some([..])` = narrow. The global `Vec` (empty = no filter) is
untouched, so there is no versioned-semantics machinery and no global compat
break. Deny-all egress is encoded as `network_block += ["*"]` (a new universal
DNS pattern) — no `SandboxConfig` shape change.

## Repo field classification (the centerpiece)

Every `SandboxOverlay` field is destructured exhaustively (no `..`) so a new
field fails to compile until classified:

- **Forbidden**: `backend`, `default_backend`, `backend_chain`, `default_env`,
  `compose`, `env_passthrough`, `remote`, `vpn`, `home`.
- **Floor** (tighten only): `enabled` (true-only), `profile`, `agent_profile`,
  `network`, `file_access`, `warm_direnv`, `on_missing`, `auto_caches` (false
  stricter), `network_audit` (true-only).
- **CeilingIntersect**: `network_allow`, `limits`.
- **Accumulate (ungated)**: `network_block`.
- **Gated (trust-on-first-use)**: `mounts`, `volumes`, `init_script`, `prepare`,
  `image`, `ports`, `gpu`, `nix_daemon`.
- **Allow (preference)**: `devenv`, `inject_devshell`, `shell`.

A weakening request is **denied, not gated** — a repo never sets a constraint.
The remedy for a repo you own is named in the message: put it in
`[workspace.<slug>]` or a global `[env.<name>]` the repo selects.

## Trust-on-first-use

Approvals are per-request, matched by the request's **canonical JSON** (whitespace-
and key-order-independent) via string equality — never a hash (`util::short_hash`
is FNV, display-only). A later edit that adds or changes a request produces a
different canonical key and re-prompts; existing approvals persist. Pre-approval,
a gated request is simply not applied and the worktree still opens.

## Explain

Provenance is a cold-path layer replay: snapshot `serde_json::to_value(&cfg)`
after each layer, diff at the dotted key's JSON pointer, and report the last
layer that changed the value. Uniform across typed overlays, profile deep-merge,
env, and `--set`; zero hot-path cost. The `sandbox.*` clamp trace is merged in
from `repo_sandbox_resolved` with the persisted approvals.
