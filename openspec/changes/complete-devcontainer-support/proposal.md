# Complete devcontainer support (spec the surface, close the honest gaps)

Linear: THE-23

## Why

devcontainer.json support already landed (tasks.md **AB 358**): a pure JSONC
parser (`thegn-core/src/devcontainer.rs`), a trust-gated overlay folding image /
build / compose / mounts / ports / env / lifecycle onto `[sandbox]`
(`devcontainer_overlay.rs`), a native OCI features resolver
(`devcontainer_features.rs`), host wiring for local worktrees
(`handlers/repo_trust.rs::resolve_env_trusted`) and remote provisioning
(`host_provision.rs`), and a live-podman/docker e2e. But:

- **None of it is specced.** No `openspec/specs/` capability mentions
  devcontainers at all, so the behaviour — including its trust gate, the one
  security-critical part — has no contract and no delta discipline.
- **Part of the spec surface is parsed and silently ignored** (`runArgs`,
  `overrideCommand`, `remoteUser`/`containerUser` outside feature installs,
  `workspaceMount`) and part is silently dropped at parse
  (`hostRequirements`, `waitFor`, `userEnvProbe`, `portsAttributes`, `init`,
  `privileged`, `capAdd`, `securityOpt`, `shutdownAction`, `secrets`,
  multi-config `.devcontainer/<folder>/` layouts, the `devcontainer.metadata`
  image label, `${devcontainerId}`). Silence is the failure mode: a repo author
  cannot tell which half of their file thegn honours.
- **There is no opt-out and no visibility**: no `[sandbox] devcontainer` knob,
  no `thegn doctor` line, and a non-OCI backend (bwrap/systemd/host) drops the
  whole container shape without a word.

"Full devcontainer support" here means: every field in the containers.dev JSON
reference is either **applied**, **refused with a visible reason** (isolation-
weakening keys), or **reserved with a warning** — never silently eaten — with
the whole surface under spec.

## What Changes

- **New `devcontainer` capability spec** capturing today's behaviour as
  requirements: detection + JSONC parse + polymorphic normalization,
  trust-gated category overlay (image/build/compose/mounts/ports/lifecycle/
  features; env applies ungated because it is container-scoped — see the
  `${localEnv}` clamp below), lifecycle mapping
  (initialize→host `prepare`, onCreate/updateContent/postCreate→ordered
  one-time steps, postStart/postAttach→`init_script`), features install
  (order, options→env, in-container oras|curl fetch), and variable
  substitution.
- **Field-surface honesty**: parse-time inventory of every recognized-but-
  unapplied key; isolation-weakening keys (`privileged`, `capAdd`,
  `securityOpt`, `runArgs`, `init`) are _refused by design_ with a warning —
  never applied, not even behind trust approval; editor-only keys
  (`customizations`) stay silently dropped per the spec's intent; the rest
  (`hostRequirements`, `portsAttributes`/`otherPortsAttributes`, `waitFor`,
  `userEnvProbe`, `shutdownAction`, `updateRemoteUserUID`, `secrets`,
  `devcontainer.metadata` image label) are **reserved** — surfaced as a
  one-line warning naming the key.
- **Multi-config discovery**: detect `.devcontainer/<folder>/devcontainer.json`
  variants; a repo-scoped selector (`.thegn.toml` `devcontainer = "<folder>"`,
  clamped like every repo key) picks one; ambiguity without a selector warns
  and uses none.
- **`${devcontainerId}`** substitution: a stable id derived from repo root +
  config path hash.
- **`${localEnv:VAR}` is clamped to the passthrough allowlist.** Today the
  overlay resolves `${localEnv:VAR}` via an unrestricted `std::env::var`
  (`handlers/repo_trust.rs`), so a repo-committed devcontainer.json can copy
  **any** host env var of the thegn process (e.g. `GH_TOKEN`) into
  `containerEnv`, where repo code runs and can exfiltrate it — quietly
  bypassing the clear-then-allowlist pane-env model. The fix: `${localEnv:VAR}`
  resolves only vars on the effective `[sandbox] env_passthrough` allowlist;
  any other var resolves to empty with a warning naming it. This is why the
  literal env categories can stay ungated.
- **Backend interplay made visible**: `[sandbox] devcontainer = "auto" | "off"`
  (new config key, documented in `config/config.toml.example`); when a trusted
  devcontainer declares a container source and the effective backend family is
  not OCI, thegn surfaces a warning naming the backend instead of silently
  ignoring the file.
- **Doctor probe**: `thegn doctor` gains a devcontainer section — presence,
  parse result, approved/pending trust categories, refused/reserved keys
  found, and whether the effective backend can honour the container source.
- **Feature ordering fidelity (best-effort)**: honour `installsAfter`/
  `dependsOn` from fetched feature metadata when available, keeping
  `overrideFeatureInstallOrder` + declaration order as the fallback; build-time
  feature layering (the reference implementation's generated-Dockerfile path)
  is explicitly out of scope and reserved.

## Impact

- **tasks.md**: AB 358 (devcontainer.json support — closes its recorded
  limitations), touches AB 362 (default-on sandbox) and O (configuration) for
  the new key.
- **Specs**: new `devcontainer` capability (this delta). `sandbox` is
  deliberately untouched — the overlay folds onto the existing sandbox model
  without changing it.
- **In-flight changes**: builds on `add-config-trust-resolution` (the
  `GatedRequest`/`Approvals` machinery and `repo_trust` table the overlay
  already uses — this change only adds request categories, no new trust
  model); orthogonal to `add-oci-runtime-tiers` (`oci_runtime` composes with a
  devcontainer image unchanged); the doctor probe follows the
  `mark-unverified-backends` honesty pattern; mount folding stays under
  `verify-sandbox-mounts`' verification umbrella; refused isolation keys align
  with `add-sandbox-policy-engine` (a repo request can never widen the trusted
  bound).
- **Capability catalog**: no new externally invokable operation — the doctor
  extension rides the existing `doctor` verb; trust approval rides the existing
  `repo-trust` verb. No catalog row added.
- **Code (host/core)**: `thegn-core` `devcontainer.rs` (inventory + multi-config
  - `${devcontainerId}`), `devcontainer_overlay.rs` (refusal warnings, backend
    check), `devcontainer_features.rs` (metadata ordering), `config_sandbox.rs`
    (`devcontainer` mode key); `thegn-host` `cmd/doctor.rs` (probe),
    `handlers/repo_trust.rs` (selector plumb-through). No DB schema change (the
    existing `repo_trust` table already stores the categories).

## Non-goals

- **Build-time feature layering** (generating a Dockerfile that bakes features
  into the image, as the reference CLI does) — thegn's in-container
  `install.sh` execution covers the dominant tool-installer case at a fraction
  of the cost; reserved, not promised.
- **`devcontainer.metadata` image-label config merge** — needs an image
  inspect at overlay time; reserved with a warning when the label is present.
- **Port-attribute UX** (`onAutoForward`, browser opening) — the `[forward]`
  panel owns port UX; `portsAttributes` stays reserved.
- **Honouring isolation-weakening keys** under any consent flow — refusal is
  the design, not a gap.
