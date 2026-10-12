# Design — package-installed shell completions

## The layer question, answered

Three candidate layers, and where each lands:

1. **Runtime verb** (`thegn completions <shell>`) — exists, spec'd, correct:
   generated from the live clap definition so it cannot drift from the
   installed binary; names the script after `argv[0]` so `tg` gets a `tg`
   completer. This stays the single source of truth.
2. **Build-time generation into the package** — the gap this change fills.
   nixpkgs convention: files in `$out/share/{bash-completion/completions,
zsh/site-functions,fish/vendor_completions.d}` are picked up by NixOS,
   home-manager and nix-darwin shell integration with zero user wiring.
   Because the package pins the binary, "stale after update" cannot happen —
   a new store path regenerates its own completions.
3. **User-wired eval/static** (agent-of-empires' preferred pattern) — the
   fallback for tarball installs; becomes documentation, not machinery.

A clap `build.rs` generation step (the other common Rust approach) was
rejected: it duplicates the command tree at build time, does not know the
final binary names (channel rename, alias), and produces artifacts the nix
layer would have to find — running the installed binary in `postInstall` is
simpler and exercises the real code path.

## postInstall mechanics

```nix
postInstall = ''
  # ... existing mv/ln/wrapProgram ...
'' + lib.optionalString (stdenv.buildPlatform.canExecute stdenv.hostPlatform) ''
  export HOME=$(mktemp -d) XDG_CONFIG_HOME=$HOME/.config XDG_STATE_HOME=$HOME/.state
  for bin in ${binName} ${aliasName}; do
    installShellCompletion --cmd $bin \
      --bash <($out/bin/$bin completions bash) \
      --zsh  <($out/bin/$bin completions zsh)  \
      --fish <($out/bin/$bin completions fish)
  done
'';
```

- `installShellFiles` joins `nativeBuildInputs`; `installShellCompletion`
  owns the share-dir paths.
- **Invocation via each name matters**: the verb names the completion target
  after `argv[0]`, so `$out/bin/tg completions zsh` emits a `#compdef tg`
  script. Generating only for `thegn` and copying would complete the wrong
  command name.
- **Wrapper interaction**: `wrapProgram` runs before this; the wrapped binary
  sets `THEGN_YAZI_BIN`/`PATH` and then clap parses `completions` — no
  daemon, no DB, no network. The `HOME`/XDG exports keep the tolerant config
  load from probing `/homeless-shelter` (missing file → defaults; the verb
  never reads config values, but the load runs before dispatch).
- **Dev channel**: `binName`/`aliasName` are already `thegn-dev`/`tg-dev`
  under `isDev`, so the loop produces non-colliding files for side-by-side
  installs for free.
- **Cross builds** (`check-cross`, any future `pkgsCross` consumer): the
  `canExecute` guard skips generation — a package without completions beats a
  broken build. Same fail-safe direction as the sandbox cap wrap.

## Verification

`just ci`'s `nix-build` step builds the package; a `postInstall` failure
fails it. The spec scenario ("share/ contains completions for both names") is
checked there — no new Rust test surface, no coverage impact
(`thegn-core` untouched). Event loop / render channels: not touched (packaging
only).

## Security

- **Build-time execution**: the freshly built binary runs inside the nix
  build sandbox — no network, no user state, an empty throwaway `HOME`. It
  can exfiltrate nothing and reads nothing but its own clap tree.
- **Provenance**: the installed completion scripts are pure
  `clap_complete` output derived from the source being built; no third-party
  script is fetched or vendored.
- **No new runtime surface**: completions are data for the user's shell; the
  binary gains no verb, flag, scope or catalog row. Credential handling,
  sandboxing and the permission model are untouched.
- **Docs one-liners** recommend user-writable paths only (`~/.zfunc`,
  `~/.config/fish/completions`, eval in rc files) — no `sudo`-into-system-dir
  instructions.

## Open questions

- Should `thegn setup` (the onboarding wizard) offer to wire completions for
  tarball users? Nice DX, but it writes to shell rc files — a consent-heavy
  side effect deliberately left out of this change.
