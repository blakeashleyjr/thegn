# treefmt-nix configuration — one entrypoint (`nix fmt`) for all content.
{
  projectRootFile = "flake.nix";

  programs = {
    rustfmt.enable = true; # rust
    alejandra.enable = true; # nix
    shfmt.enable = true; # bash/sh
    taplo.enable = true; # toml
    yamlfmt.enable = true; # yaml
    prettier = {
      enable = true; # markdown only (yaml handled by yamlfmt)
      includes = ["*.md"];
    };
  };

  settings.global.excludes = [
    "*.kdl" # no kdl formatter; layouts are hand-maintained
    "*.lock"
    "LICENSE"
    "target/*"
    "result"
    ".direnv/*"
    ".git/*"
  ];
}
