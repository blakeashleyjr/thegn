# Tasks — batteries-included distributions

## 1. Shipped Nix baseline

- [x] 1.1 Package Thegn, Alacritty, and Fira Code Nerd Font with generated
      Fontconfig state and a writable user-local Alacritty config wrapper.
- [x] 1.2 Export the Nix batteries package and document `nix run .#batteries`.
- [x] 1.3 Diagnose missing Alacritty in the ordinary installer and allow macOS
      packaging to consume an explicit Alacritty config.

## 2. Platform contract

- [ ] 2.1 Publish the exact Linux Nix/non-Nix, macOS, and Windows support
      matrix, including bundled/delegated components and fallbacks.
- [ ] 2.2 Add explicit standalone-Linux batteries mode or a deterministic
      package-native equivalent without implicit terminal/font replacement.
- [ ] 2.3 Complete the macOS terminal/font/config/binary artifact and launcher.
- [ ] 2.4 Implement Windows batteries support or explicitly defer it to a
      bounded issue with tested fallback instructions.

## 3. Diagnostics and evidence

- [ ] 3.1 Make doctor/startup failures actionable across terminal, font,
      writable config, launcher, and verified-binary components.
- [ ] 3.2 Rehearse every claimed artifact on a clean host/VM and persist exact
      install/launch/upgrade/uninstall evidence.
- [ ] 3.3 Prove every artifact consumes THE-52 checksummed/attested release
      inputs.

## 4. Reconciliation

- [x] 4.1 Reconcile this delta with the shipped Nix baseline and the remaining
      Linear acceptance criteria.
- [ ] 4.2 Validate the completed change strictly before archive.
