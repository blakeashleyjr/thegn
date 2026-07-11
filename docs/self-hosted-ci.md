# Self-hosted CI on ageless-studio

CI **supports both** GitHub-hosted and self-hosted runners and **defaults to
self-hosted**: **4 runners** on `ageless-studio` (user `targe`, reached over
Tailscale), each capped at **3 CPUs**. Because they're persistent and share warm
host-level caches, the builds that used to cold-compile for the better part of
an hour on GitHub (`nix-build`, `test`, `coverage`) are near-instant after the
first warm run — and cost no GitHub-hosted minutes.

Both are wired so you can flip between them with **one repo variable, no workflow
edits** (see _Switching runners_ below). The opt-in **`macos`** job is always
GitHub-hosted (no Apple hardware on ageless).

## One-time setup

Run **on ageless-studio, as `targe`** (SSH in over Tailscale, or use a thegn
`env=ageless` shell):

```sh
git clone https://github.com/blakeashleyjr/superzej   # or cd an existing checkout
cd thegn

NIX_GITHUB_TOKEN=<classic PAT, repo scope> \  # private flake inputs (muse, termite-chat)
GH_PAT=<PAT with repo admin> \                # to auto-register the runners
  bash scripts/ci/ageless-runner-setup.sh
```

`NIX_GITHUB_TOKEN` is baked into the host's `~/.config/nix/nix.conf` as an
`access-tokens` entry, so jobs resolve private flake inputs **without** a
workflow secret. `GH_PAT` is only used to mint runner registration tokens — if
`gh` is already logged in as a repo admin on the box, you can omit it.

The script is idempotent (re-run to add runners, refresh config, or change
caps). Knobs (all overridable via env, with these defaults):

| var             | default                             | meaning                                          |
| --------------- | ----------------------------------- | ------------------------------------------------ |
| `RUNNER_USER`   | `targe`                             | account that owns the runners                    |
| `RUNNER_COUNT`  | `4`                                 | number of runners                                |
| `RUNNER_CPUS`   | `3`                                 | CPU cap per runner (`CPUQuota=RUNNER_CPUS*100%`) |
| `RUNNER_LABELS` | `self-hosted,linux,x64,ageless,nix` | runner labels                                    |
| `CACHE_BASE`    | `/home/targe/gha-cache`             | shared warm-cache root                           |
| `SCCACHE_SIZE`  | `60G`                               | sccache ceiling                                  |

### Prerequisites on the box

- **systemd** (WSL: `systemd=true` in `/etc/wsl.conf`, then `wsl --shutdown`).
- `sudo`, plus `curl`, `tar`, `jq`.
- Enough cores: `RUNNER_COUNT × RUNNER_CPUS` = 12 by default. Fewer cores still
  works (`CPUQuota` is a limit, not a reservation) but runners will contend; the
  script warns.

## What it sets up

- **Host nix** (Determinate Systems installer if absent) with a tuned
  `nix.conf`: flakes on, `cores = 3`, `keep-outputs`/`keep-derivations = true`
  (so the dev-shell closure survives GC), download-resilience, and the
  private-input access token.
- **4 runners** under `/home/targe/actions-runners/ageless-{1..4}`, each a
  systemd service, auto-start + auto-restart, capped via a
  `CPUQuota=300%` drop-in (+ `MemoryHigh=12G` so a wedged job can't OOM the box).
- **Warm caches**, shared across all runners and injected into every job via the
  runner's official `.env` / `.path` files:
  - persistent **`/nix/store`** — the big one: `nix build .#default` and
    `nix develop` become cache hits, not cold builds.
  - shared **sccache** dir (`RUSTC_WRAPPER=sccache`, `SCCACHE_DIR`,
    `SCCACHE_CACHE_SIZE=60G`) — warm Rust crate compiles across jobs and runners.
  - shared **`CARGO_HOME`** — the crate registry isn't re-downloaded.
  - each runner's **`_work/`** persists → incremental `target/` between jobs.

## Verify

```sh
# on GitHub: Settings → Actions → Runners  (expect 4 × ageless-* Idle)
gh api repos/blakeashleyjr/superzej/actions/runners --jq '.runners[]|{name,status}'
# on the box:
systemctl list-units 'actions.runner.*' --no-pager
journalctl -u 'actions.runner.*' -f          # live job logs
```

Then push a commit (or re-run a workflow) and watch the jobs land on the
`ageless-*` runners.

## The workflow side

- `.github/workflows/ci.yml`: every Linux job is
  `runs-on: ${{ fromJSON(vars.CI_RUNS_ON || '["self-hosted", "ageless"]') }}` —
  one source of truth, defaulting to the self-hosted ageless runners. `macos`
  stays `macos-14`.
- `.github/actions/ci-setup`: every step is gated on
  `runner.environment == 'github-hosted'`, so on ageless it's a **no-op** — nix
  and the caches are already on the host. It still installs nix + restores
  caches for the GitHub-hosted macOS job (and for any Linux job if you toggle
  back to `ubuntu-latest`).

The job-level `timeout-minutes` (test 50, nix-build 60) are unchanged — they're
now generous ceilings that a warm cache never approaches.

## Switching runners

Both runner types are fully supported; the `CI_RUNS_ON` repo variable selects
which the Linux jobs use — no workflow edits, takes effect on the next run:

```sh
# ← self-hosted ageless (the default; also: just delete the variable)
gh variable delete CI_RUNS_ON

# → back to GitHub-hosted (e.g. ageless is down, or for a one-off comparison)
gh variable set CI_RUNS_ON --body '["ubuntu-latest"]'

# inspect current setting
gh variable get CI_RUNS_ON 2>/dev/null || echo "(unset → self-hosted)"
```

Because `ci-setup` keys off `runner.environment`, both targets Just Work: on
`ubuntu-latest` it installs nix + restores the GitHub caches; on `ageless` it
no-ops onto the warm host caches.

## Operations

```sh
# restart / stop a runner
sudo systemctl restart actions.runner.blakeashleyjr-thegn.ageless-1.service

# update the runner binary (GitHub auto-updates runners, but to force it):
bash scripts/ci/ageless-runner-setup.sh        # re-run; picks the latest release

# reclaim cache space if it grows unbounded
du -sh /home/targe/gha-cache/*
sccache --stop-server; rm -rf /home/targe/gha-cache/sccache/*   # nuke sccache
nix-collect-garbage -d                                          # nix store GC

# tear everything down (keeps caches unless PURGE_CACHES=1)
GH_PAT=<admin PAT> bash scripts/ci/ageless-runner-uninstall.sh
```

## Notes & caveats

- **Security.** Self-hosted runners execute workflow code as `targe` with access
  to the host. This is safe here because the repo is **private** (only
  collaborators can trigger CI). Do NOT make the repo public without switching to
  ephemeral runners + strict fork-PR gating — a fork PR would otherwise run
  arbitrary code on ageless.
- **Global git config.** The `test` job runs `git config --global` (the svc git
  tests need a default branch + identity). On a persistent runner this sets
  `targe`'s global git identity to the CI values; harmless for a dedicated CI
  account, but be aware if `targe` is used interactively.
- **Availability.** If ageless is offline, self-hosted jobs queue until a runner
  is back. That's the point of keeping GitHub-hosted supported: flip
  `CI_RUNS_ON` to `["ubuntu-latest"]` (see _Switching runners_) to run on
  GitHub's runners until ageless returns.
- **Concurrency.** 4 runners → up to 4 jobs at once; the ~13 Linux jobs queue
  across them. sccache and the cargo registry are concurrency-safe; each runner
  has its own `_work/target`, so parallel builds don't corrupt each other.
