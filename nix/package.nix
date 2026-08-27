{
  lib,
  # crane, not rustPlatform: it compiles the dependency tree into its OWN
  # derivation (`cargoArtifacts`), so editing our crates reuses those ~600
  # already-built dependencies. buildRustPackage puts deps and workspace in one
  # derivation, so any source change rebuilt everything — the difference between
  # a ten-minute CI job and an hour one.
  craneLib,
  # Prebuilt dependency artifacts. Passed in (rather than built here) so the
  # stable and dev channels share one dependency build; `dev` is an empty
  # feature that pulls in no extra crates, so the artifacts are identical.
  cargoArtifacts,
  makeWrapper,
  # `installShellCompletion` (a setup hook) — used in postInstall to drop the
  # generated bash/zsh/fish scripts into the XDG dirs every shell already
  # searches. `stdenv` is only read for the cross-compilation guard.
  installShellFiles,
  stdenv,
  # Native build inputs for fff-search's vendored C deps: `pkg-config` + `zlib`
  # are needed by libgit2-sys → libz-sys (git2 `vendored-libgit2`); lmdb-master-sys
  # builds its C via the `cc` crate (stdenv compiler, no extra input).
  pkg-config,
  zlib,
  # runtime tools thegn shells out to
  git,
  fzf,
  gum,
  lazygit,
  yazi,
  delta,
  gh,
  coreutils,
  # yazi's preview/runtime tools (passed pinned from the flake); injected onto
  # PATH so previews work inside the file-manager drawer.
  yaziDeps ? [],
  # Release channel: "stable" (regular pre-alpha) or "dev". The dev build only
  # flips the compiled-in default channel via the host's `dev` Cargo feature —
  # no extra code — so experimental subsystems (remotes, observe,
  # placement, non-GitHub trackers) are honoured instead of clamped off. It
  # installs as `thegn-dev`/`tg-dev` so it can sit beside a stable install.
  # `THEGN_CHANNEL` overrides the default at runtime for either binary.
  channel ? "stable",
  # Defaults to the same allowlisted source the flake passes in, so a bare
  # `callPackage ./nix/package.nix {}` cannot drift from `nix build`.
  src ? import ./source.nix {inherit lib;} ../.,
}: let
  runtimeDeps = [git fzf gum lazygit yazi delta gh coreutils] ++ yaziDeps;
  isDev = channel == "dev";
  # The dev build coexists with a stable install under distinct names.
  binName =
    if isDev
    then "thegn-dev"
    else "thegn";
  aliasName =
    if isDev
    then "tg-dev"
    else "tg";
in
  craneLib.buildPackage {
    pname = binName;
    version = "0.1.0";

    inherit src cargoArtifacts;

    # Build ONLY the user-facing binary, with the `dev` Cargo feature flipping
    # the compiled-in default channel. Without the scoping cargo builds every
    # bin in the workspace, which both wastes time and shipped `fake_lsp` — an
    # LSP test fixture reached in tests via `CARGO_BIN_EXE_fake_lsp` — straight
    # onto the PATH of anyone who ran `nix profile install`. Matches what
    # release.yml builds.
    cargoExtraArgs =
      "-p thegn-host --bin thegn"
      + lib.optionalString isDev " --features dev";

    nativeBuildInputs = [makeWrapper pkg-config installShellFiles];
    buildInputs = [zlib];

    # rusqlite is vendored with the `bundled` feature → no system sqlite needed.
    # fff-search links vendored libgit2 + LMDB (built from source in-sandbox).

    # The host's PTY/pane tests spawn a real `/bin/sh` on a pseudo-terminal,
    # which the hermetic Nix sandbox has neither — they pass under `just test`
    # (and `just ci` gates on test + coverage + smoke before this build). So the
    # package build itself just compiles + installs.
    doCheck = false;

    postInstall = ''
      # cargo installs the binary as `thegn`; the dev channel renames it so a
      # dev build can sit beside a stable one (thegn-dev / tg-dev).
      ${lib.optionalString isDev "mv $out/bin/thegn $out/bin/${binName}"}

      # The native host is the user-facing program, with a short alias.
      ln -s ${binName} $out/bin/${aliasName}

      # Expose the pinned yazi under a thegn-private name for the file drawer.
      ln -s ${yazi}/bin/yazi $out/bin/thegn-yazi

      # Shell completions. The PACKAGER owns delivery: an install gives working
      # `${binName}` and `${aliasName}` completions in bash/zsh/fish with no
      # rc-file edit and no `eval "$(… completions zsh)"` at shell startup —
      # which would cost a config+DB-loading process spawn in every pane thegn
      # itself opens. Generated from the binary we just built, so the scripts
      # cannot drift from the CLI tree.
      #
      # Ordering is load-bearing, both ways:
      #   - AFTER the ${aliasName} symlink, because the generator names the
      #     script from argv[0]; running the binary through the symlink is the
      #     only supported way to ask for the alias' script.
      #   - BEFORE wrapProgram, because wrapping moves the real binary aside and
      #     replaces $out/bin/${binName} with a shell wrapper. Generating from
      #     the plain binary needs none of the wrapped PATH.
      #
      # bash/zsh/fish only: installShellCompletion has no elvish/PowerShell
      # destination and neither shell has a standard Nix install dir. Those two
      # stay a manual `${binName} completions <shell>` (see docs/cli.md).
      #
      # Skipped under cross-compilation — the just-built binary cannot run in
      # the sandbox, and the package must still build without completions.
      ${lib.optionalString (stdenv.buildPlatform.canExecute stdenv.hostPlatform) ''
        # `completions` dispatches through run_subcommand, which loads the
        # layered config and merges DB hosts — i.e. it OPENS THE SQLITE STATE DB
        # — before it reaches the generator. HOME is /homeless-shelter in the
        # sandbox, so point the whole XDG surface at scratch and skip the brand
        # migration pass.
        export HOME="$TMPDIR"
        export XDG_STATE_HOME="$TMPDIR/state"
        export XDG_CONFIG_HOME="$TMPDIR/config"
        export THEGN_NO_MIGRATE=1
        unset THEGN_LOG

        # Written to files rather than piped through `<(…)`: process
        # substitution hides the generator's exit status, which would install a
        # truncated script as a silent success.
        for name in ${binName} ${aliasName}; do
          for sh in bash zsh fish; do
            "$out/bin/$name" completions "$sh" > "$TMPDIR/$name.$sh"
          done
          installShellCompletion --cmd "$name" \
            --bash "$TMPDIR/$name.bash" \
            --zsh "$TMPDIR/$name.zsh" \
            --fish "$TMPDIR/$name.fish"
        done
      ''}

      # Wrap the binary so it finds the pinned yazi + the tools it shells out to
      # (git/lazygit/delta/gh) regardless of the user's PATH.
      wrapProgram $out/bin/${binName} \
        --set THEGN_YAZI_BIN ${yazi}/bin/yazi \
        --prefix PATH : ${lib.makeBinPath runtimeDeps}
    '';

    meta = {
      description =
        "Terminal-native git-worktree IDE"
        + lib.optionalString isDev " (dev channel: experimental features enabled)";
      mainProgram = binName;
      license = lib.licenses.mit;
      platforms = lib.platforms.linux ++ lib.platforms.darwin;
    };
  }
