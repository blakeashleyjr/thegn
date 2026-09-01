# Running CI locally

Two ways to reproduce the server-side gate on your machine.

## Fast path — `just` (recommended)

Each CI job in `.github/workflows/ci.yml` runs exactly one thing:
`nix develop --command just <stage>`. So the quickest way to run "the CI
checks" is to run those same recipes in your dev shell — no container, no nix
reinstall:

```sh
nix develop            # or: direnv allow (the dev shell is on PATH)
just quick [crate]     # cheap scoped iteration check
cargo nextest run -p <crate> <filter>  # only the tests you are touching
# …or a deliberate stage for diagnosis or a final/pre-push/PR gate:
just lint
just test
just smoke
just ci                # the CI gate; use just ci-local when e2e is intended
```

The scoped commands are the day-to-day loop. Individual heavy stages are useful
for deliberate diagnosis or the pre-push/PR boundary, but are not per-edit
commands. A green `just ci` locally covers the non-e2e CI gate (modulo
runner-only infrastructure — see below); `just ci-local` adds the local e2e
gate. Follow the dev-loop policy in `CLAUDE.md` and run the full gates once at
the appropriate boundary.

## Faithful path — `act`

[`act`](https://github.com/nektos/act) runs the actual GitHub Actions workflow
in a container. Use it to debug the **workflow itself** — the `ci-setup`
composite action, job matrix, event triggers, secrets wiring — not to run the
checks day-to-day. It is heavy: every job installs nix in the container and
cold-builds from scratch.

### One-time setup

1. `act` ships in the dev shell — enter it (`nix develop` / `direnv allow`).
2. Start a container engine: Docker, or podman with the socket exported —
   `export DOCKER_HOST="unix://$XDG_RUNTIME_DIR/podman/podman.sock"`.
3. Create the secrets file `act` expects:
   ```sh
   cp .secrets.example .secrets
   # NIX_GITHUB_TOKEN may be left blank
   ```
   `.secrets` is gitignored. Every flake input is public, so the token is
   optional — it only raises GitHub's anonymous API rate limit, which repeated
   in-container nix fetches can trip.

### Run

```sh
just act-list           # list the jobs act sees
just act-job name=lint  # run one job (start here — lint is the quickest)
just act                # run the whole workflow (push event)
just act -- --verbose   # pass extra flags through to act
just act-clean          # remove act's reused containers if one wedges
```

Defaults live in `.actrc`: the `catthehacker/ubuntu:act-latest` runner image,
`linux/amd64` (identical on Apple silicon), `--secret-file .secrets`, and
`--reuse` (keeps the job container so the in-container nix install + `/nix`
store survive between runs — without it every run reinstalls nix).

### Caveats

- **Slow.** The first run of a job installs nix and cold-builds the whole
  workspace + nix closure. `--reuse` amortizes this across later runs; a cold
  `nix-build` job can still take the better part of an hour.
- **Disk.** The images + `/nix` store are large; keep several GB free.
- **nix-in-container.** The DeterminateSystems installer runs without systemd
  inside the act container; if a job fails during nix install rather than during
  the build, that is an act/container-environment issue, not a repo bug — fall
  back to the fast path (`just <stage>`) for the actual check.
- **Not everything is wired for act.** Opt-in jobs (`e2e`, `macos`,
  `update-baselines`) gate on commit-message markers / `macos-15` runners and
  won't run meaningfully under act.

If you only want to know "will the gate pass?", use the fast path.

## Optional local-runner recipes

Local workflow runners are configured tools, not `CiProvider` implementations:
they have different event, credential, job-identity, and log semantics. If a
repository needs one, add it under `[[tools]]` and invoke it as a reproduction
helper; keep forge-backed CI inspection on the normal provider path.

- `act` reproduces GitHub Actions locally; the commands above use the checked-in
  workflow and `.secrets` setup.
- `gama` can be added as a configured command for a repository's local workflow
  reproduction.
- `wrkflw` can be added as a configured command for a repository-specific local
  workflow reproduction.

These tools do not populate the bounded CI log cache or change provider
selection. Treat their output as local diagnostics and use `thegn ci logs` for
the redacted forge-backed evidence used by the Work-tab drill and PR handoff.
