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
    confirm_delete = cfg.confirmDelete;
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
    theme.preset = cfg.themePreset;
    theme.focus_border = cfg.themeFocusBorder;
    theme.pane_padding = cfg.themePanePadding;
    theme.undercurl = cfg.themeUndercurl;
    theme.color = cfg.themeColor;
    theme.glyphs = cfg.themeGlyphs;
    theme.agent_glyphs = cfg.themeAgentGlyphs;
    theme.mascot = cfg.themeMascot;
    theme.mascot_motion = cfg.themeMascotMotion;
    ui.confirm_delete_project = cfg.uiConfirmDeleteProject;
    ui.full_mode_chip = cfg.uiFullModeChip;
    ui.dismiss_overlay_on_click_outside = cfg.uiDismissOverlayOnClickOutside;
    ui.sidebar_freeze_sort = cfg.uiSidebarFreezeSort;
    ui.sidebar_nav_skips_collapsed = cfg.uiSidebarNavSkipsCollapsed;
    ui.sidebar_dividers = cfg.uiSidebarDividers;
    ui.fullscreen_keep_masthead = cfg.uiFullscreenKeepMasthead;
    ui.fullscreen_keep_statusbar = cfg.uiFullscreenKeepStatusbar;
    ui.sidebar_show_status_icon = cfg.uiSidebarShowStatusIcon;
    ui.sidebar_show_diff_stat = cfg.uiSidebarShowDiffStat;
    ui.sidebar_show_ahead_behind = cfg.uiSidebarShowAheadBehind;
    ui.sidebar_show_pr_chip = cfg.uiSidebarShowPrChip;
    ui.sidebar_show_jj = cfg.uiSidebarShowJj;
    ui.sidebar_show_submodules = cfg.uiSidebarShowSubmodules;
    ui.sidebar_detail_branch = cfg.uiSidebarDetailBranch;
    ui.sidebar_detail_branch_stat = cfg.uiSidebarDetailBranchStat;
    ui.sidebar_detail_pr = cfg.uiSidebarDetailPr;
    ui.sidebar_project_sort = cfg.uiSidebarProjectSort;
    ui.sidebar_terminals_section = cfg.uiSidebarTerminalsSection;
    ui.sidebar_focus_detail = cfg.uiSidebarFocusDetail;
    ui.language = cfg.uiLanguage;
    ui.sidebar_icon_ahead = cfg.uiSidebarIconAhead;
    ui.sidebar_icon_behind = cfg.uiSidebarIconBehind;
    ui.sidebar_icon_status = cfg.uiSidebarIconStatus;
    search.max_results = cfg.searchMaxResults;
    search.respect_gitignore = cfg.searchRespectGitignore;
    search.include_hidden = cfg.searchIncludeHidden;
    search.structural = cfg.searchStructural;
    palette.content_max_results = cfg.paletteContentMaxResults;
    palette.file_max_results = cfg.paletteFileMaxResults;
    palette.symbol_max_results = cfg.paletteSymbolMaxResults;
    palette.content_search_hidden = cfg.paletteContentSearchHidden;
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

    confirmDelete = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Ask before destructive worktree actions (deleting a worktree from disk via the sidebar). Unset (null) = the binary's default.";
    };

    themeAccent = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      example = "#f083ba";
      description = "Focus accent color (#rrggbb) for every thegn surface. Unset (null) = the binary's default.";
    };

    themePreset = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      example = "storm";
      description = "Built-in palette preset name (for example, prism, storm or nord) or a local theme name from $XDG_CONFIG_HOME/thegn/themes. Unset (null) = the binary's default.";
    };

    themeFocusBorder = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      example = "#6ee7d8";
      description = "Frame color (#rrggbb) of the focused pane, tab and chrome edge. Unset (null) = the binary's default.";
    };

    themePanePadding = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.u16);
      default = null;
      description = "Horizontal cells between a pane's frame and its content, each side. Unset (null) = the binary's default.";
    };

    themeUndercurl = lib.mkOption {
      type = lib.types.nullOr (lib.types.enum ["auto" "on" "off"]);
      default = null;
      description = "Curly-underline support mode. Unset (null) = the binary's default.";
    };

    themeColor = lib.mkOption {
      type = lib.types.nullOr (lib.types.enum ["auto" "truecolor" "256" "16" "none"]);
      default = null;
      description = "Color fidelity mode. Unset (null) = the binary's default.";
    };

    themeGlyphs = lib.mkOption {
      type = lib.types.nullOr (lib.types.enum ["auto" "unicode" "ascii"]);
      default = null;
      description = "Glyph fidelity mode. Unset (null) = the binary's default.";
    };

    themeAgentGlyphs = lib.mkOption {
      type = lib.types.nullOr (lib.types.enum ["letter" "symbol" "auto"]);
      default = null;
      description = "Sidebar agent marker style. Unset (null) = the binary's default.";
    };

    themeMascot = lib.mkOption {
      type = lib.types.nullOr (lib.types.enum ["owl" "knight" "off"]);
      default = null;
      description = "Loading-splash mascot. Unset (null) = the binary's default.";
    };

    themeMascotMotion = lib.mkOption {
      type = lib.types.nullOr (lib.types.enum ["blink" "still"]);
      default = null;
      description = "Splash-mascot motion. Unset (null) = the binary's default.";
    };

    uiConfirmDeleteProject = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Ask before destructive project worktree actions. Unset (null) = the binary's default.";
    };

    uiFullModeChip = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Whether to display the full word for the mode chip. Unset (null) = the binary's default.";
    };

    uiDismissOverlayOnClickOutside = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Dismiss a detail popup when the user left-clicks outside it, like Escape. Unset (null) = the binary's default.";
    };

    uiSidebarFreezeSort = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Hold the sidebar's worktree order steady while you are navigating it. Unset (null) = the binary's default.";
    };

    uiSidebarNavSkipsCollapsed = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Shift+Alt+↑/↓ steps past workspaces and terminal hosts you have collapsed. Unset (null) = the binary's default.";
    };

    uiSidebarDividers = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Separate one workspace's block from the next in the sidebar. Unset (null) = the binary's default.";
    };

    uiFullscreenKeepMasthead = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "In full-window pane fullscreen, keep the top masthead bar visible. Unset (null) = the binary's default.";
    };

    uiFullscreenKeepStatusbar = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "In full-window pane fullscreen, keep the bottom status bar visible. Unset (null) = the binary's default.";
    };

    uiSidebarShowStatusIcon = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Show the dirty status icon in a worktree row's right cluster. Unset (null) = the binary's default.";
    };

    uiSidebarShowDiffStat = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Show the uncommitted working-tree line stat in a worktree row's right cluster. Unset (null) = the binary's default.";
    };

    uiSidebarShowAheadBehind = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Show the upstream ahead and behind counts in a worktree row. Unset (null) = the binary's default.";
    };

    uiSidebarShowPrChip = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Show the compact open-PR chip in a worktree row's right cluster. Unset (null) = the binary's default.";
    };

    uiSidebarShowJj = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Show the jujutsu-colocation marker when the repo has a .jj/ beside .git/. Unset (null) = the binary's default.";
    };

    uiSidebarShowSubmodules = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Show a separate indicator when a submodule pointer or checkout is dirty. Unset (null) = the binary's default.";
    };

    uiSidebarDetailBranch = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Lead the focused detail line with the branch name. Unset (null) = the binary's default.";
    };

    uiSidebarDetailBranchStat = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Show the total branch-vs-default-branch line stat on the focused detail line. Unset (null) = the binary's default.";
    };

    uiSidebarDetailPr = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Show the open PR number on the focused detail line. Unset (null) = the binary's default.";
    };

    uiSidebarProjectSort = lib.mkOption {
      type = lib.types.nullOr (lib.types.enum ["manual" "attention"]);
      default = null;
      description = "Sidebar project ordering: keep the manual order, or bubble the most-urgent project to the top. Unset (null) = the binary's default.";
    };

    uiSidebarTerminalsSection = lib.mkOption {
      type = lib.types.nullOr (lib.types.enum ["always" "nonempty"]);
      default = null;
      description = "Sidebar TERMINALS section visibility. Unset (null) = the binary's default.";
    };

    uiSidebarFocusDetail = lib.mkOption {
      type = lib.types.nullOr (lib.types.enum ["all" "cursor" "off"]);
      default = null;
      description = "Which rows expand to a detail line while the sidebar is focused. Unset (null) = the binary's default.";
    };

    uiLanguage = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      example = "auto";
      description = "Language code, or auto to detect from system. Unset (null) = the binary's default.";
    };

    uiSidebarIconAhead = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      description = "Override glyph for the ahead marker; empty uses the built-in glyph. Unset (null) = the binary's default.";
    };

    uiSidebarIconBehind = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      description = "Override glyph for the behind marker; empty uses the built-in glyph. Unset (null) = the binary's default.";
    };

    uiSidebarIconStatus = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      description = "Override glyph for the dirty status marker; empty uses the built-in glyph. Unset (null) = the binary's default.";
    };

    searchMaxResults = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.unsigned);
      default = null;
      description = "Maximum results per search (pane-history list and workspace search stream). Unset (null) = the binary's default.";
    };

    searchRespectGitignore = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Whether the workspace search walker honors .gitignore. Unset (null) = the binary's default.";
    };

    searchIncludeHidden = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Whether the workspace search walker descends hidden files and directories. Unset (null) = the binary's default.";
    };

    searchStructural = lib.mkOption {
      type = lib.types.nullOr (lib.types.enum ["ast-grep" "none"]);
      default = null;
      description = "Structural (AST) search tier: ast-grep, or none to disable it. Unset (null) = the binary's default.";
    };

    paletteContentMaxResults = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.unsigned);
      default = null;
      description = "Maximum content-search matches in the Search Everywhere palette. Unset (null) = the binary's default.";
    };

    paletteFileMaxResults = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.unsigned);
      default = null;
      description = "Maximum file-path results after fuzzy ranking. Unset (null) = the binary's default.";
    };

    paletteSymbolMaxResults = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.unsigned);
      default = null;
      description = "Maximum symbol matches returned. Unset (null) = the binary's default.";
    };

    paletteContentSearchHidden = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Include hidden and ignored files in the file index. Unset (null) = the binary's default.";
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
