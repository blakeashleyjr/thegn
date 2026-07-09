# Running CI locally

Two ways to reproduce the server-side gate on your machine.

## Fast path — `just` (recommended)

Each CI job in `.github/workflows/ci.yml` runs exactly one thing:
`nix develop --command just <stage>`. So the quickest way to run "the CI
checks" is to run those same recipes in your dev shell — no container, no nix
reinstall:

```sh
nix develop            # or: direnv allow (the dev shell is on PATH)
just ci                # the whole gate: fmt + lint + build + test + coverage + smoke + …
# …or a single stage while iterating:
just lint
just test
just smoke
```

This is the same code the runners execute, so a green `just ci` locally means a
green gate on GitHub (modulo the runner-only infra — see below). Follow the
dev-loop policy in `CLAUDE.md`: iterate with `just quick`, run the heavy gates
once before pushing.

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
3. Provide the token the workflow needs:
   ```sh
   cp .secrets.example .secrets
   # edit .secrets: NIX_GITHUB_TOKEN=<classic PAT with repo scope>
   ```
   `.secrets` is gitignored. The token lets in-container nix fetch the private
   flake inputs (muse, termite-chat).

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
  `update-baselines`) gate on commit-message markers / `macos-14` runners and
  won't run meaningfully under act.

If you only want to know "will the gate pass?", use the fast path.
