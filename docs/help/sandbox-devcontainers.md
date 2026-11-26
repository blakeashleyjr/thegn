---
id: sandbox-devcontainers
title: Sandbox devcontainers
parent: sandboxing
order: 2
---

# Sandbox devcontainers

Part of [[sandboxing]]. Running a worktree in the repository's own devcontainer.

## Devcontainers

With the default `auto` mode, thegn discovers a repo's
`.devcontainer/devcontainer.json` or `.devcontainer.json`, parses JSONC, and
maps its supported subset onto the existing sandbox. To opt out globally or in
a trusted overlay:

```toml
[sandbox]
devcontainer = "off" # "auto" (default) | "off"
```

If neither primary file exists, thegn also discovers
`.devcontainer/<name>/devcontainer.json`. One variant is unambiguous; multiple
variants need a top-level selector in the repo's `.thegn.toml`:

```toml
devcontainer = "gpu"

[sandbox]
devcontainer = "auto"
```

The selector only picks a file. It does not approve anything in that file.
Review pending category requests and their ids with `thegn repo trust .`, then
approve one explicitly with `thegn repo trust . --approve <id>`.

### Security boundary

A repo JSON file is untrusted, just like any other cloned executable content.
thegn records trust-on-first-use approval separately for its image, build,
compose, mounts, ports, lifecycle, and features categories. An unapproved
category stays pending and is not applied; the worktree still opens. Editing a
request changes its canonical form and requires approval again.

Literal `containerEnv` and `remoteEnv` values do not prompt, but host-variable
substitution is clamped: `${localEnv:NAME}` is available only when `NAME` is in
the effective `[sandbox] env_passthrough` list. A blocked name becomes empty
and is reported without exposing its value.

Under a named profile (`--profile <name>`), ambient credentials are denied
by default for sandboxes, devcontainer `${localEnv:}` substitutions and
provider placements. Only infrastructure names such as `PATH`, `HOME`,
`TERM` and `LANG`, `LC_*`, `THEGN_*`, and names in
`[profiles.<name>.sandbox] env_passthrough` are shared; that profile list
replaces `[sandbox] env_passthrough`. At startup, the thegn process also unsets
names ending in `_TOKEN`, `_SECRET`, `_PASSWORD`, `_KEY`, `_AUTH`,
`_CREDENTIAL`/`_CREDENTIALS` or `_PASSPHRASE`, plus cloud/package names such as
`GOOGLE_APPLICATION_CREDENTIALS` and `DOCKER_AUTH_CONFIG`. It keeps
`SSH_AUTH_SOCK` only when the profile has `ssh/id`. Its git, gh and GnuPG
directories are mounted read-only in
sandboxes; a pane that needs to write them, such as `gh auth login` or GnuPG
signing, needs an explicit `[sandbox] mounts` grant. Startup refuses if the
profile state or credential directories cannot be created. Startup also
refuses if `ssh/id` exists but its target is not a regular file owned by you
with no group or other access.
The default profile is unchanged.
Named profile directories are created owner-only without following symlinks.
`thegn doctor` lists denied and shared names, never values. The share list is
read at startup.

Some repo requests are never eligible for approval. `privileged`, `capAdd`,
`securityOpt`, `runArgs`, and `init` could weaken the trusted sandbox, so thegn
always refuses them and names the reason. Bind mounts that resolve to a
container-engine socket, or contain one in an ancestor directory, are also
privilege requests: they are omitted from the generic mounts approval and
reported as `refused: engine-socket`. Binds of thegn's control or state
directories are refused the same way. Recognized fields that thegn does not
apply — such as `hostRequirements`, port attributes, `waitFor`,
`workspaceMount`, `overrideCommand`, `secrets`, remote-user/UID settings, and
compose overrides — are reported as reserved. Unknown top-level keys are also
reported and not applied; editor-only `customizations` is ignored silently.
This is the supported subset for this thegn version, not a promise to mirror
every current or future containers.dev field.

Named volumes declared by a devcontainer carry an owner label. A volume labelled
for another devcontainer is refused; a legacy unlabelled volume is claimed on
first local use, while an unlabelled volume on a remote host is refused.
Volumes configured under `[sandbox] volumes` are not owner-bound.

Supported native OCI behavior includes image, Dockerfile build, compose
service, mounts, forwarded ports, container/process environment, lifecycle
hooks, and in-container feature installs. `overrideFeatureInstallOrder` is
honored; fetched `installsAfter`/`dependsOn` metadata ordering, generated
Dockerfile feature layers, and `devcontainer.metadata` image-label merging are
reserved.

The image thegn builds for a devcontainer `build` is tagged from the build
declaration and Dockerfile contents, so changing the Dockerfile rebuilds it on
the next launch. If the Dockerfile cannot be read for hashing, thegn warns and
uses the declaration alone for the tag.

The image tag also covers files in the build `context`: their relative paths,
contents and symlink targets, but not file permission bits. A top-level `.git`
directory is skipped. Editing a context file rebuilds the image on the next
launch, and the first launch after upgrading rebuilds once because the tag
format changed. If the context cannot be digested (more than 2048 entries, more
than 16 MiB of file content, a path deeper than 32 levels, or an unreadable
file), thegn warns that it could not digest the build context, names the path
and the reason, and says the image tag covers only the Dockerfile (when
readable) and the build declaration. The build still runs.

### Lifecycle and provider fallback

After lifecycle approval, `initializeCommand` uses the host-side one-time
prepare hook. `onCreateCommand`, `updateContentCommand`, and
`postCreateCommand` are ordered one-time provisioning steps. In particular,
postCreate is not rerun for every pane. `postStartCommand` and
`postAttachCommand` use the existing per-pane init script and run before each
pane shell — thegn has no distinct attach event, so per-pane is its explicit
per-start/attach analogue.

For a local, fully approved config containing only safely applied fields, an
installed `devcontainer` CLI may be **CLI-ready**. If that bounded provider
probe is unavailable/degraded, the config is not safe to pass through raw, or
`devcontainer up` fails, thegn uses its existing native OCI path for the
supported approved subset. A non-OCI backend cannot honor an
image/build/compose source; doctor reports that degradation instead of
silently claiming the container shape was used. While falling back between
backends, a backend whose launch probe failed is remembered for the same
environment, placement, and worktree; later launches skip it with a warning that
the earlier probe failed, while current policy and isolation checks still run.
The memory is dropped when the config is reloaded, and for a worktree when you
retry, start its runtime, or run it on the host. An explicitly chosen backend is
always probed.

The `devcontainer` CLI provider's trust digest covers the selected
`devcontainer.json` and the bytes of its referenced Dockerfile and Compose
files inside the repository. The digest is checked before `devcontainer up`
and each exec; changing one of those files requires fresh approval. A missing,
absolute, or repository-escaping reference refuses provider use.
Before snapshotting the selected config and again before `devcontainer up`, the
CLI provider checks that `devcontainer.json` is still a regular file inside the
worktree. A symlink swap or non-regular file refuses provider use before
`devcontainer up` runs. The first check also runs before the snapshot
is written; if the later check refuses the CLI provider, thegn warns
and uses its native OCI path for the supported approved subset.
The trust digest also covers Compose `env_file`, build Dockerfile, `extends.file`
and `include` references, following nested files up to 4 levels and 256
references. Absolute, remote, interpolated or repository-escaping paths, and
references beyond either limit, refuse provider use. Every provider exec and
session lookup rechecks that both the selected config and its snapshot remain
regular files inside the worktree; a failed check refuses exec or drops the
session.

`thegn doctor` shows the `Devcontainer support` block: mode, repo, candidates,
selected file or selection error, provider, status, trust counts, and backend
honorability. It reads config and approvals and may run bounded
`devcontainer --version`; it never pulls, builds, starts, runs lifecycle code,
or makes a network probe just to report health.

The sidebar and active tab bar use `dc:<selected-path> [<state>]` (or
`dc:[<state>]` before selection). The transient states are `off`, `ambiguous`,
`invalid`, `pending`, `ready`, and `degraded`. `ready` means CLI-ready;
`degraded` can still mean the supported subset is running through the native
OCI fallback. Re-run doctor for the detailed reason.
