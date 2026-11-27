# Self-hosted CI on ageless-studio

The CI workflow is the source of truth for triggers, jobs, and runner labels.
Linux jobs default to GitHub-hosted `ubuntu-latest`. Self-hosted runners are an
explicit opt-in through the repository variable `CI_RUNS_ON`. The macOS job
uses GitHub-hosted `macos-15`; Windows jobs use `windows-latest`.

## Security prerequisites

Assume the repository is public and all repository content, build scripts, and
dependencies are hostile. Fork pull requests run the lean CI jobs on
GitHub-hosted runners with a read-only token and no secrets; heavy jobs run on
nightly or dispatch. A dispatch run can execute the selected branch's code. A
persistent self-hosted runner gives that code access to its host account and
may let it poison `/nix/store`, sccache, `CARGO_HOME`, and the runner's `_work`
directory for later jobs.

Keep Linux jobs on GitHub-hosted runners unless the self-hosted host is isolated
from administrator credentials and unrelated workloads, and its credentials
and caches are isolated or disposable between untrusted jobs. Require approval
for **all outside contributors** in Settings → Actions. Approval is a human
gate; it does not isolate a runner or make approved code safe. If these
prerequisites cannot be met, do not opt in. Do not infer a narrower trust
policy from repository visibility or collaborator status.

## One-time setup

The runner setup script is in `scripts/ci/ageless-runner-setup.sh`. Run it on
ageless-studio as an operator account, never as root or `thegn-ci`. The runner
uses a dedicated system account named `thegn-ci`, with no login shell and no
sudo access. On NixOS, add the declaration printed by the script to
`configuration.nix` and rebuild. NixOS generates `/etc/systemd/system`, so the
script prints the declarative user/group module and refuses to install units
there.

Place a repository administration PAT in `/etc/thegn-ci/registration.token`
with owner `root:root` and mode `0600`, or provide `GH_PAT` once in the setup
environment. The script writes that environment value directly to the
root-only token file and removes it from its environment. The operator's `gh`
login is not used, and the token is never stored in the runner account or
passed to a job. `NIX_GITHUB_TOKEN` is optional: all flake inputs are public, so
no token is needed. Keep it unset because setup refuses it and it is not written
to a runner `nix.conf`.

To enter the token without putting it in shell history, run:

```sh
sudo install -d -m 0700 -o root -g root /etc/thegn-ci
read -rsp 'GitHub administration PAT: ' registration_token; printf '\n'
printf '%s' "$registration_token" | sudo install -m 0600 -o root -g root /dev/stdin /etc/thegn-ci/registration.token
unset registration_token
bash scripts/ci/ageless-runner-setup.sh
```

Setup refuses an unsafe runner account when it is the operator or root, belongs
to `sudo`, `wheel`, `admin`, `docker`, `podman`, `libvirt`, `kvm`, or `lxd`, has
sudo rights or an uncheckable sudo policy, has an interactive shell, or has
GitHub, SSH, GPG, Git, Docker, or Nix access-token credentials in its HOME.
Remove the credentials or group membership, remove sudo grants, and set the
shell to `/usr/sbin/nologin` before retrying.
It also refuses to proceed while legacy `actions.runner.*` systemd units or
`$HOME/actions-runners` from the persistent runner layout remain. Stop and
uninstall each old service with its runner directory's `sudo ./svc.sh stop`
and `sudo ./svc.sh uninstall`, remove its systemd drop-in, then deregister it
with `./config.sh remove --token <remove-token>` before retrying setup.

The root-owned systemd template starts a root-owned loop for each runner. Each
iteration deletes and recreates that runner's `_work` directory and HOME with
mode `0700`, asks GitHub for an encoded JIT configuration, and launches exactly
one ephemeral job in a transient systemd unit as `thegn-ci`. The unit applies
CPU and memory limits; its cgroup owns the job's descendants. GitHub removes
the ephemeral runner after its single job, and the loop obtains a fresh JIT
configuration for the next job. The JIT value is single-use and appears in the
argv of the `thegn-ci`-owned runner process.

The Nix multi-user daemon and `/nix/store` remain shared. There is no shared
writable Cargo registry or sccache, no runner cache bin directory on PATH, and
each job's HOME is wiped before use. Untrusted-event rejection, task-bound
short-lived credentials for protected jobs, authenticated shared caches, and
canary jobs also remain open on THE-414.

## Bootstrap artifacts are authenticated before execution

Setup executes no artifact it has not authenticated. The Nix multi-user daemon
comes from the exact Determinate Nix installer v3.24.2 Linux x86_64 release
asset; the Actions runner comes from the pinned
`actions-runner-linux-x64-2.338.2.tar.gz` release asset, not from
`releases/latest`. The version, exact byte size, SHA-256, and byte/time caps for
both live in `scripts/ci/ageless-runner-setup.sh` as reviewed data, shared with
`setup-macos.sh` through `scripts/lib/bootstrap-artifacts.sh`.

Each artifact is downloaded into a private staging directory over HTTPS with a
connect timeout, a stalled-transfer abort, a total time budget, and a hard byte
cap. Redirects are limited to HTTPS and their final origin must be an approved
GitHub release host, so a redirect cannot silently move the transfer elsewhere.
The pinned digest, not the host, decides what may execute: downloaded bytes are
checked against the reviewed size and SHA-256 before they run or are promoted
into a root-owned path. A runner archive already on disk is re-authenticated on
every run, so existence never substitutes for integrity. Before extraction, the
archive's member list is validated: paths must stay inside the tree, symlinks
must not escape it, and hardlinks, device nodes, duplicate destinations, empty
or malformed archives, and oversized expansions are refused.

The bare (no-Nix) macOS path in `setup-macos.sh` authenticates rustup the same
way: the pinned `rustup-init` 1.29.3 binary for the host architecture from
`static.rust-lang.org`, verified by size and SHA-256, then run. It no longer
pipes a remote shell script into `sh`, so the executable that runs cannot
change between review and use.

## Refresh a pinned bootstrap artifact

Pins are reviewed repository data, never fetched at run time. To move one:

1. Choose the exact release tag or tool version; never use a mutable
   `latest` endpoint.
2. Download the asset, record its byte size, and record its SHA-256. Cross-check
   the digest against a second source when the publisher provides one (for
   rustup, the official `rustup-init.sha256` next to the binary). The pins in
   this repository come from the GitHub release asset `digest` for the pinned
   tag (Determinate Nix installer, `actions/runner`) and from
   `https://static.rust-lang.org/rustup/archive/<version>/<target>/rustup-init.sha256`
   (rustup).
3. Update the version, size, digest, and any new byte/time cap together in one
   reviewed commit, so a partially refreshed pin can never validate.
4. If the artifact legitimately moves to a new origin host, extend the
   `GITHUB_RELEASE_HOSTS` / `RUSTUP_INIT_HOSTS` allowlist in the same commit.
5. Run `just test-bootstrap`, which exercises the pins, the origin allowlist,
   archive validation, and every refusal without touching the network or sudo.

Two isolation gaps remain open: all concurrent jobs use the same `thegn-ci`
UID, so one job may inspect or modify another job's processes and files; and
the account has no cron/at denial configured, so persistence through `crontab`
or `at` has not been closed. Do not treat the shared runner as suitable for
untrusted jobs until both are addressed.

## Verify runners

```sh
# GitHub: Settings → Actions → Runners
gh api repos/blakeashleyjr/thegn/actions/runners --jq '.runners[]|{name,status}'

# On the host
systemctl list-units 'thegn-ci-runner@*.service' --no-pager
journalctl -u 'thegn-ci-runner@*.service' -f
```

Pull requests run the lean CI jobs; heavy jobs also run nightly and by manual
dispatch. CI does not run on push events. Start a manual run explicitly with
`gh workflow run ci.yml --ref <branch>`. The optional
`e2e`, `macos`, `windows`, and `windows-artifact-smoke` jobs require
`-f extras=true`; they are disabled by default. Check `.github/workflows/ci.yml`
for the current job list and gates.

## Runner selection

The workflow uses this expression for Linux jobs:

```yaml
runs-on: ${{ fromJSON((github.event_name == 'workflow_dispatch' && github.repository == 'blakeashleyjr/thegn' && vars.CI_RUNS_ON) || '["ubuntu-latest"]') }}
```

### Opt in to self-hosted Linux runners

Only after the security prerequisites above are met, run:

```sh
gh variable set CI_RUNS_ON --body '["self-hosted", "ageless"]'
```

Self-hosted runners are selected only for `workflow_dispatch` runs of
`blakeashleyjr/thegn`; re-runs keep that event, and GitHub restricts dispatch and
re-run to users with write access. Every other event and every fork uses
GitHub-hosted `ubuntu-latest`, even if triggers such as `push`, `pull_request`,
`pull_request_target`, or `schedule` are restored.

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
# Restart a root-owned runner loop; its next iteration starts a fresh JIT job
sudo systemctl restart thegn-ci-runner@1.service

# Refresh the runner payload and loop configuration
bash scripts/ci/ageless-runner-setup.sh

# Inspect the shared Nix store
nix-collect-garbage -d

# Remove runner loops, their root-only token, and runner data
bash scripts/ci/ageless-runner-uninstall.sh

# Also remove the dedicated account (not available for a NixOS-declared user)
REMOVE_USER=1 bash scripts/ci/ageless-runner-uninstall.sh
```

The uninstaller handles the hardened ephemeral layout. For a legacy persistent
installation, use the manual `svc.sh` and `config.sh remove` steps above before
removing its old runner directory and drop-in.

Self-hosted jobs queue while the runner loop is offline; see the rollback
section above to restore GitHub-hosted Linux runners.
