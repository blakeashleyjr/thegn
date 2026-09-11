# Tasks — package-channel release readiness

## 1. Shipped build and render baseline

- [x] 1.1 Define enabled/disabled targets, archive contract, and manager
      outputs in `packaging/release.json`.
- [x] 1.2 Implement deterministic Homebrew/AUR/nfpm rendering and strict
      archive/output validation with packaging tests.
- [x] 1.3 Build/upload deb and rpm release assets plus rendered manager metadata
      from validated same-tag inputs.
- [x] 1.4 Attach keyless GitHub OIDC provenance and checksums to release
      artifacts.
- [x] 1.5 Implement protected, credential-checked, content-idempotent Homebrew
      and AUR publication, including generated `.SRCINFO`.

## 2. External setup and public rehearsals

- [ ] 2.1 Complete/document the public Homebrew tap and AUR package/deploy-key
      configuration.
- [ ] 2.2 Publish a real release to Homebrew/AUR, install from both public
      channels, and record version/checksum/provenance plus same-tag rerun.
- [ ] 2.3 Install deb and rpm assets on clean representative systems and record
      install, upgrade, and uninstall behavior.
- [ ] 2.4 Verify all published metadata resolves only same-release
      checksummed/attested assets.

## 3. Decisions and documentation

- [x] 3.1 Defer Scoop/winget until THE-140's artifact leg and THE-145's native
      parity evidence are green; create
      bounded Scoop/winget work or explicitly defer both.
- [x] 3.2 Defer hosted apt/rpm repositories and crates.io through public alpha;
      create bounded follow-ups only if their operational cost is accepted.
- [x] 3.3 Update installation docs to advertise only successfully rehearsed
      channels and preserve evidence/commands in release documentation.

## 4. Reconciliation

- [x] 4.1 Reconcile this change with the delivered manifest/render/attestation
      baseline and the remaining Linear acceptance criteria.
- [ ] 4.2 Validate the completed change strictly before archive.
