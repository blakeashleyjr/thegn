# Design

## Decisions

- **Opt-in, default off.** Unattended merge stays off unless the owner sets
  `gate_sandbox = "sealed"`.
- **Never fall back to the host.** Any failure to seal holds the land and asks.
- **Refuse host-reaching mounts.** A sandbox with the Nix daemon socket, or any
  socket or device mount, is refused: `--unshare-net` does not block a bound socket.
- **`gate_target_dir` is ignored** when sealed: the target dir is the sandbox's own bind.
- **Only `regenerate_command` is repo-influenced** among the fold steps, so it moves in.
- **Honest cost.** Gates that download dependencies or run `nix develop` cannot
  run sealed; those lands still ask.
