# Platform: native Windows

## ADDED Requirements

### Requirement: Windows backends climb the D5 ladder and are verified only by device evidence

Windows backend support SHALL be built in the D5 ladder order: Job Object limits with
truthful reporting first; then WSL2 as a placement through a generated argv connector,
with the Linux sandbox composed inside the distro by a version-matched Linux thegn;
then the OCI family on Windows, where containers inside WSL come first and native
Docker Desktop, podman machine and `wslc` stay behind an opt-in; then native isolation
through a small separate helper using Microsoft MXC's process security environment
(PSEC) on Windows 11 24H2 or later, failing closed on anything else. A Windows backend
row MUST NOT be reported `Verified` or `Attested` until all of the following hold: a
committed evidence file names the device, the OS build and UBR, the thegn commit and
the helper version, with an attestation row in `sandbox_attestations`; the backend's
mandatory on-device checks pass (`jobobject` J1-J3 and J5-J7; `mxc` F1-F4, N1-N2, N4
decided, P1-P3 and R1-R2; the WSL connector recipe `live-tested` W1-W6 and W8-W10;
every Windows OCI row its O checks plus its `--cpus`/`--memory` limit checks); an
opt-in CI run is green for everything CI can run; and the doctor output in the evidence
matches the manifest row. Until then the row stays unverified, `WinIsolation` stays
Experimental, and `jobobject` limits are reported as pending on-device validation.

#### Scenario: A row without a device evidence file stays unverified

- **WHEN** a Windows backend has a green opt-in CI run but no committed evidence file
  naming the device and the OS build
- **THEN** its row is not `Verified` or `Attested`, and `WinIsolation` stays
  Experimental

#### Scenario: The ladder lifts no secure-policy refusal by itself

- **WHEN** a secure policy resolves a backend for a pane on a Windows host
- **THEN** native Job Object containment is refused, WSL placement with an inner Linux
  backend is refused until WS06-P2-10 evidence, and MXC is refused until WS06-P4-08
  evidence
