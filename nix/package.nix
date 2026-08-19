{
  lib,
  rustPlatform,
  makeWrapper,
  installShellFiles,
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
  rustPlatform.buildRustPackage {
    pname = binName;
    version = "0.1.0";

    inherit src;

    # The `dev` Cargo feature (host crate) flips the default channel to dev.
    buildFeatures = lib.optionals isDev ["dev"];

    # Build ONLY the user-facing binary. Without this cargo builds every bin in
    # the workspace, which both wastes time and shipped `fake_lsp` — a test
    # fixture for the LSP client, reached in tests via `CARGO_BIN_EXE_fake_lsp`
    # — straight onto the PATH of anyone who ran `nix profile install`. Matches
    # what release.yml builds (`--bin thegn`).
    cargoBuildFlags = ["-p" "thegn-host" "--bin" "thegn"];

    cargoLock.lockFile = ../Cargo.lock;

    nativeBuildInputs = [makeWrapper installShellFiles pkg-config];
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
