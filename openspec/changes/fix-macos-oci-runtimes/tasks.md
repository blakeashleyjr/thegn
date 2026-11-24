## Harness

- [x] Add the private Mac evidence runner and shared case helpers.
- [x] Capture Apple `container` and read-only Podman/Docker metadata.
- [x] Add hermetic tests for result classification, redaction, and overwrite refusal.
- [x] Document the owner-run invocation and safety constraints.
- [x] Run `test/macos/selftest.sh` on a non-Mac host.
- [ ] Run `just macos-ondevice <new-private-outdir>` on the owner's Apple-silicon Mac.
- [ ] Review all evidence rows and record the runtime decisions before changing enforcement.

## podman-rootful (THE-2131)

- [x] Gate `podman-rootful` to Linux in `backend_runs_on` and add the macOS remedy text.
- [x] Add the `platform-macos` spec delta and update the backends help page.
