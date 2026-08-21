# nix-darwin module for thegn.
#
# `self` is the flake, so the default package resolves without an overlay.
# Imported as:  imports = [ inputs.thegn.darwinModules.default ];
#
# Deliberately thin. nix-darwin manages the SYSTEM, and thegn's configuration is
# per-user (`~/.config/thegn/config.toml`) — nix-darwin has no equivalent of
# home-manager's `xdg.configFile`, and `environment.etc` is the wrong place for
# it. So this module only puts the binary on PATH; to declare configuration too,
# compose it with the home-manager module, which nix-darwin users already have:
#
#   darwinConfigurations."mac" = darwin.lib.darwinSystem {
#     system = "aarch64-darwin";
#     modules = [
#       inputs.thegn.darwinModules.default
#       { programs.thegn.enable = true; }
#       home-manager.darwinModules.home-manager
#       {
#         home-manager.users.you = {
#           imports = [ inputs.thegn.homeManagerModules.default ];
#           programs.thegn = { enable = true; themeAccent = "#f083ba"; };
#         };
#       }
#     ];
#   };
#
# Enabling both is fine and is the intended shape: the home-manager module's
# `home.packages` and this module's `environment.systemPackages` install the same
# store path, so there is nothing to collide.
self: {
  config,
  pkgs,
  lib,
  ...
}: let
  cfg = config.programs.thegn;
in {
  options.programs.thegn = {
    enable = lib.mkEnableOption "thegn terminal-native worktree IDE";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "thegn.packages.\${system}.default";
      description = ''
        The thegn package to use. On darwin this is the plain host binary; the
        adjacent static-musl bridge is an x86_64-linux-only artifact.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [cfg.package];
  };
}
