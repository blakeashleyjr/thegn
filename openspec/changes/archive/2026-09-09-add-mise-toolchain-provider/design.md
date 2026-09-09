# Design — mise toolchain activation

## Generic activation seam

Core represents detected files, their canonical content identity, activation
policy, provider state, and ordered PATH/environment layers without executing a
tool. The host `mise_provider` is the adapter that knows the external binary,
local filesystem, remote/provider execution, trust store, cache, and refresh
channel. This keeps launch composition provider-shaped without claiming a
runtime-loadable plugin.

## Detection and identity

Local detection and `DETECT_PROBE_SCRIPT` recognize the same project-level mise
surface: `mise.toml`, `.mise.toml`, local/nested config variants, bounded
`conf.d/*.toml`, the selected `mise.<MISE_ENV>.toml`, `.tool-versions`, and
common language version files. The identity hashes the worktree identity,
ordered relative names, contents, and `mise.lock` when present. Remote caches
store target-derived facts and never infer approval from host-local files.

## Authorization

Shims mode prepends a directory and does not evaluate repository config, so it
is the unapproved fallback for `auto` and `env`. Full environment resolution
and explicit install use the content-bound `mise.env` repository-trust request.
Thegn never invokes `mise trust`; that command would mutate a second ambient
trust database and weaken the single authorization source.

The explicit install entry points call only `mise install`, after re-reading
the target identity and confirming current Thegn approval. Normal activation
never installs anything.

## Off-loop cache contract

Local and remote resolution/probes run on bounded workers. Owner-only state
files contain identity-stamped activation data. A launch performs no child
process or remote I/O: it consumes a matching cache or returns shims/reserved
safe-base state while an off-loop refresh is scheduled. Completion uses the
existing refresh channel and terminal waker.

## Composition and diagnostics

PATH precedence is bundle, devshell, mise, base. Mise environment values are
fill-only and credential-shaped keys are discarded. Doctor reports the cached
provider state, selected provisioning tier, injection mode, trust status,
detected files, shims, and degradation reason. It does not promise the external
binary's version and does not execute repo-authored resolution as a probe.
