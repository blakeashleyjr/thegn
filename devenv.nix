{pkgs, ...}: {
  # Developer environment for superzej. `devenv shell` to enter, `devenv test`
  # to run the git-hooks + smoke test.

  packages = with pkgs; [
    # task runner
    just
    # linters / formatters used outside the language toolchain
    shellcheck
    shfmt
    yamllint
    taplo
    alejandra
    # runtime tools superzej shells out to
    git
    zellij
    fzf
    gum
    lazygit
    yazi
    delta
  ];

  # Use the nixpkgs toolchain (no channel/rust-overlay) so rustfmt/clippy match
  # the flake's treefmt + checks exactly — avoids formatter version skew.
  languages.rust.enable = true;

  # Comprehensive pre-commit hooks (rust + bash + yaml + toml + nix).
  git-hooks.hooks = {
    # rust
    rustfmt.enable = true;
    clippy.enable = true;
    # bash
    shellcheck.enable = true;
    shfmt.enable = true;
    # yaml
    yamllint.enable = true;
    # toml
    taplo.enable = true;
    # nix
    alejandra.enable = true;
  };

  enterShell = ''
    echo "superzej devenv — cargo build | just smoke | nix fmt"
  '';

  # `devenv test` runs the hooks, then this.
  enterTest = ''
    cargo build
    ./test/smoke.sh target/debug/superzej
  '';
}
