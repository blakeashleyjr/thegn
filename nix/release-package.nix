{
  lib,
  stdenv,
  autoPatchelfHook,
  makeWrapper,
  installShellFiles,
  makeDesktopItem,
  zlib,
  git,
  fzf,
  gum,
  lazygit,
  yazi,
  delta,
  gh,
  gdu,
  coreutils,
  bubblewrap,
  passt,
  yaziDeps ? [],
  archive,
  src,
  version,
}: let
  runtimeDeps = [git fzf gum lazygit yazi delta gh gdu coreutils] ++ yaziDeps;
  sandboxDeps = lib.optionals stdenv.hostPlatform.isLinux [bubblewrap passt];
  sandboxArgs =
    lib.optionalString (sandboxDeps != [])
    " --suffix PATH : ${lib.makeBinPath sandboxDeps}";
  desktopItem = makeDesktopItem {
    name = "thegn";
    desktopName = "thegn";
    genericName = "Git Worktree IDE";
    comment = "Terminal-native git-worktree IDE";
    exec = "thegn-standalone";
    icon = "thegn";
    terminal = false;
    categories = ["Development"];
    startupWMClass = "thegn";
  };
in
  stdenv.mkDerivation {
    pname = "thegn";
    inherit version;
    src = archive;

    dontConfigure = true;
    dontBuild = true;
    dontAutoPatchelf = true;

    nativeBuildInputs = [autoPatchelfHook makeWrapper installShellFiles];
    buildInputs = [stdenv.cc.cc.lib zlib];

    installPhase = ''
      if [ ! -f thegn ] || [ ! -x thegn ]; then
        echo "thegn release archive must contain an executable thegn at its root" >&2
        exit 1
      fi

      install -Dm755 thegn $out/bin/thegn
      autoPatchelf $out/bin/thegn

      ln -s thegn $out/bin/tg
      install -Dm644 ${src}/LICENSE-MIT $out/share/licenses/thegn/LICENSE-MIT
      install -Dm644 ${src}/LICENSE-APACHE $out/share/licenses/thegn/LICENSE-APACHE
      ln -s ${yazi}/bin/yazi $out/bin/thegn-yazi

      export HOME="$TMPDIR"
      export XDG_STATE_HOME="$TMPDIR/state"
      export XDG_CONFIG_HOME="$TMPDIR/config"
      export THEGN_NO_MIGRATE=1
      unset THEGN_LOG

      for name in thegn tg; do
        for sh in bash zsh fish; do
          "$out/bin/$name" completions "$sh" > "$TMPDIR/$name.$sh"
        done
        installShellCompletion --cmd "$name" \
          --bash "$TMPDIR/$name.bash" \
          --zsh "$TMPDIR/$name.zsh" \
          --fish "$TMPDIR/$name.fish"
      done

      install -Dm644 ${src}/config/alacritty.toml $out/share/thegn/alacritty.toml
      install -Dm644 ${src}/config/thegn.svg $out/share/icons/hicolor/scalable/apps/thegn.svg
      substitute ${./thegn-standalone.sh} $out/bin/thegn-standalone --subst-var-by out $out --subst-var-by bin thegn
      chmod 755 $out/bin/thegn-standalone
      install -Dm644 ${desktopItem}/share/applications/thegn.desktop -t $out/share/applications

      wrapProgram $out/bin/thegn \
        --set THEGN_YAZI_BIN ${yazi}/bin/yazi \
        --set-default THEGN_ALACRITTY_CONFIG $out/share/thegn/alacritty.toml \
        --prefix PATH : ${lib.makeBinPath runtimeDeps}${sandboxArgs}
    '';

    doInstallCheck = true;
    installCheckPhase = ''
      $out/bin/thegn --version | grep -F -- "${version}"
    '';

    meta = {
      mainProgram = "thegn";
      platforms = ["x86_64-linux"];
      license = with lib.licenses; [mit asl20];
      description = "thegn (published release binary)";
    };
  }
