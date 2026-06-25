{
  lib,
  rustPlatform,
  makeWrapper,
  installShellFiles,
  # runtime tools superzej shells out to
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
  src ?
    lib.cleanSourceWith {
      src = ../.;
      # Drop build artifacts so the store path is stable across rebuilds.
      filter = path: _type: let
        rel = lib.removePrefix (toString ../. + "/") (toString path);
      in
        !(lib.hasPrefix "target" rel
          || lib.hasPrefix "result" rel
          || lib.hasPrefix ".direnv" rel
          || lib.hasPrefix ".git/" rel);
    },
}: let
  runtimeDeps = [git fzf gum lazygit yazi delta gh coreutils] ++ yaziDeps;
in
  rustPlatform.buildRustPackage {
    pname = "superzej";
    version = "0.1.0";

    inherit src;

    cargoLock.lockFile = ../Cargo.lock;

    nativeBuildInputs = [makeWrapper installShellFiles];

    # rusqlite is vendored with the `bundled` feature → no system sqlite needed.

    # The host's PTY/pane tests spawn a real `/bin/sh` on a pseudo-terminal,
    # which the hermetic Nix sandbox has neither — they pass under `just test`
    # (and `just ci` gates on test + coverage + smoke before this build). So the
    # package build itself just compiles + installs.
    doCheck = false;

    postInstall = ''
      # The native host (`szhost`) is the user-facing program. Install it as
      # `superzej`, with `sj`/`szhost` aliases.
      mv $out/bin/szhost $out/bin/superzej
      ln -s superzej $out/bin/sj
      ln -s superzej $out/bin/szhost

      # Expose the pinned yazi under a superzej-private name for the file drawer.
      ln -s ${yazi}/bin/yazi $out/bin/superzej-yazi

      # Wrap the binary so it finds the pinned yazi + the tools it shells out to
      # (git/lazygit/delta/gh) regardless of the user's PATH.
      wrapProgram $out/bin/superzej \
        --set SUPERZEJ_YAZI_BIN ${yazi}/bin/yazi \
        --prefix PATH : ${lib.makeBinPath runtimeDeps}
    '';

    meta = {
      description = "Terminal-native git-worktree IDE";
      mainProgram = "superzej";
      license = lib.licenses.mit;
      platforms = lib.platforms.linux;
    };
  }
