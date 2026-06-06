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

      # Expose the pinned zellij under a superzej-private name, and point the
      # binary at it. superzej drives THIS zellij (its own version, socket and
      # cache namespace) — never the user's system `zellij`.
      ln -s ${zellij}/bin/zellij $out/bin/superzej-zellij

      # Inject runtime tools onto PATH (so the plugins' `zellij run` resolves to
      # the pinned build too) and pin the binary superzej drives.
      wrapProgram $out/bin/superzej \
        --set SUPERZEJ_ZELLIJ_BIN ${zellij}/bin/zellij \
        --prefix PATH : ${lib.makeBinPath runtimeDeps}

      # Layouts and the zellij config (config/zellij.kdl) are both embedded in the
      # binary and seeded into superzej's private ~/.superzej/{layouts,zellij.kdl}
      # at launch — nothing zellij-related is shipped into the user's config tree.
    '';

    meta = {
      description = "Terminal-native git-worktree IDE on top of zellij";
      mainProgram = "superzej";
      license = lib.licenses.mit;
      platforms = lib.platforms.linux;
    };
  }
