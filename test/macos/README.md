# macOS on-device evidence

This is the owner-run evidence gate for the Apple `container` CLI and the
Podman/Docker environments visible on the Apple-silicon Mac. It does not build
Thegn or upload evidence. The Mac run is required because Linux cannot verify
the native runtime's command dialect, network boundary, labels, or process
limits.

Run the hermetic helper checks anywhere:

```sh
just macos-ondevice-selftest
```

Run the live capture in an interactive Mac session after selecting a new,
private output directory:

```sh
just macos-ondevice "$HOME/spike-evidence/WS05-P0-01-$(date +%F-%H%M%S)"
```

The output directory must be private and owned by the current user. A run never
overwrites an earlier evidence file. The JSONL records macOS version,
architecture, commit, command status, output hash, and verdict; it omits raw
stdout/stderr. Raw case captures stay in the private output directory. They
receive basic username/path/host/address/image redaction, but may still include
runtime metadata that should not enter the repository. Review locally and
copy only the minimum whitelisted fields needed for a fixture or decision.
Never put credentials in `SPIKE_IMAGE` or the environment.

The Apple runtime cases use a per-run random network/container name. The host
reachability control binds a random port to one concrete interface address and
serves only a random canary response; it does not expose the current directory
or any user file. Cleanup is registered only for successfully created network
resources and targets only this run's random names. Podman and Docker probes
are read-only. If the process is forcibly killed before its trap runs, inspect
the exact per-run network/container name before removing it manually.

For the `network --internal` decision, the script first proves that a normal
container can start, reach an external IP directly (so DNS failure cannot look
like egress isolation), and reach the canary listener. An internal-network request counts as blocked only when the guest returns an
explicit network-denied error. Container startup failures, missing markers,
timeouts, listener failures, and missing host addressing are inconclusive.
The positive/negative decision must be reviewed against all per-case rows; a
nonzero command exit by itself is never evidence of isolation.
