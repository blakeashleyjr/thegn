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
  # yazi's preview/runtime tools (passed pinned from the flake); injected onto
  # PATH so previews work inside the file-manager drawer.
  yaziDeps ? [],
}: let
  runtimeDeps = [git zellij fzf gum lazygit yazi delta gh coreutils] ++ yaziDeps;
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
      # The native host is the installed user-facing program during the native
      # rebuild. Keep the transitional zellij-driven CLI available explicitly.
      mv $out/bin/superzej $out/bin/superzej-cli
      mv $out/bin/szhost $out/bin/superzej
      ln -s superzej $out/bin/sj
      ln -s superzej $out/bin/szhost

      # Expose the pinned zellij under a superzej-private name for the legacy CLI.
      ln -s ${zellij}/bin/zellij $out/bin/superzej-zellij

      # Same for yazi: expose the pinned build under a private name for the legacy
      # file drawer path.
      ln -s ${yazi}/bin/yazi $out/bin/superzej-yazi

      # The native host itself does not drive zellij/WASM plugins. Wrap the legacy
      # CLI so old subcommands still get the pinned toolchain when invoked as
      # `superzej-cli`.
      wrapProgram $out/bin/superzej-cli \
        --set SUPERZEJ_ZELLIJ_BIN ${zellij}/bin/zellij \
        --set SUPERZEJ_YAZI_BIN ${yazi}/bin/yazi \
        --prefix PATH : ${lib.makeBinPath runtimeDeps}

      # Layouts and the zellij config (config/zellij.kdl) are both embedded in the
      # legacy CLI and seeded into superzej's private ~/.superzej/{layouts,zellij.kdl}
      # at launch — nothing zellij-related is shipped into the user's config tree.
    '';

    meta = {
      description = "Terminal-native git-worktree IDE on top of zellij";
      mainProgram = "superzej";
      license = lib.licenses.mit;
      platforms = lib.platforms.linux;
    };
  }
