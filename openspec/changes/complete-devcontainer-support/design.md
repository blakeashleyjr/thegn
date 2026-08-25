# Design — complete devcontainer support

## Context

The implementation largely exists (tasks.md AB 358): `thegn-core`
`devcontainer.rs` (pure JSONC parser + normalization + substitution),
`devcontainer_overlay.rs` (trust-gated category fold onto `SandboxConfig`,
lifecycle→hook mapping), `devcontainer_features.rs` (native OCI features:
oras|curl fetch + `install.sh`), host wiring in
`handlers/repo_trust.rs::resolve_env_trusted` and `host_provision.rs`, and a
live-podman/docker e2e. This change is mostly **spec debt + honesty gaps**, not
a new subsystem: put the whole surface under contract, and make every
unapplied field visible.

## The field classification (centerpiece)

Every field in the containers.dev reference lands in exactly one class. The
classes and their rationale:

| Class                 | Fields                                                                                                                                                                                                                                                                         | Behaviour                                              |
| --------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------ |
| **Applied**           | `image`, `build.*`, `dockerComposeFile`/`service`/`runServices`, `features` (+`overrideFeatureInstallOrder`), `mounts`, `forwardPorts`, `containerEnv`, `remoteEnv`, `workspaceFolder`, all lifecycle commands, substitution variables                                         | per the trust gate + precedence rules                  |
| **Refused by design** | `privileged`, `capAdd`, `securityOpt`, `runArgs`, `init`                                                                                                                                                                                                                       | never applied, even trust-approved; warned with reason |
| **Reserved**          | `hostRequirements`, `portsAttributes`/`otherPortsAttributes`, `waitFor`, `userEnvProbe`, `shutdownAction`, `updateRemoteUserUID`, `secrets`, `workspaceMount`, `overrideCommand`, `remoteUser`/`containerUser` (beyond feature-install seeding), `devcontainer.metadata` label | one-line warning naming the key                        |
| **Editor-only**       | `customizations`                                                                                                                                                                                                                                                               | silently dropped                                       |

Refusal is not a gap to be closed later: those keys _weaken isolation below
the trusted base_, and `add-sandbox-policy-engine`'s invariant is that a repo
request can never widen the trusted bound. Putting them behind a consent
prompt would make a clone a consent dialog for sandbox escape — the exact
anti-pattern `add-config-trust-resolution` exists to prevent. `runArgs` is in
the refused class (not reserved) because it is an arbitrary-flag escape hatch:
`--privileged`, `--cap-add`, device mounts all fit through it.

The inventory is computed at parse time (a `recognized_unapplied()` companion
to the parser, exhaustively tested against the classes above) so the overlay
and doctor share one source of truth. A newly-added spec field that appears in
no class fails a test — the same "exhaustive destructure" discipline
`config_resolve` uses.

## The `${localEnv}` clamp (security fix, not just spec)

Today `handlers/repo_trust.rs` builds the substitution context with
`local_env: &|k| std::env::var(k).ok()` — unrestricted. thegn's own process
env is the user's full shell env (panes are clear-then-allowlist precisely to
keep it out of children), so a repo-committed
`containerEnv = { X = "${localEnv:GH_TOKEN}" }` copies the real token into a
container where repo code runs. That defeats the pane-env model through a side
door, and it is why the env categories being ungated was only _almost_ true.

Fix: the `local_env` closure consults the effective
`[sandbox] env_passthrough` allowlist; a miss substitutes empty and pushes a
warning naming the variable. The allowlist is the user's existing declaration
of "vars I forward into sandboxes", so no new consent surface is needed.

## Multi-config and `${devcontainerId}`

Discovery adds the `.devcontainer/<folder>/devcontainer.json` layout to
`detect_and_parse`. Selection is a repo-scoped `.thegn.toml`
`devcontainer = "<folder>"` key — a _preference_ in the
`config_resolve` classification (it picks among repo-authored files; it grants
nothing), so it needs no gate. Ambiguity without a selector applies nothing:
guessing which of two container definitions to trust-prompt for is worse than
asking the user to pick.

`${devcontainerId}` = `util::short_hash(repo_root + "\0" + config_relpath)`
(display-stable, not security-relevant — the spec uses it for volume naming),
so it is stable across sessions and distinct per variant.

## Backend interplay and the opt-out

`[sandbox] devcontainer = "auto" | "off"` is a `config_enum!` on
`SandboxConfig`. `off` short-circuits before parse — no trust prompts, no
warnings, one notice line. In `auto`, when the effective backend family is not
OCI (bwrap/systemd/host), the container-shape categories are skipped with a
warning naming the backend; env and lifecycle pieces that are meaningful
without a container (initialize→prepare) still apply under their gates. This
follows the `mark-unverified-backends` honesty pattern: degrade loudly, never
silently.

## Doctor

`thegn doctor` gains a devcontainer block per repo context: file presence +
selected variant, parse result, category trust states (from the existing
`repo_trust` table), refused/reserved keys found, backend honour-ability. Pure
read; no new capability-catalog row (rides the existing `doctor` verb).

## Event loop / damage / persistence

- Parse + overlay already run on the off-loop spawn paths
  (`resolve_env_trusted` on the spawn_blocking pane-materialize worker;
  `host_provision` on its pipeline thread). No new wake path, no ticker; new
  warnings ride the existing notification/status channels (chrome dirty ⇒
  `Full` frame on the next wake, as today).
- **No SQLite schema change**: gated categories are rows in the existing
  `repo_trust` table; the inventory and warnings are transient.
- No new interactive surface: no action/keybind/zone; the config key is
  documented in `config/config.toml.example` (the generated config-reference
  help page picks it up).

## Alternatives considered

- **Reference-CLI build-time feature layering** (generate a Dockerfile that
  bakes features into the image): rejected for now — thegn's in-container
  `install.sh` execution covers the dominant tool-installer case without a
  Dockerfile-generation subsystem; reserved, surfaced when a feature needs it.
- **Consent flow for `privileged`/`capAdd`/…**: rejected; refusal is the
  design (see above).
- **Honouring `devcontainer.metadata` image labels**: needs an image inspect
  at overlay time (network/daemon on the resolve path); reserved with a
  warning when the label is present.
- **Silent drop for reserved keys** (status quo): rejected — a repo author
  cannot tell which half of their file thegn honours; the one-line warning is
  the minimum honest surface.

## Security

- **Trust boundary**: devcontainer.json is repo-committed and therefore
  attacker-authored until approved. Every category that can execute code or
  touch the host (image/build/compose/mounts/ports/lifecycle/features) is a
  `GatedRequest` through the `add-config-trust-resolution` TOFU flow,
  canonical-form matched so edits re-prompt. Unapproved ⇒ pending, not
  applied, worktree opens.
- **Isolation-weakening keys are refused absolutely** — no consent path.
- **`${localEnv}` clamp** closes the host-env exfiltration side door (above).
- **Blast radius of the new surface**: the config key and doctor block are
  read-only; the selector key only chooses among repo files already subject to
  the gates. No new write surface, no credentials handled (the `secrets` field
  stays reserved precisely because it would be one).

## Open questions

- Should the non-OCI warning offer a one-key "switch this worktree's backend
  to podman" action? (UX sugar; not needed for the contract.)
- Whether `updateContentCommand` should re-run on content refresh for remote
  worktrees (the spec's prebuild semantics) — reserved with the key until a
  prebuild story exists.
