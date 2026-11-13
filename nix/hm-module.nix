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
      stderrCapMb = lib.mkOption {
        type = lib.types.nullOr (lib.types.ints.unsigned);
        default = null;
        description = "Cap captured stderr retention across restarts, in MiB. Unset (null) = the binary's default.";
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
    session.scrollback_lines = cfg.sessionScrollbackLines;
    session.restore_grace_secs = cfg.sessionRestoreGraceSecs;
    session.focus_on_create = cfg.sessionFocusOnCreate;
    session.resident_pool_limit = cfg.sessionResidentPoolLimit;
    clipboard.image_paste = cfg.clipboardImagePaste;
    clipboard.max_image_bytes = cfg.clipboardMaxImageBytes;
    clipboard.remote_dir = cfg.clipboardRemoteDir;
    clipboard.keep_hours = cfg.clipboardKeepHours;
    loc.enabled = cfg.locEnabled;
    loc.scan_interval_secs = cfg.locScanIntervalSecs;
    loc.max_scan_per_round = cfg.locMaxScanPerRound;
    loc.watch_invalidate_secs = cfg.locWatchInvalidateSecs;
    replay.enabled = cfg.replayEnabled;
    replay.max_bytes_per_pane = cfg.replayMaxBytesPerPane;
    replay.max_duration_secs = cfg.replayMaxDurationSecs;
    replay.keyframe_interval_ms = cfg.replayKeyframeIntervalMs;
    replay.keyframe_interval_bytes = cfg.replayKeyframeIntervalBytes;
    replay.idle_threshold_ms = cfg.replayIdleThresholdMs;
    replay.persist = cfg.replayPersist;
    drawer.command = cfg.drawerCommand;
    drawer.config_home = cfg.drawerConfigHome;
    drawer.height = cfg.drawerHeight;
    drawer.width = cfg.drawerWidth;
    drawer.image_previews = cfg.drawerImagePreviews;
    drawer.git_status = cfg.drawerGitStatus;
    drawer.contain = cfg.drawerContain;
    drawer.memory_max = cfg.drawerMemoryMax;
    drawer.memory_swap_max = cfg.drawerMemorySwapMax;
    drawer.cpu_quota = cfg.drawerCpuQuota;
    drawer.pool_limit = cfg.drawerPoolLimit;
    drawer.prewarm = cfg.drawerPrewarm;
    strip.visible = cfg.stripVisible;
    git.backend = cfg.gitBackend;
    git.override_gpg = cfg.gitOverrideGpg;
    git.merge_guard = cfg.gitMergeGuard;
    git.auto_fetch = cfg.gitAutoFetch;
    git.auto_fetch_interval_secs = cfg.gitAutoFetchIntervalSecs;
    git.auto_fetch_min_interval_secs = cfg.gitAutoFetchMinIntervalSecs;
    git.auto_fetch_notify = cfg.gitAutoFetchNotify;
    git.structural_diff = cfg.gitStructuralDiff;
    git.auto_fetch_colocated = cfg.gitAutoFetchColocated;
    git.submodules = cfg.gitSubmodules;
    stats.cpu_icon = cfg.statsCpuIcon;
    stats.mem_icon = cfg.statsMemIcon;
    stats.net_icon = cfg.statsNetIcon;
    stats.gpu_icon = cfg.statsGpuIcon;
    stats.temp_icon = cfg.statsTempIcon;
    stats.swap_icon = cfg.statsSwapIcon;
    stats.freq_icon = cfg.statsFreqIcon;
    stats.load_icon = cfg.statsLoadIcon;
    stats.uptime_icon = cfg.statsUptimeIcon;
    stats.battery_icon = cfg.statsBatteryIcon;
    stats.battery_charging_icon = cfg.statsBatteryChargingIcon;
    stats.battery_warn = cfg.statsBatteryWarn;
    stats.disk_icon = cfg.statsDiskIcon;
    stats.disk_free_warn = cfg.statsDiskFreeWarn;
    stats.disk_free_critical = cfg.statsDiskFreeCritical;
    stats.disk_path = cfg.statsDiskPath;
    notifications.desktop = cfg.notificationsDesktop;
    notifications.surface_self_log_errors = cfg.notificationsSurfaceSelfLogErrors;
    notifications.github_mentions = cfg.notificationsGithubMentions;
    notifications.agent_attention_inbox = cfg.notificationsAgentAttentionInbox;
    notifications.merge_queue_hold_desktop = cfg.notificationsMergeQueueHoldDesktop;
    notifications.dnd.enabled = cfg.notificationsDndEnabled;
    notifications.sound.mute = cfg.notificationsSoundMute;
    notifications.sound.suppress_focused = cfg.notificationsSoundSuppressFocused;
    activity.enabled = cfg.activityEnabled;
    activity.red_requires_agent = cfg.activityRedRequiresAgent;
    activity.agent_programs = cfg.activityAgentPrograms;
    recording.dir = cfg.recordingDir;
    recording.max_bytes = cfg.recordingMaxBytes;
    media.enabled = cfg.mediaEnabled;
    media.default_action = cfg.mediaDefaultAction;
    media.show_art = cfg.mediaShowArt;
    media.overlay_on_badge_click = cfg.mediaOverlayOnBadgeClick;
    media.seek_step_secs = cfg.mediaSeekStepSecs;
    media.seek_step_video_secs = cfg.mediaSeekStepVideoSecs;
    media.poll_interval_secs = cfg.mediaPollIntervalSecs;
    media.players_priority = cfg.mediaPlayersPriority;
    monitor = {
      system = cfg.monitorSystem;
      gpu = cfg.monitorGpu;
      proc_rows = cfg.monitorProcRows;
      processes = cfg.monitorProcesses;
      process_io = cfg.monitorProcessIo;
    };
    pr.ttl_secs = cfg.prTtlSecs;
    watch.pr_interval_secs = cfg.watchPrIntervalSecs;
    limits.test_cpu_quota = cfg.limitsTestCpuQuota;
    limits.test_mem_max = cfg.limitsTestMemMax;
    limits.test_max_parallel = cfg.limitsTestMaxParallel;
    limits.test_timeout_secs = cfg.limitsTestTimeoutSecs;
    limits.discover_timeout_secs = cfg.limitsDiscoverTimeoutSecs;
    limits.isolated_target_dir = cfg.limitsIsolatedTargetDir;
    diagnostics.crash_reports = cfg.diagnosticsCrashReports;
    diagnostics.crash_retention = cfg.diagnosticsCrashRetention;
    diagnostics.ring_size = cfg.diagnosticsRingSize;
    disk.show_sizes = cfg.diskShowSizes;
    disk.warn_threshold_gb = cfg.diskWarnThresholdGb;
    disk.scan_interval_secs = cfg.diskScanIntervalSecs;
    disk.max_scan_per_round = cfg.diskMaxScanPerRound;
    disk.auto_clean_on_merge = cfg.diskAutoCleanOnMerge;
    disk.clean_on_pr_closed = cfg.diskCleanOnPrClosed;
    disk.idle_clean_days = cfg.diskIdleCleanDays;
    disk.generation_min_age_days = cfg.diskGenerationMinAgeDays;
    disk.reclaim_on_low_disk = cfg.diskReclaimOnLowDisk;
    disk.sccache = cfg.diskSccache;
    daemon.enabled = cfg.daemonEnabled;
    daemon.idle_exit_secs = cfg.daemonIdleExitSecs;
    daemon.lease_grace_secs = cfg.daemonLeaseGraceSecs;
    daemon.stream_gap_window_secs = cfg.daemonStreamGapWindowSecs;
    network.mode = cfg.networkMode;
    network.offline_after_failures = cfg.networkOfflineAfterFailures;
    network.recovery_probe_secs = cfg.networkRecoveryProbeSecs;
    preview.enabled = cfg.previewEnabled;
    preview.fetch_timeout_ms = cfg.previewFetchTimeoutMs;
    preview.max_body_bytes = cfg.previewMaxBodyBytes;
    usage.enabled = cfg.usageEnabled;
    usage.statusbar = cfg.usageStatusbar;
    usage.discover_profiles = cfg.usageDiscoverProfiles;
    usage.token_rollups = cfg.usageTokenRollups;
    usage.history_days = cfg.usageHistoryDays;
    usage.poll_interval_secs = cfg.usagePollIntervalSecs;
    lsp.enabled = cfg.lspEnabled;
    lsp.hover = cfg.lspHover;
    lsp.require_containment = cfg.lspRequireContainment;
    semantic.worktree_index = cfg.semanticWorktreeIndex;
    semantic.index_max_files = cfg.semanticIndexMaxFiles;
    semantic.map_budget_lines = cfg.semanticMapBudgetLines;
    host_discovery.tailnet.enabled = cfg.hostDiscoveryTailnetEnabled;
    host_discovery.tailnet.online_only = cfg.hostDiscoveryTailnetOnlineOnly;
    lifecycle.snapshot.server_side_encryption = cfg.lifecycleSnapshotServerSideEncryption;
    lifecycle.snapshot.conditional_create = cfg.lifecycleSnapshotConditionalCreate;
    log = {
      inherit (cfg.log) level file dir format;
      rotation_size_mb = cfg.log.rotationSizeMb;
      max_files = cfg.log.maxFiles;
      stderr_cap_mb = cfg.log.stderrCapMb;
    };
    sandbox = {
      inherit (cfg.sandbox) enabled backend image network mounts devenv;
      backend_chain = cfg.sandbox.backendChain;
      env_passthrough = cfg.sandbox.envPassthrough;
      init_script = cfg.sandbox.initScript;
      on_missing = cfg.sandbox.onMissing;
      limits = {
        heavy_slots = cfg.limitsHeavySlots;
        max_load = cfg.limitsMaxLoad;
      };
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

    sessionScrollbackLines = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.u32);
      default = null;
      description = "Maximum plain-text scrollback lines captured per pane on snapshot and repainted on restore; 0 disables capture. Unset (null) = the binary's default.";
    };

    sessionRestoreGraceSecs = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.between 0 315360000);
      default = null;
      description = "Restore-time grace (seconds) after which a persisted active or running activity dot is downgraded to a settled state. Unset (null) = the binary's default.";
    };

    sessionFocusOnCreate = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Whether creating a worktree jumps to its new tab instead of opening it in the background. Unset (null) = the binary's default.";
    };

    sessionResidentPoolLimit = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.unsigned);
      default = null;
      description = "Maximum parked workspaces kept resident for instant switch-back; 0 reaps panes on every switch. Unset (null) = the binary's default.";
    };

    clipboardImagePaste = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Master switch for clipboard image paste; text paste is unaffected. Unset (null) = the binary's default.";
    };

    clipboardMaxImageBytes = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.unsigned);
      default = null;
      description = "Refuse a clipboard image larger than this many bytes (0 restores the default). Unset (null) = the binary's default.";
    };

    clipboardRemoteDir = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      example = "~/.cache/thegn/paste";
      description = "Remote drop directory for pasted images (a leading ~ is expanded through the remote $HOME). Unset (null) = the binary's default.";
    };

    clipboardKeepHours = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.unsigned);
      default = null;
      description = "Age in hours past which a pasted-image drop file is swept on the next paste. Unset (null) = the binary's default.";
    };

    locEnabled = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Count lines of code at all; false hides the LOC chip, its detail table and the Files-footer count and runs no tokei walk. Unset (null) = the binary's default.";
    };

    locScanIntervalSecs = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.between 0 2678400);
      default = null;
      description = "Per-worktree re-count TTL in seconds; 0 counts every pump. Unset (null) = the binary's default.";
    };

    locMaxScanPerRound = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.u32);
      default = null;
      description = "Worktrees counted per background round; 0 = unlimited. Unset (null) = the binary's default.";
    };

    locWatchInvalidateSecs = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.between 0 315360000);
      default = null;
      description = "Shortest gap in seconds between content-driven recounts of the active worktree; 0 disables them. Unset (null) = the binary's default.";
    };

    replayEnabled = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Master switch for the per-pane replay ring; off allocates no ring. Unset (null) = the binary's default.";
    };

    replayMaxBytesPerPane = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.unsigned);
      default = null;
      description = "Per-pane byte budget for the replay ring; oldest events are evicted past it. Unset (null) = the binary's default.";
    };

    replayMaxDurationSecs = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.between 0 315360000);
      default = null;
      description = "Per-pane duration budget in seconds; older events are evicted. Unset (null) = the binary's default.";
    };

    replayKeyframeIntervalMs = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.between 0 315360000000);
      default = null;
      description = "Capture a keyframe marker after this many milliseconds of activity. Unset (null) = the binary's default.";
    };

    replayKeyframeIntervalBytes = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.unsigned);
      default = null;
      description = "Capture a keyframe marker after this many bytes, whichever comes first. Unset (null) = the binary's default.";
    };

    replayIdleThresholdMs = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.between 0 315360000000);
      default = null;
      description = "During playback a gap larger than this many milliseconds is collapsed to a short constant. Unset (null) = the binary's default.";
    };

    replayPersist = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Mirror each pane's replay ring to disk so scrubbing reaches into the previous run after a restart (the one replay feature with real disk cost). Unset (null) = the binary's default.";
    };

    drawerCommand = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      description = "File manager to run; empty runs the bundled pinned yazi; a non-empty value with no explicit kind selects the custom provider. Unset (null) = the binary's default.";
    };

    drawerConfigHome = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      description = "yazi config dir (YAZI_CONFIG_HOME); empty = private thegn config home, \"system\" = your own yazi config. Unset (null) = the binary's default.";
    };

    drawerHeight = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      example = "35%";
      description = "Drawer height as a percentage or a row count. Unset (null) = the binary's default.";
    };

    drawerWidth = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      example = "full";
      description = "Drawer width: full or center. Unset (null) = the binary's default.";
    };

    drawerImagePreviews = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Whether drawer image previews are enabled. Unset (null) = the binary's default.";
    };

    drawerGitStatus = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Whether the drawer shows git status. Unset (null) = the binary's default.";
    };

    drawerContain = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Whether drawer launches use a systemd scope. Unset (null) = the binary's default.";
    };

    drawerMemoryMax = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      description = "MemoryMax for the drawer scope; empty omits the property. Unset (null) = the binary's default.";
    };

    drawerMemorySwapMax = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      description = "MemorySwapMax for the drawer scope; empty omits the property. Unset (null) = the binary's default.";
    };

    drawerCpuQuota = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      description = "CPUQuota for the drawer scope. Unset (null) = the binary's default.";
    };

    drawerPoolLimit = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.unsigned);
      default = null;
      description = "Maximum hidden drawers kept alive; 0 disables pooling. Unset (null) = the binary's default.";
    };

    drawerPrewarm = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Whether the active worktree's drawer is prewarmed. Unset (null) = the binary's default.";
    };

    stripVisible = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Whether the pinned-program strip is shown when it has at least one pin. Unset (null) = the binary's default.";
    };

    gitBackend = lib.mkOption {
      type = lib.types.nullOr (lib.types.enum ["auto" "gix" "cli"]);
      default = null;
      description = "Git read engine: auto, gix, or cli. Unset (null) = the binary's default.";
    };

    gitOverrideGpg = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Pass `-c commit.gpgSign=false -c tag.gpgSign=false` to history-rewriting operations so a gpg prompt cannot hang a background op. Unset (null) = the binary's default.";
    };

    gitMergeGuard = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Install the pre-merge-commit hook that refuses a merge into the canonical checkout from inside a thegn sandbox. Unset (null) = the binary's default.";
    };

    gitAutoFetch = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Poll the remote in the background with `git fetch --prune` (never a merge). Unset (null) = the binary's default.";
    };

    gitAutoFetchIntervalSecs = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.between 0 2678400);
      default = null;
      description = "Background-fetch cadence in seconds; 0 disables the periodic poll. Unset (null) = the binary's default.";
    };

    gitAutoFetchMinIntervalSecs = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.between 0 2678400);
      default = null;
      description = "Floor between two background fetches of the same repo. Unset (null) = the binary's default.";
    };

    gitAutoFetchNotify = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Also raise an inbox notification when a background fetch finds new upstream commits. Unset (null) = the binary's default.";
    };

    gitStructuralDiff = lib.mkOption {
      type = lib.types.nullOr (lib.types.enum ["off" "auto" "difft"]);
      default = null;
      description = "Read-only diff renderer: off, auto, or difft. Unset (null) = the binary's default.";
    };

    gitAutoFetchColocated = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Also run background auto-fetch in repos colocated with jujutsu. Unset (null) = the binary's default.";
    };

    gitSubmodules = lib.mkOption {
      type = lib.types.nullOr (lib.types.enum ["auto" "off"]);
      default = null;
      description = "Submodule lifecycle mode: auto or off. Unset (null) = the binary's default.";
    };

    statsCpuIcon = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      description = "CPU masthead stats-widget icon; text or a symbol. Unset (null) = the binary's default.";
    };

    statsMemIcon = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      description = "Memory masthead stats-widget icon; text or a symbol. Unset (null) = the binary's default.";
    };

    statsNetIcon = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      description = "Network masthead stats-widget icon; text or a symbol. Unset (null) = the binary's default.";
    };

    statsGpuIcon = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      description = "GPU masthead stats-widget icon; text or a symbol. Unset (null) = the binary's default.";
    };

    statsTempIcon = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      description = "Temperature masthead stats-widget icon; text or a symbol. Unset (null) = the binary's default.";
    };

    statsSwapIcon = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      description = "Swap masthead stats-widget icon; text or a symbol. Unset (null) = the binary's default.";
    };

    statsFreqIcon = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      description = "Frequency masthead stats-widget icon; text or a symbol. Unset (null) = the binary's default.";
    };

    statsLoadIcon = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      description = "Load masthead stats-widget icon; text or a symbol. Unset (null) = the binary's default.";
    };

    statsUptimeIcon = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      description = "Uptime masthead stats-widget icon; text or a symbol. Unset (null) = the binary's default.";
    };

    statsBatteryIcon = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      description = "Battery masthead stats-widget icon; text or a symbol. Unset (null) = the binary's default.";
    };

    statsBatteryChargingIcon = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      description = "Battery-charging masthead stats-widget icon; text or a symbol. Unset (null) = the binary's default.";
    };

    statsBatteryWarn = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.u8);
      default = null;
      description = "Battery percentage at/below which the widget turns red. Unset (null) = the binary's default.";
    };

    statsDiskIcon = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      description = "Disk masthead stats-widget icon; text or a symbol. Unset (null) = the binary's default.";
    };

    statsDiskFreeWarn = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.u8);
      default = null;
      description = "Free-disk percentage at/below which the disk widget turns amber. Unset (null) = the binary's default.";
    };

    statsDiskFreeCritical = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.u8);
      default = null;
      description = "Free-disk percentage at/below which the disk widget turns red. Unset (null) = the binary's default.";
    };

    statsDiskPath = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      description = "Filesystem the disk masthead widget measures; empty = the filesystem holding the worktrees directory. Unset (null) = the binary's default.";
    };

    notificationsDesktop = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Enable desktop notifications. Unset (null) = the binary's default.";
    };

    notificationsSurfaceSelfLogErrors = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Surface thegn's own log errors as notifications. Unset (null) = the binary's default.";
    };

    notificationsGithubMentions = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Enable notifications for GitHub mentions. Unset (null) = the binary's default.";
    };

    notificationsAgentAttentionInbox = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Route agent-attention events to the inbox. Unset (null) = the binary's default.";
    };

    notificationsMergeQueueHoldDesktop = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Show merge-queue hold notifications on desktop sinks. Unset (null) = the binary's default.";
    };

    notificationsDndEnabled = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Enable do-not-disturb notification handling. Unset (null) = the binary's default.";
    };

    notificationsSoundMute = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Mute notification sounds. Unset (null) = the binary's default.";
    };

    notificationsSoundSuppressFocused = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Suppress sounds for focused notifications. Unset (null) = the binary's default.";
    };

    activityEnabled = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Track activity dots. Unset (null) = the binary's default.";
    };

    activityRedRequiresAgent = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Require a real agent before a worktree can show a needs-you dot. Unset (null) = the binary's default.";
    };

    activityAgentPrograms = lib.mkOption {
      type = lib.types.nullOr (lib.types.listOf lib.types.str);
      default = null;
      description = "Additional agent CLI program names. Unset (null) = the binary's default.";
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

    recordingDir = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      description = "Directory for session recordings (empty = the per-profile default). Unset (null) = the binary's default.";
    };

    recordingMaxBytes = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.unsigned);
      default = null;
      description = "Cap on a recording file in bytes (0 = no cap). Unset (null) = the binary's default.";
    };

    mediaEnabled = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Enable the media controls. Unset (null) = the binary's default.";
    };

    mediaDefaultAction = lib.mkOption {
      type = lib.types.nullOr (lib.types.enum ["play_pause" "next" "previous" "volume_up" "volume_down"]);
      default = null;
      description = "Action bound to a bare media key. Unset (null) = the binary's default.";
    };

    mediaShowArt = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Show album art in the media overlay. Unset (null) = the binary's default.";
    };

    mediaOverlayOnBadgeClick = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Open the media overlay when the badge is clicked. Unset (null) = the binary's default.";
    };

    mediaSeekStepSecs = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.between 0 315360000);
      default = null;
      description = "Seek step in seconds for audio. Unset (null) = the binary's default.";
    };

    mediaSeekStepVideoSecs = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.between 0 315360000);
      default = null;
      description = "Seek step in seconds for video. Unset (null) = the binary's default.";
    };

    mediaPollIntervalSecs = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.between 0 2678400);
      default = null;
      description = "Media player poll interval in seconds. Unset (null) = the binary's default.";
    };

    mediaPlayersPriority = lib.mkOption {
      type = lib.types.nullOr (lib.types.listOf lib.types.str);
      default = null;
      description = "Preferred media players, highest priority first. Unset (null) = the binary's default.";
    };

    monitorProcRows = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.unsigned);
      default = null;
      description = "Rows shown in the monitor process table. Unset (null) = the binary's default.";
    };

    monitorProcesses = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Sample processes in the monitor. Unset (null) = the binary's default.";
    };

    monitorProcessIo = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Sample per-process I/O in the monitor. Unset (null) = the binary's default.";
    };

    limitsTestCpuQuota = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      description = "CPU quota (systemd CPUQuota, such as 150%) for test runs. Unset (null) = the binary's default.";
    };

    limitsTestMemMax = lib.mkOption {
      type = lib.types.nullOr (lib.types.str);
      default = null;
      description = "Memory ceiling (systemd MemoryMax, such as 4G) for test runs. Unset (null) = the binary's default.";
    };

    limitsHeavySlots = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.unsigned);
      default = null;
      description = "Maximum concurrent heavyweight gates on this host. Unset (null) = the binary's default.";
    };

    limitsMaxLoad = lib.mkOption {
      type = lib.types.nullOr (lib.types.numbers.nonnegative);
      default = null;
      description = "One-minute load-average ceiling for heavyweight gates. Unset (null) = the binary's default.";
    };

    limitsTestMaxParallel = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.unsigned);
      default = null;
      description = "Maximum parallel test jobs. Unset (null) = the binary's default.";
    };

    limitsTestTimeoutSecs = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.between 0 315360000);
      default = null;
      description = "Test run timeout in seconds. Unset (null) = the binary's default.";
    };

    limitsDiscoverTimeoutSecs = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.between 0 315360000);
      default = null;
      description = "Test discovery timeout in seconds. Unset (null) = the binary's default.";
    };

    limitsIsolatedTargetDir = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Give test runs an isolated build target directory. Unset (null) = the binary's default.";
    };

    diagnosticsCrashReports = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Write crash reports. Unset (null) = the binary's default.";
    };

    diagnosticsCrashRetention = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.unsigned);
      default = null;
      description = "Number of crash reports to keep. Unset (null) = the binary's default.";
    };

    diagnosticsRingSize = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.unsigned);
      default = null;
      description = "Entries held in the in-memory diagnostics ring. Unset (null) = the binary's default.";
    };

    diskShowSizes = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Show per-worktree disk sizes. Unset (null) = the binary's default.";
    };

    diskWarnThresholdGb = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.unsigned);
      default = null;
      description = "Total GB above which `thegn disk` reports a warning. Unset (null) = the binary's default.";
    };

    diskScanIntervalSecs = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.between 0 2678400);
      default = null;
      description = "Seconds between background disk scans. Unset (null) = the binary's default.";
    };

    diskMaxScanPerRound = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.u32);
      default = null;
      description = "Maximum worktrees measured per scan round. Unset (null) = the binary's default.";
    };

    diskAutoCleanOnMerge = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Clean a worktree's build output when its branch merges. Unset (null) = the binary's default.";
    };

    diskCleanOnPrClosed = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Clean a worktree's build output when its PR closes. Unset (null) = the binary's default.";
    };

    diskIdleCleanDays = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.u32);
      default = null;
      description = "Days a worktree sits idle before its build output is reclaimed. Unset (null) = the binary's default.";
    };

    diskGenerationMinAgeDays = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.u32);
      default = null;
      description = "Minimum age in days before an old build generation is pruned. Unset (null) = the binary's default.";
    };

    diskReclaimOnLowDisk = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Reclaim least-recently-touched build output under disk pressure. Unset (null) = the binary's default.";
    };

    diskSccache = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Wire sccache into the dev shell. Unset (null) = the binary's default.";
    };

    daemonEnabled = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Route new panes through the pane daemon so they survive quitting the UI; false restores in-process panes that die with the compositor. Unset (null) = the binary's default.";
    };

    daemonIdleExitSecs = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.between 0 315360000);
      default = null;
      description = "Seconds with no live sessions before the pane daemon exits; 0 never exits. Unset (null) = the binary's default.";
    };

    daemonLeaseGraceSecs = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.between 0 315360000);
      default = null;
      description = "Seconds a detached session's PTY stays warm; 0 never reaps. Unset (null) = the binary's default.";
    };

    daemonStreamGapWindowSecs = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.between 0 315360000);
      default = null;
      description = "Seconds of repeated attach-stream sequence gaps before a pane is ended; 0 never ends it. Unset (null) = the binary's default.";
    };

    networkOfflineAfterFailures = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.u32);
      default = null;
      description = "Consecutive transient network failures before the app concludes it is offline (auto mode). Unset (null) = the binary's default.";
    };

    networkMode = lib.mkOption {
      type = lib.types.nullOr (lib.types.enum ["auto" "online" "offline"]);
      default = null;
      description = "Offline-detection mode. Unset (null) = the binary's default.";
    };

    networkRecoveryProbeSecs = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.between 0 2678400);
      default = null;
      description = "While offline, re-probe for connectivity at most this often (seconds). Unset (null) = the binary's default.";
    };

    previewEnabled = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Enable frontend preview discovery and surfaces. This never launches a dev server. Unset (null) = the binary's default.";
    };

    previewFetchTimeoutMs = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.between 100 30000);
      default = null;
      description = "Wall-clock limit for one bounded preview HTTP fetch (milliseconds). Unset (null) = the binary's default.";
    };

    previewMaxBodyBytes = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.between 1 16777216);
      default = null;
      description = "Maximum response bytes retained by one preview fetch. Unset (null) = the binary's default.";
    };

    usageEnabled = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Enable the AI-account usage tracker. Unset (null) = the binary's default.";
    };

    usageStatusbar = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Show the usage badge in the statusbar. Unset (null) = the binary's default.";
    };

    usageDiscoverProfiles = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Discover additional provider credential profiles. Unset (null) = the binary's default.";
    };

    usageTokenRollups = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Aggregate host-wide token counts from local transcripts. Unset (null) = the binary's default.";
    };

    usageHistoryDays = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.between 0 3650);
      default = null;
      description = "Days of usage history to retain (0 disables history). Unset (null) = the binary's default.";
    };

    # Rust floors values below 60 at use; keep the Nix option stricter.
    usagePollIntervalSecs = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.between 60 2678400);
      default = null;
      description = "Usage polling cadence in seconds (minimum 60). Unset (null) = the binary's default.";
    };

    lspEnabled = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Enable language-server integration. Unset (null) = the binary's default.";
    };

    lspHover = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Show language-server hover and signature previews. Unset (null) = the binary's default.";
    };

    lspRequireContainment = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Refuse language servers that cannot join the shared resource slice. Unset (null) = the binary's default.";
    };

    semanticWorktreeIndex = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Enable the worktree-wide semantic entity index. Unset (null) = the binary's default.";
    };

    semanticIndexMaxFiles = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.positive);
      default = null;
      description = "Maximum number of files parsed by the semantic index. Unset (null) = the binary's default.";
    };

    semanticMapBudgetLines = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.positive);
      default = null;
      description = "Default line budget for the semantic repo map. Unset (null) = the binary's default.";
    };

    hostDiscoveryTailnetEnabled = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Offer tailnet host discovery. Unset (null) = the binary's default.";
    };

    hostDiscoveryTailnetOnlineOnly = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Limit tailnet discovery to online peers. Unset (null) = the binary's default.";
    };

    lifecycleSnapshotServerSideEncryption = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Sign S3 snapshot uploads with server-side AES256 encryption. Unset (null) = the binary's default.";
    };

    lifecycleSnapshotConditionalCreate = lib.mkOption {
      type = lib.types.nullOr (lib.types.bool);
      default = null;
      description = "Require immutable S3 snapshot uploads to create only when absent. Unset (null) = the binary's default.";
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
