{
  pkgs,
  thegn,
  alacrittyProfile,
  alacritty,
  firaCodeNerdFont,
}: let
  fontconfig = pkgs.makeFontsConf {
    fontDirectories = [firaCodeNerdFont];
  };
in
  (pkgs.writeShellApplication {
    name = "thegn-batteries";
    runtimeInputs = [pkgs.coreutils];
    text = ''
      config_root="''${XDG_CONFIG_HOME:-$HOME/.config}"
      config_file="$config_root/thegn/alacritty.toml"

      if [ ! -e "$config_file" ]; then
        install -Dm644 "${alacrittyProfile}" "$config_file"
      fi

      export THEGN_ALACRITTY_CONFIG="$config_file"
      export FONTCONFIG_FILE="${fontconfig}"

      exec "${alacritty}/bin/alacritty" \
        --config-file "$config_file" \
        -e "${thegn}/bin/thegn" "$@"
    '';
  }).overrideAttrs (old: {
    meta =
      (old.meta or {})
      // {
        description = "thegn in pinned Alacritty with FiraCode Nerd Font";
        mainProgram = "thegn-batteries";
      };
  })
