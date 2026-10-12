# Self-hosted CI on ageless-studio

The CI workflow is the source of truth for triggers, jobs, and runner labels.
Linux jobs default to GitHub-hosted `ubuntu-latest`. Self-hosted runners are an
explicit opt-in through the repository variable `CI_RUNS_ON`. The macOS job
uses GitHub-hosted `macos-15`; Windows jobs use `windows-latest`.

## Security prerequisites

Assume the repository is public and all repository content, build scripts, and
dependencies are hostile. The workflow is currently dispatch-only, but a
future pull request trigger could execute fork code. A dispatch run can also
execute the selected branch's code. A persistent self-hosted runner gives that
code access to its host account and may let it poison `/nix/store`, sccache,
`CARGO_HOME`, and the runner's `_work` directory for later jobs.

Keep Linux jobs on GitHub-hosted runners unless the self-hosted host is isolated
from administrator credentials and unrelated workloads, and its credentials
and caches are isolated or disposable between untrusted jobs. Require approval
for **all outside contributors** in Settings → Actions. Approval is a human
gate; it does not isolate a runner or make approved code safe. If these
prerequisites cannot be met, do not opt in. Do not infer a narrower trust
policy from repository visibility or collaborator status.

## One-time setup

The runner setup script is in `scripts/ci/ageless-runner-setup.sh`. Before
running it, satisfy the security prerequisites above. Run **on ageless-studio,
as `targe`** (SSH over Tailscale or use a thegn `env=ageless` shell):

```sh
git clone https://github.com/blakeashleyjr/thegn   # or cd an existing checkout
cd thegn

NIX_GITHUB_TOKEN="<optional PAT>" \
GH_PAT="<PAT with repo admin>" \
  bash scripts/ci/ageless-runner-setup.sh
```

`NIX_GITHUB_TOKEN` is optional. All flake inputs are public; this token only
raises GitHub's anonymous API rate limit during flake fetches. `GH_PAT` is used
to mint runner registration tokens. If `gh` is already logged in as a repo
admin on the host, it can be omitted.

The script defaults to four runners, three CPUs each, labels
`self-hosted,linux,x64,ageless,nix`, and persistent warm caches. Review its
current settings and isolation before enabling the repository variable.

## Verify runners

```sh
# GitHub: Settings → Actions → Runners
gh api repos/blakeashleyjr/thegn/actions/runners --jq '.runners[]|{name,status}'

# On the host
systemctl list-units 'actions.runner.*' --no-pager
journalctl -u 'actions.runner.*' -f
```

The `workflow_dispatch` trigger is the only enabled trigger: CI does not run on
push or pull request events. Start a run explicitly with
`gh workflow run ci.yml --ref <branch>`. The optional
`e2e`, `macos`, `windows`, and `windows-artifact-smoke` jobs require
`-f extras=true`; they are disabled by default. Check `.github/workflows/ci.yml`
for the current job list and gates.

## Runner selection

The workflow uses this expression for Linux jobs:

```yaml
runs-on: ${{ fromJSON(vars.CI_RUNS_ON || '["ubuntu-latest"]') }}
```

### Opt in to self-hosted Linux runners

Only after the security prerequisites above are met, run:

```sh
gh variable set CI_RUNS_ON --body '["self-hosted", "ageless"]'
```

### Roll back to the GitHub-hosted default

To remove the override and restore `ubuntu-latest`, run:

```sh
gh variable delete CI_RUNS_ON
```

Inspect the current setting; the fallback shown for an unset variable is the
actual workflow default:

```sh
gh variable get CI_RUNS_ON 2>/dev/null || echo "(unset → GitHub-hosted ubuntu-latest)"
```

The `macos-15` and `windows-latest` jobs do not use `CI_RUNS_ON`.

## Host operations

```sh
# Restart a runner
sudo systemctl restart actions.runner.blakeashleyjr-thegn.ageless-1.service

# Update the runner binary
bash scripts/ci/ageless-runner-setup.sh

# Inspect or clear the shared cache (clearing it affects every runner)
du -sh /home/targe/gha-cache/*
sccache --stop-server; rm -rf /home/targe/gha-cache/sccache/*
nix-collect-garbage -d

# Remove the runner services
GH_PAT=<admin PAT> bash scripts/ci/ageless-runner-uninstall.sh
```

The Linux `test` job sets global git identity on its runner account. Use a
dedicated account so this does not affect interactive work. Self-hosted jobs
queue while the runner is offline; see the rollback section above to restore
GitHub-hosted Linux runners.
