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
  gh,
  coreutils,
}: let
  runtimeDeps = [git zellij fzf gum lazygit yazi delta gh coreutils];
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

      # Ship the layouts. The zellij config (config/zellij.kdl) is embedded in
      # the binary and seeded to ~/.superzej/zellij.kdl at launch, not shipped here.
      mkdir -p $out/share/zellij/layouts
      cp layouts/superzej.kdl layouts/worktree-tab.kdl layouts/worktree-tab-extra.kdl layouts/home-tab.kdl $out/share/zellij/layouts/
    '';

    meta = {
      description = "Terminal-native git-worktree IDE on top of zellij";
      mainProgram = "superzej";
      license = lib.licenses.mit;
      platforms = lib.platforms.linux;
    };
  }
