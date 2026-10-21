# Home-manager module for thegn.
#
# `self` is the flake, so the default package resolves without an overlay.
# Imported as:  imports = [ inputs.thegn.homeManagerModules.default ];
self: {
  config,
  pkgs,
  lib,
  ...
}: let
  cfg = config.programs.thegn;

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

  remoteSubmodule = lib.types.submodule {
    options = {
      host = lib.mkOption {
        type = lib.types.nullOr (lib.types.str);
        default = null;
        example = "user@devbox";
        description = "Remote ssh target to run worktrees on. Empty = local. Unset (null) = the binary's default.";
      };
      port = lib.mkOption {
        type = lib.types.nullOr (lib.types.port);
        default = null;
        description = "ssh port for the remote. Unset (null) = the binary's default.";
      };
      transport = lib.mkOption {
        type = lib.types.nullOr (lib.types.enum ["mosh" "ssh"]);
        default = null;
        description = "Interactive pane transport (control plane always uses ssh). Unset (null) = the binary's default.";
      };
      mode = lib.mkOption {
        type = lib.types.nullOr (lib.types.enum ["remote" "local_exec" "sshfs"]);
        default = null;
        description = "Where the remote worktree lives. Unset (null) = the binary's default.";
      };
      remoteDir = lib.mkOption {
        type = lib.types.nullOr (lib.types.str);
        default = null;
        description = "Where remote worktrees live (mode = remote). Unset (null) = the binary's default.";
      };
      forwardAgent = lib.mkOption {
        type = lib.types.nullOr (lib.types.bool);
        default = null;
        description = "ssh -A so remote git push reuses the host ssh-agent. Unset (null) = the binary's default.";
      };
    };
  };

  sandboxSubmodule = lib.types.submodule {
    options = {
      enabled = lib.mkOption {
        type = lib.types.nullOr (lib.types.bool);
        default = null;
        description = "Run each worktree's interactive process in a container/sandbox. Unset (null) = the binary's default.";
      };
      backend = lib.mkOption {
        type = lib.types.nullOr (lib.types.enum ["auto" "podman" "docker" "bwrap" "systemd" "apple" "wsl" "none"]);
        default = null;
        description = "Sandbox backend; \"auto\" walks backendChain. Unset (null) = the binary's default.";
      };
      backendChain = lib.mkOption {
        type = lib.types.nullOr (lib.types.listOf lib.types.str);
        # "apple" is macOS's `container` CLI and is probed only on macOS; the
        # win/linux-native entries are likewise inert off their own OS.
        default = null;
        description = "Auto-detection order; add \"host\" only together with [sandbox] isolation_floor = \"off\" in config.toml. Unset (null) = the binary's default.";
      };
      image = lib.mkOption {
        type = lib.types.nullOr (lib.types.str);
        default = null;
        example = "ghcr.io/you/devbox:latest";
        description = "OCI image. Empty = host-toolchain mode (bwrap/systemd). Unset (null) = the binary's default.";
      };
      network = lib.mkOption {
        type = lib.types.nullOr (lib.types.enum ["nat" "host" "none"]);
        default = null;
        description = "Sandbox network mode (agents need egress). Unset (null) = the binary's default.";
      };
      envPassthrough = lib.mkOption {
        type = lib.types.nullOr (lib.types.listOf lib.types.str);
        default = null;
        description = "Host env vars forwarded into the sandbox. Unset (null) = the binary's default.";
      };
      mounts = lib.mkOption {
        type = lib.types.nullOr (lib.types.listOf lib.types.str);
        default = null;
        description = "Extra bind mounts (\"host\", \"host:ro|rw|cache\", \"host:dest\", \"host:dest:ro|rw|cache\"). Unset (null) = the binary's default.";
      };
      initScript = lib.mkOption {
        type = lib.types.nullOr (lib.types.str);
        default = null;
        description = "Shell run inside the sandbox before the agent/shell. Unset (null) = the binary's default.";
      };
      devenv = lib.mkOption {
        type = lib.types.nullOr (lib.types.bool);
        default = null;
        description = "Wrap the inner command in `devenv shell --`. Unset (null) = the binary's default.";
      };
      onMissing = lib.mkOption {
        type = lib.types.nullOr (lib.types.enum ["warn" "prompt" "fail"]);
        default = null;
        description = "What to do when no backend is available. Unset (null) = the binary's default.";
      };
      remote = lib.mkOption {
        type = remoteSubmodule;
        default = {};
        description = "Run worktrees on a remote machine over mosh/ssh.";
      };
    };
  };

  logSubmodule = lib.types.submodule {
    options = {
      level = lib.mkOption {
        type = lib.types.nullOr (lib.types.enum ["error" "warn" "info" "debug" "trace"]);
        default = null;
        description = "Default log verbosity (THEGN_LOG can refine per-module). Unset (null) = the binary's default.";
      };
      file = lib.mkOption {
        type = lib.types.nullOr (lib.types.bool);
        default = null;
        description = "Mirror diagnostics to a rotating log file under `dir`. Unset (null) = the binary's default.";
      };
      dir = lib.mkOption {
        type = lib.types.nullOr (lib.types.str);
        default = null;
        description = "Log directory (empty = $XDG_STATE_HOME/thegn/logs). Unset (null) = the binary's default.";
      };
      rotationSizeMb = lib.mkOption {
        type = lib.types.nullOr (lib.types.ints.unsigned);
        default = null;
        description = "Rotate the active log once it exceeds this many MiB. Unset (null) = the binary's default.";
      };
      maxFiles = lib.mkOption {
        type = lib.types.nullOr (lib.types.ints.unsigned);
        default = null;
        description = "How many rotated log files to keep. Unset (null) = the binary's default.";
      };
      format = lib.mkOption {
        type = lib.types.nullOr (lib.types.enum ["text" "json"]);
        default = null;
        description = "Log file encoding. Unset (null) = the binary's default.";
      };
    };
  };

  actionSubmodule = lib.types.submodule {
    options = {
      name = lib.mkOption {
        type = lib.types.str;
        description = "Stable id + default menu/hint label.";
      };
      key = lib.mkOption {
        type = lib.types.str;
        example = "Alt D";
        description = "Key chord (e.g. \"Alt D\").";
      };
      run = lib.mkOption {
        type = lib.types.str;
        description = "Shell command run via `sh -c`.";
      };
      menu = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = "Surface this action in the Cmd+K palette.";
      };
      hint = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Short statusbar hint (defaults to name).";
      };
    };
  };

  tomlFormat = pkgs.formats.toml {};

  pruneNulls = lib.filterAttrsRecursive (_: v: v != null);
  pruneEmpty = value:
    if builtins.isAttrs value
    then let
      pruned = lib.mapAttrs (_: pruneEmpty) value;
    in
      lib.filterAttrs (_: v: v != {}) pruned
    else value;

  # Rendered to ~/.config/thegn/config.toml; keys match the Rust serde struct.
  configFile = tomlFormat.generate "thegn-config.toml" (pruneEmpty (pruneNulls {
    worktrees_dir = cfg.worktreesDir;
    projects_dir =
      if cfg.projectsDir != null
      then cfg.projectsDir
      else cfg.workspacesDir;
    repo_roots = cfg.repoRoots;
    repo_scan_depth = cfg.repoScanDepth;
    base_branch = cfg.baseBranch;
    branch_prefix = cfg.branchPrefix;
    picker = cfg.picker;
    worktree_mode = cfg.worktreeMode;
    name_scheme = cfg.nameScheme;
    auto_remove_worktree = cfg.autoRemoveWorktree;
    agents =
      if cfg.agents == null
      then null
      else map (a: {inherit (a) name command;}) cfg.agents;
    tools =
      if cfg.tools == null
      then null
      else map (t: {inherit (t) name command;}) cfg.tools;
    keybinds = cfg.keybinds;
    actions =
      if cfg.actions == null
      then null
      else map (a: lib.filterAttrs (_: v: v != null) {inherit (a) name key run menu hint;}) cfg.actions;
    theme.accent = cfg.themeAccent;
    monitor = {
      system = cfg.monitorSystem;
      gpu = cfg.monitorGpu;
    };
    pr.ttl_secs = cfg.prTtlSecs;
    watch.pr_interval_secs = cfg.watchPrIntervalSecs;
    log = {
      inherit (cfg.log) level file dir format;
      rotation_size_mb = cfg.log.rotationSizeMb;
      max_files = cfg.log.maxFiles;
    };
    sandbox = {
      inherit (cfg.sandbox) enabled backend image network mounts devenv;
      backend_chain = cfg.sandbox.backendChain;
      env_passthrough = cfg.sandbox.envPassthrough;
      init_script = cfg.sandbox.initScript;
      on_missing = cfg.sandbox.onMissing;
      remote = {
        inherit (cfg.sandbox.remote) host port transport mode;
        remote_dir = cfg.sandbox.remote.remoteDir;
        forward_agent = cfg.sandbox.remote.forwardAgent;
      };
    };
  }));
in {
  options.programs.thegn = {
    enable = lib.mkEnableOption "thegn terminal-native worktree IDE";

    package = lib.mkOption {
      type = lib.types.package;
      # `pkgs.system` is deprecated (renamed to stdenv.hostPlatform.system) and
      # warns on every evaluation. On darwin this resolves to the plain host
      # binary — the adjacent static-musl bridge is x86_64-linux-only.
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "thegn.packages.\${system}.default";
      description = "The thegn package to use.";
    };

    worktreesDir = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      description = "Base directory for git worktrees (grouped per repo). Unset (null) = the binary's default.";
    };

    projectsDir = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description =
        "Where project repositories are cloned."
        + " Canonical replacement for workspacesDir. Unset (null) = the binary's default.";
    };

    workspacesDir = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "Deprecated compatibility alias for projectsDir; removed after three stable releases. Unset (null) = the binary's default.";
    };

    repoRoots = lib.mkOption {
      type = lib.types.nullOr (lib.types.listOf lib.types.str);
      default = null;
      example = lib.literalExpression ''[ "/home/you/code" "/home/you/src" ]'';
      description = "Directories scanned by the project repo picker. Unset (null) = the binary's default.";
    };

    repoScanDepth = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.unsigned);
      default = null;
      description = "Max directory depth when scanning repoRoots for git repos. Unset (null) = the binary's default.";
    };

    baseBranch = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      description = "Base ref for new worktrees. \"auto\" = current branch, else the repo default. Unset (null) = the binary's default.";
    };

    branchPrefix = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      description = "Prefix for generated branch names. Unset (null) = the binary's default.";
    };

    picker = lib.mkOption {
      type = lib.types.nullOr (lib.types.enum ["auto" "gum" "fzf" "select"]);
      default = null;
      description = "TUI used for the agent/tool picker. Unset (null) = the binary's default.";
    };

    worktreeMode = lib.mkOption {
      type = lib.types.nullOr (lib.types.enum ["global" "in_repo"]);
      default = null;
      description = "Where worktrees live: a global dir, or <repo>/.worktrees. Unset (null) = the binary's default.";
    };

    nameScheme = lib.mkOption {
      type = lib.types.nullOr (lib.types.enum ["words" "numbered"]);
      default = null;
      description = "Auto branch naming: readable words (tg/brisk-otter) or numbered (tg/pane-1). Unset (null) = the binary's default.";
    };

    autoRemoveWorktree = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Remove the worktree automatically when its pane is closed. Unset (null) = the binary's default.";
    };

    themeAccent = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      example = "#f083ba";
      description = "Focus accent color (#rrggbb) for every thegn surface. Unset (null) = the binary's default.";
    };

    monitorSystem = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      example = "htop";
      description = ''
        CPU/RAM resource monitor opened from the top-bar stats widget
        (highlight CPU or MEM with Super+Alt+Up, then Enter). Default is `btm`
        (ClementTsang/bottom); run in an embedded tiled pane. Unset (null) = the binary's default.
      '';
    };

    monitorGpu = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      example = "nvtop";
      description = ''
        GPU resource monitor opened from the top-bar stats widget (highlight
        GPU with Super+Alt+Up, then Enter). Run in an embedded tiled pane. Unset (null) = the binary's default.
      '';
    };

    agents = lib.mkOption {
      type = lib.types.nullOr (lib.types.listOf agentSubmodule);
      default = null;
      example = lib.literalExpression ''
        [
          { name = "claude"; command = "claude"; }
          { name = "aider";  command = "aider --model sonnet"; }
          { name = "shell";  command = "__shell__"; }
        ]
      '';
      description = "Coding agents offered in the new-worktree picker. Unset (null) = the binary's default.";
    };

    tools = lib.mkOption {
      type = lib.types.nullOr (lib.types.listOf agentSubmodule);
      default = null;
      description = "Per-worktree tools (also bound to Alt-g/y/e//). Unset (null) = the binary's default.";
    };

    prTtlSecs = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.between 0 315360000);
      default = null;
      description = "PR-status cache TTL (seconds) before a live `gh` re-fetch. Unset (null) = the binary's default.";
    };

    watchPrIntervalSecs = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.between 0 2678400);
      default = null;
      description = "PR refresh interval (seconds) for the per-session watch daemon. Unset (null) = the binary's default.";
    };

    log = lib.mkOption {
      type = logSubmodule;
      default = {};
      description = "Logging: level, optional rotating file sink, format.";
    };

    keybinds = lib.mkOption {
      type = lib.types.nullOr (lib.types.attrsOf lib.types.str);
      default = null;
      example = lib.literalExpression ''{ new-worktree = "Ctrl w"; }'';
      description = "Rebind built-in actions by id (see `thegn keys list`). Unset (null) = the binary's default.";
    };

    actions = lib.mkOption {
      type = lib.types.nullOr (lib.types.listOf actionSubmodule);
      default = null;
      example = lib.literalExpression ''[ { name = "deploy"; key = "Alt D"; run = "just deploy"; menu = true; } ]'';
      description = "User-defined keybind actions (chord → shell command). Unset (null) = the binary's default.";
    };

    sandbox = lib.mkOption {
      type = sandboxSubmodule;
      default = {};
      example = lib.literalExpression ''
        {
          backend = "podman";
          image = "ghcr.io/you/devbox:latest";
          remote = { host = "user@devbox"; transport = "mosh"; };
        }
      '';
      description = "Per-worktree container/sandbox settings (see README \"Sandboxing worktrees\").";
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [cfg.package];

    # thegn reads this TOML config. The native host owns its own chrome
    # (sidebar/panel/tabbar/statusbar) in-process — there are no WASM plugins to
    # deploy and no permissions to pre-grant.
    xdg.configFile."thegn/config.toml".source = configFile;
  };
}
