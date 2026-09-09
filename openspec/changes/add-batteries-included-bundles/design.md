# Design — batteries-included platform distributions

## Current support matrix

| Platform path      | Current state    | Bundle contract                                                                                              |
| ------------------ | ---------------- | ------------------------------------------------------------------------------------------------------------ |
| Nix Linux/macOS    | Shipped baseline | Thegn + pinned Alacritty + Fira Code Nerd Font + generated Fontconfig and writable Alacritty config          |
| Standalone Linux   | Partial          | Binary installer diagnoses missing Alacritty; explicit deterministic batteries install/rehearsal remains     |
| macOS app/launcher | Partial          | App builder accepts Alacritty config; complete terminal/font/binary artifact and clean-host rehearsal remain |
| Windows            | Undecided        | Must be implemented and rehearsed or explicitly deferred with a bounded issue and fallback                   |

The published matrix must name what is bundled, what is generated user-locally,
and what is delegated to the host. A path is not supported merely because a
build script can render an artifact.

## Nix reference implementation

`nix/batteries.nix` is the normative baseline: inputs are pinned by the flake;
Fontconfig points at the packaged Nerd Font; an immutable Alacritty template is
copied to writable per-user state; and the wrapper passes that exact config to
the terminal before launching Thegn. It avoids installing fonts or modifying an
existing terminal globally.

## Remaining platform decisions

Standalone Linux may add an explicit installer flag or adopt an equally
deterministic package-native artifact, but the opt-in boundary and exact
terminal/font/config ownership must be machine-testable. macOS must compose the
existing launcher/app tooling with the terminal and font into a real artifact.
Windows requires an explicit product decision; silence is not support.

## Verification and supply chain

Every claimed artifact needs a clean-host/VM rehearsal covering install,
launch, writable config, terminal identity/capabilities, font availability,
doctor output, upgrade, and uninstall/recovery as applicable. Exact commands
and artifact identity are persisted in release documentation. Artifacts must
derive from THE-52's checksummed and attested release inputs rather than an
untracked binary copy.
