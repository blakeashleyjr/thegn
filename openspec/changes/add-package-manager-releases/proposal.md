# Finish verified package-channel publication

Linear: THE-52

## Problem

The release pipeline can deterministically build and render several package
outputs, but a generated template is not a supported public channel until its
repository credentials, publication, clean-host install, upgrade/uninstall,
and rerun behavior have been rehearsed. “Every possible package manager” is
intentionally narrowed to channels explicitly accepted and maintained.

## Shipped baseline

- `packaging/release.json` is the machine-readable contract for Linux
  GNU/musl, Darwin, Homebrew, AUR, and nfpm deb/rpm outputs. Windows,
  Scoop, and winget are explicitly disabled.
- The tag workflow validates required archive/checksum inputs, renders manager
  metadata deterministically, builds deb/rpm assets, uploads checksums, and
  emits keyless GitHub build-provenance attestations.
- Homebrew/AUR publication is protected/manual, validates its environment and
  narrowly scoped repository/deploy configuration, regenerates `.SRCINFO`, and
  pushes only when content changes.
- Renderer/artifact behavior is covered under `packaging/tests` and landed in
  `47d675cc`, `b5848513`, and `c300c51e`.

## Remaining work

- Complete the one-time public Homebrew tap and AUR package/deploy-key setup.
- Publish and install from both real channels, recording version/checksum and
  idempotent rerun evidence.
- Install/rehearse deb and rpm assets on clean representative systems and
  document upgrade/uninstall behavior.
- Decide Windows publication only after Windows artifacts are supported; split
  accepted Scoop/winget work or explicitly defer it.
- Decide whether hosted apt/rpm repositories and crates.io are worth their
  operational cost, creating bounded follow-ups only for accepted channels.
- Ensure published metadata references only checksummed/attested assets and
  advertise only rehearsed channels.

## Non-goals

- Publication to every registry in existence.
- Claiming support from template generation alone.
- Bundling terminals/fonts (THE-15) or creating hosted infrastructure before a
  product decision accepts it.

## Dependency and overlap

THE-52 owns release artifacts, provenance, package metadata, publication, and
registry rehearsal. THE-15 consumes those verified inputs and owns the complete
batteries-included runtime experience.
