{
  lib,
  rustPlatform,
  makeWrapper,
  installShellFiles,
  # runtime tools superzej shells out to
  git,
  zellij,
  fzf,
  gum,
  lazygit,
  yazi,
  delta,
  coreutils,
}: let
  runtimeDeps = [git zellij fzf gum lazygit yazi delta coreutils];
in
  rustPlatform.buildRustPackage {
    pname = "superzej";
    version = "0.1.0";

    src = lib.cleanSourceWith {
      src = ../.;
      # Drop build artifacts so the store path is stable across rebuilds.
      filter = path: _type: let
        rel = lib.removePrefix (toString ../. + "/") (toString path);
      in
        !(lib.hasPrefix "target" rel
          || lib.hasPrefix "result" rel
          || lib.hasPrefix ".direnv" rel
          || lib.hasPrefix ".git/" rel);
    };

    cargoLock.lockFile = ../Cargo.lock;

    nativeBuildInputs = [makeWrapper installShellFiles];

    # rusqlite is vendored with the `bundled` feature → no system sqlite needed.

    postInstall = ''
      # Short alias.
      ln -s superzej $out/bin/sj

      # Inject runtime tools onto PATH without polluting the user's shell.
      wrapProgram $out/bin/superzej \
        --prefix PATH : ${lib.makeBinPath runtimeDeps}

      # Ship the layouts (not keybinds.kdl — that's embedded in the binary and
      # merged into the config at launch, it is not a zellij layout).
      mkdir -p $out/share/zellij/layouts
      cp layouts/superzej.kdl layouts/workspace-tab.kdl $out/share/zellij/layouts/
    '';

    meta = {
      description = "Terminal-native git-worktree IDE on top of zellij";
      mainProgram = "superzej";
      license = lib.licenses.mit;
      platforms = lib.platforms.linux;
    };
  }
