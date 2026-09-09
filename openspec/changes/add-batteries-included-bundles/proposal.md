# Complete batteries-included distributions per platform

Linear: THE-15

## Problem

A bare Thegn binary does not guarantee the terminal, Nerd Font, generated
configuration, writable state, and diagnostics needed for the intended
experience. The batteries path must be explicit and reproducible without
silently replacing a user's terminal or system-wide font configuration.

## Shipped baseline

- `nix run .#batteries` and the batteries package compose Thegn, Alacritty,
  Fira Code Nerd Font, generated Fontconfig state, and a writable Alacritty
  config copy, then launch with `THEGN_ALACRITTY_CONFIG`.
- The flake exports the package and README/help documents the Nix path.
- The ordinary installer reports missing Alacritty; macOS app packaging can
  consume an explicitly supplied Alacritty configuration.
- The Nix implementation landed in `ed3908ef` and is the executable reference
  for other platforms.

## Remaining work

- Publish a precise Linux Nix/non-Nix, macOS, and Windows support matrix.
- Provide an explicit standalone-Linux batteries mode or a deterministic,
  tested package-native equivalent.
- Produce and rehearse a macOS batteries artifact/launcher.
- Implement the Windows path or explicitly defer it to a bounded issue with
  tested fallback instructions.
- Make doctor/startup failures actionable for missing terminal, font, writable
  config, and launcher components for every claimed path.
- Rehearse each claimed artifact on a clean host/VM and preserve exact commands
  and evidence in release documentation.
- Consume only checksummed/attested release inputs owned by THE-52.

## Non-goals

- Replacing an existing user's terminal without opt-in.
- Writing system-wide fonts/config when a self-contained or user-local path
  works.
- Supporting every terminal emulator or owning registry publication.

## Dependency

THE-52 owns provenance, rendered release artifacts, publication, and registry
rehearsal. This change owns the contents and launch fidelity of each
batteries-included artifact.
