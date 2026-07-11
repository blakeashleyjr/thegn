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
    pname = "thegn";
    version = "0.1.0";

    inherit src;

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
      # The native host (`thegn`) is the user-facing program, with a short
      # `tg` alias.
      ln -s thegn $out/bin/tg

      # Expose the pinned yazi under a thegn-private name for the file drawer.
      ln -s ${yazi}/bin/yazi $out/bin/thegn-yazi

      # Wrap the binary so it finds the pinned yazi + the tools it shells out to
      # (git/lazygit/delta/gh) regardless of the user's PATH.
      wrapProgram $out/bin/thegn \
        --set THEGN_YAZI_BIN ${yazi}/bin/yazi \
        --prefix PATH : ${lib.makeBinPath runtimeDeps}
    '';

    meta = {
      description = "Terminal-native git-worktree IDE";
      mainProgram = "thegn";
      license = lib.licenses.mit;
      platforms = lib.platforms.linux ++ lib.platforms.darwin;
    };
  }
