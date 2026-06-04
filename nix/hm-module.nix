# Home-manager module for superzej.
#
# `self` is the flake, so the default package resolves without an overlay.
# Imported as:  imports = [ inputs.superzej.homeManagerModules.default ];
self: {
  config,
  pkgs,
  lib,
  ...
}: let
  cfg = config.programs.superzej;

  agentSubmodule = lib.types.submodule {
    options = {
      name = lib.mkOption {
        type = lib.types.str;
        description = "Display name shown in the picker.";
      };
      command = lib.mkOption {
        type = lib.types.str;
        description = "Command to run in the worktree (e.g. \"claude\" or \"aider --model sonnet\"). Use \"__shell__\" for a plain login shell.";
      };
    };
  };

  tomlFormat = pkgs.formats.toml {};

  # Rendered to ~/.config/superzej/config.toml; keys match the Rust serde struct.
  configFile = tomlFormat.generate "superzej-config.toml" {
    worktrees_dir = cfg.worktreesDir;
    workspaces_dir = cfg.workspacesDir;
    repo_roots = cfg.repoRoots;
    repo_scan_depth = cfg.repoScanDepth;
    base_branch = cfg.baseBranch;
    branch_prefix = cfg.branchPrefix;
    picker = cfg.picker;
    worktree_mode = cfg.worktreeMode;
    name_scheme = cfg.nameScheme;
    auto_remove_worktree = cfg.autoRemoveWorktree;
    agents = map (a: {inherit (a) name command;}) cfg.agents;
    tools = map (t: {inherit (t) name command;}) cfg.tools;
  };
in {
  options.programs.superzej = {
    enable = lib.mkEnableOption "superzej terminal-native worktree IDE";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.system}.default;
      defaultText = lib.literalExpression "superzej.packages.\${system}.default";
      description = "The superzej package to use.";
    };

    worktreesDir = lib.mkOption {
      type = lib.types.str;
      default = "${config.home.homeDirectory}/.superzej/worktrees";
      description = "Base directory for git worktrees (grouped per repo).";
    };

    workspacesDir = lib.mkOption {
      type = lib.types.str;
      default = "${config.home.homeDirectory}/code";
      description = "Where remote URLs are cloned by `new-workspace`.";
    };

    repoRoots = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [cfg.workspacesDir];
      defaultText = lib.literalExpression "[ config.programs.superzej.workspacesDir ]";
      example = lib.literalExpression ''[ "/home/you/code" "/home/you/src" ]'';
      description = "Directories scanned by the workspace repo picker.";
    };

    repoScanDepth = lib.mkOption {
      type = lib.types.int;
      default = 5;
      description = "Max directory depth when scanning repoRoots for git repos.";
    };

    baseBranch = lib.mkOption {
      type = lib.types.str;
      default = "auto";
      description = "Base ref for new worktrees. \"auto\" = current branch, else the repo default.";
    };

    branchPrefix = lib.mkOption {
      type = lib.types.str;
      default = "sz/";
      description = "Prefix for generated branch names.";
    };

    picker = lib.mkOption {
      type = lib.types.enum ["auto" "gum" "fzf" "select"];
      default = "auto";
      description = "TUI used for the agent/tool picker.";
    };

    worktreeMode = lib.mkOption {
      type = lib.types.enum ["global" "in_repo"];
      default = "global";
      description = "Where worktrees live: a global dir, or <repo>/.worktrees.";
    };

    nameScheme = lib.mkOption {
      type = lib.types.enum ["words" "numbered"];
      default = "words";
      description = "Auto branch naming: readable words (sz/brisk-otter) or numbered (sz/pane-1).";
    };

    autoRemoveWorktree = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Remove the worktree automatically when its pane is closed.";
    };

    agents = lib.mkOption {
      type = lib.types.listOf agentSubmodule;
      default = [
        {
          name = "claude";
          command = "claude";
        }
        {
          name = "shell";
          command = "__shell__";
        }
      ];
      example = lib.literalExpression ''
        [
          { name = "claude"; command = "claude"; }
          { name = "aider";  command = "aider --model sonnet"; }
          { name = "shell";  command = "__shell__"; }
        ]
      '';
      description = "Coding agents offered in the new-worktree picker.";
    };

    tools = lib.mkOption {
      type = lib.types.listOf agentSubmodule;
      default = [
        {
          name = "lazygit";
          command = "lazygit";
        }
        {
          name = "yazi";
          command = "yazi";
        }
        {
          name = "editor";
          command = "\${EDITOR:-vi} .";
        }
        {
          name = "diff";
          command = "git diff";
        }
      ];
      description = "Per-worktree tools (also bound to Alt-g/y/e//).";
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [cfg.package];

    # superzej reads this TOML config.
    xdg.configFile."superzej/config.toml".source = configFile;

    # Ship layouts into the *writable* zellij layouts dir — never touches the
    # user's read-only config.kdl. zellij resolves these by name.
    xdg.configFile."zellij/layouts/superzej.kdl".source = "${cfg.package}/share/zellij/layouts/superzej.kdl";
    xdg.configFile."zellij/layouts/worktree-tab.kdl".source = "${cfg.package}/share/zellij/layouts/worktree-tab.kdl";
    xdg.configFile."zellij/layouts/home-tab.kdl".source = "${cfg.package}/share/zellij/layouts/home-tab.kdl";
    xdg.configFile."zellij/layouts/worktree-tab-extra.kdl".source = "${cfg.package}/share/zellij/layouts/worktree-tab-extra.kdl";

    # Deploy the WASM plugins to the literal paths the session layout references
    # (file:~/.local/share/superzej/{sidebar,panel,tabbar,statusbar}.wasm). home.file
    # is relative to $HOME, so this matches regardless of $XDG_DATA_HOME.
    home.file.".local/share/superzej/sidebar.wasm".source = "${self.packages.${pkgs.system}.superzej-sidebar}/share/superzej/sidebar.wasm";
    home.file.".local/share/superzej/panel.wasm".source = "${self.packages.${pkgs.system}.superzej-panel}/share/superzej/panel.wasm";
    home.file.".local/share/superzej/tabbar.wasm".source = "${self.packages.${pkgs.system}.superzej-tabbar}/share/superzej/tabbar.wasm";
    home.file.".local/share/superzej/statusbar.wasm".source = "${self.packages.${pkgs.system}.superzej-statusbar}/share/superzej/statusbar.wasm";

    # Pre-grant the plugins' zellij permissions so the first session never shows
    # the "Allow? (y/n)" prompt. Idempotent; merges into permissions.kdl.
    home.activation.superzejGrantPlugins = lib.hm.dag.entryAfter ["writeBoundary"] ''
      $DRY_RUN_CMD ${cfg.package}/bin/superzej grant-plugins
    '';
  };
}
