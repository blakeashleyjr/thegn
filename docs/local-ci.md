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

## Checkout hooks and linked worktrees

The dev shell installs only the generated pre-commit/pre-merge/pre-push gates.
It does not install a `post-checkout` hook, seed `.pre-commit-config.yaml`, or
run a healer after checkout/worktree creation. Git checkout and `git worktree
add` therefore do not execute files selected by the branch being entered.

`PREK_ALLOW_NO_CONFIG=1` remains an optional local-development compatibility
setting for a missing generated config; it is not a test waiver or a security
boundary. Review a linked worktree before explicitly configuring its local
prek hooks. CI and explicit merge gates are the authoritative enforcement
path.

On shell entry, immutable trusted detector code performs a bounded, read-only
check for the exact known legacy `post-checkout` bytes. It accepts only the
canonical checkout's ordinary `.git/hooks` directory, optionally when local
`core.hooksPath` resolves exactly to that same directory; custom/global/shared
paths and symlinks/special files are reported and untouched. If the exact
legacy file is found, remove it manually
after reviewing it (for example, after confirming the reported digest and
running `rm -- .git/hooks/post-checkout` from that canonical checkout). The
detector never performs that deletion, and unknown files must remain untouched.

`just heal-git` is still available as an explicit developer repair command for
a wedged shared Git config. It executes the checked-out shell utility only by
deliberate invocation and is not generation-fenced or race-safe; it is not a
checkout-hook security boundary. That shared-config repair is tracked by
THE-371.

## Docker compatibility shim

Entering the dev shell may create the persistent symlink
`~/.docker/docker.sock` when `DOCKER_HOST` is unset, the Podman socket is
available, and that path does not already exist. The shim never replaces an
existing socket, file, directory, or symlink, and it does not remove other
links. Set `DOCKER_HOST` explicitly to opt out of this compatibility path.

To recover, remove the link only after confirming it points to the Podman
socket:

```sh
if [ "$(readlink "$HOME/.docker/docker.sock")" = "$XDG_RUNTIME_DIR/podman/podman.sock" ]; then
  rm -- "$HOME/.docker/docker.sock"
fi
```

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
3. No secrets file is needed for the default anonymous run. All current flake
   inputs are public. If you deliberately need a token for a trusted workflow
   debug, create `.secrets` yourself and opt in on that invocation.

### Run

```sh
just act-list           # list the jobs act sees
just act-job name=lint  # run one job (start here — lint is the quickest)
just act                # run the whole workflow (push event)
just act -- --verbose   # pass extra flags through to act
just act -- --secret-file .secrets  # explicitly expose credentials for this run
just act -- --reuse     # explicitly retain the act job container
just act-clean          # remove act containers if one wedges
```

Defaults live in `.actrc`: the `catthehacker/ubuntu:act-latest` runner image
and `linux/amd64` (identical on Apple silicon). The default run does not load a
secret file and does not reuse its job container. `--secret-file .secrets` is
an explicit credential opt-in: it gives the checked-out branch's workflow and
composite-action code access to the token, so use it only when you trust that
code. The token is unnecessary for anonymous access to current public flake
inputs. `--reuse` explicitly retains the container and its mutable `/nix`
state; avoid carrying it across revisions or trust domains. Both modes execute
workflow and local composite-action files from the checked-out revision, and
`.actrc` still selects the mutable `act-latest` image tag. These defaults do not
admit a reviewed workflow revision or authenticate the runner image; use only
branches you trust. Image digest admission is tracked separately by THE-413.

### Caveats

- **Slow.** Each disposable run installs nix and cold-builds the whole
  workspace + nix closure; a cold `nix-build` job can still take the better
  part of an hour.
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
  workflow anonymously by default. Credentials and reuse require explicit
  flags for an individual invocation.
- `gama` can be added as a configured command for a repository's local workflow
  reproduction.
- `wrkflw` can be added as a configured command for a repository-specific local
  workflow reproduction.

These tools do not populate the bounded CI log cache or change provider
selection. Treat their output as local diagnostics and use `thegn ci logs` for
the redacted forge-backed evidence used by the Work-tab drill and PR handoff.
