{
  pkgs,
  lib,
  self,
  thegn,
}: let
  # This stand-in covers only the two Home Manager options written by the
  # module. It evaluates the thegn module itself, not Home Manager.
  stub = {lib, ...}: {
    options.home.packages = lib.mkOption {
      type = lib.types.listOf lib.types.package;
      default = [];
    };
    options.xdg.configFile = lib.mkOption {
      type = lib.types.attrsOf (lib.types.submodule {
        options.source = lib.mkOption {
          type = lib.types.path;
        };
      });
      default = {};
    };
  };

  minimal = lib.evalModules {
    modules = [
      stub
      (import ./hm-module.nix self)
      {
        programs.thegn = {
          enable = true;
          package = thegn;
        };
      }
    ];
    specialArgs = {inherit pkgs;};
  };

  maximal = lib.evalModules {
    modules = [
      stub
      (import ./hm-module.nix self)
      {
        programs.thegn = {
          enable = true;
          package = thegn;
          worktreesDir = "/tmp/thegn-hm/worktrees";
          projectsDir = "/tmp/thegn-hm/projects";
          workspacesDir = "/tmp/thegn-hm/workspaces";
          repoRoots = ["/tmp/thegn-hm/repos"];
          repoScanDepth = 3;
          baseBranch = "main";
          branchPrefix = "hm/";
          picker = "auto";
          worktreeMode = "global";
          nameScheme = "words";
          autoRemoveWorktree = true;
          confirmDelete = false;
          themeAccent = "#f083ba";
          themePreset = "storm";
          themeFocusBorder = "#6ee7d8";
          themePanePadding = 1;
          themeUndercurl = "auto";
          themeColor = "256";
          themeGlyphs = "unicode";
          themeAgentGlyphs = "symbol";
          themeMascot = "knight";
          themeMascotMotion = "still";
          uiConfirmDeleteProject = false;
          uiFullModeChip = false;
          uiDismissOverlayOnClickOutside = false;
          uiSidebarFreezeSort = false;
          uiSidebarNavSkipsCollapsed = false;
          uiSidebarDividers = false;
          uiFullscreenKeepMasthead = false;
          uiFullscreenKeepStatusbar = false;
          uiSidebarShowStatusIcon = false;
          uiSidebarShowDiffStat = false;
          uiSidebarShowAheadBehind = false;
          uiSidebarShowPrChip = false;
          uiSidebarShowJj = false;
          uiSidebarShowSubmodules = false;
          uiSidebarDetailBranch = false;
          uiSidebarDetailBranchStat = false;
          uiSidebarDetailPr = false;
          uiSidebarProjectSort = "attention";
          uiSidebarTerminalsSection = "nonempty";
          uiSidebarFocusDetail = "all";
          uiLanguage = "auto";
          uiSidebarIconAhead = "+";
          uiSidebarIconBehind = "-";
          uiSidebarIconStatus = "*";
          searchMaxResults = 500;
          searchRespectGitignore = false;
          searchIncludeHidden = true;
          searchStructural = "none";
          paletteContentMaxResults = 250;
          paletteFileMaxResults = 100;
          paletteSymbolMaxResults = 50;
          paletteContentSearchHidden = true;
          sessionScrollbackLines = 200;
          sessionRestoreGraceSecs = 120;
          sessionFocusOnCreate = false;
          sessionResidentPoolLimit = 4;
          clipboardImagePaste = false;
          clipboardMaxImageBytes = 1048576;
          clipboardRemoteDir = "~/.cache/thegn/paste-hm";
          clipboardKeepHours = 12;
          locEnabled = false;
          locScanIntervalSecs = 1800;
          locMaxScanPerRound = 4;
          locWatchInvalidateSecs = 120;
          replayEnabled = true;
          replayMaxBytesPerPane = 4194304;
          replayMaxDurationSecs = 300;
          replayKeyframeIntervalMs = 2000;
          replayKeyframeIntervalBytes = 65536;
          replayIdleThresholdMs = 1500;
          replayPersist = false;
          drawerCommand = "yazi";
          drawerConfigHome = "system";
          drawerHeight = "40%";
          drawerWidth = "center";
          drawerImagePreviews = true;
          drawerGitStatus = false;
          drawerContain = false;
          drawerMemoryMax = "1G";
          drawerMemorySwapMax = "256M";
          drawerCpuQuota = "150%";
          drawerPoolLimit = 2;
          drawerPrewarm = false;
          stripVisible = false;
          gitBackend = "cli";
          gitOverrideGpg = true;
          gitMergeGuard = false;
          gitAutoFetch = false;
          gitAutoFetchIntervalSecs = 600;
          gitAutoFetchMinIntervalSecs = 120;
          gitAutoFetchNotify = true;
          gitStructuralDiff = "difft";
          gitAutoFetchColocated = true;
          gitSubmodules = "off";
          statsCpuIcon = "C";
          statsMemIcon = "M";
          statsNetIcon = "N";
          statsGpuIcon = "G";
          statsTempIcon = "T";
          statsSwapIcon = "S";
          statsFreqIcon = "F";
          statsLoadIcon = "L";
          statsUptimeIcon = "U";
          statsBatteryIcon = "B";
          statsBatteryChargingIcon = "BC";
          statsBatteryWarn = 30;
          statsDiskIcon = "D";
          statsDiskFreeWarn = 20;
          statsDiskFreeCritical = 5;
          statsDiskPath = "~";
          notificationsDesktop = false;
          notificationsSurfaceSelfLogErrors = true;
          notificationsGithubMentions = false;
          notificationsAgentAttentionInbox = true;
          notificationsMergeQueueHoldDesktop = true;
          notificationsDndEnabled = true;
          notificationsSoundMute = true;
          notificationsSoundSuppressFocused = false;
          activityEnabled = false;
          activityRedRequiresAgent = false;
          activityAgentPrograms = ["claude" "my-agent"];
          monitorSystem = "htop";
          monitorGpu = "nvtop";
          recordingDir = "/tmp/thegn-hm/recordings";
          recordingMaxBytes = 1048576;
          mediaEnabled = true;
          mediaShowArt = false;
          mediaOverlayOnBadgeClick = false;
          mediaSeekStepSecs = 5;
          mediaSeekStepVideoSecs = 20;
          mediaPollIntervalSecs = 4;
          mediaPlayersPriority = ["mpv" "spotify"];
          monitorProcRows = 15;
          monitorProcesses = true;
          monitorProcessIo = true;
          limitsTestCpuQuota = "200%";
          limitsTestMemMax = "2G";
          limitsTestMaxParallel = 2;
          limitsTestTimeoutSecs = 900;
          limitsDiscoverTimeoutSecs = 30;
          limitsIsolatedTargetDir = false;
          diagnosticsCrashReports = true;
          diagnosticsCrashRetention = 5;
          diagnosticsRingSize = 128;
          diskShowSizes = false;
          diskWarnThresholdGb = 200;
          diskScanIntervalSecs = 60;
          diskMaxScanPerRound = 8;
          diskAutoCleanOnMerge = false;
          diskCleanOnPrClosed = true;
          diskIdleCleanDays = 30;
          diskGenerationMinAgeDays = 21;
          diskReclaimOnLowDisk = false;
          diskSccache = true;
          daemonEnabled = true;
          daemonIdleExitSecs = 600;
          daemonLeaseGraceSecs = 30;
          daemonStreamGapWindowSecs = 300;
          networkOfflineAfterFailures = 3;
          networkRecoveryProbeSecs = 15;
          previewEnabled = false;
          previewFetchTimeoutMs = 1000;
          previewMaxBodyBytes = 4096;
          usageEnabled = true;
          usageStatusbar = false;
          usageDiscoverProfiles = true;
          usageTokenRollups = true;
          usageHistoryDays = 30;
          usagePollIntervalSecs = 600;
          lspEnabled = false;
          lspHover = false;
          lspRequireContainment = true;
          semanticWorktreeIndex = false;
          semanticIndexMaxFiles = 7000;
          semanticMapBudgetLines = 500;
          hostDiscoveryTailnetEnabled = true;
          hostDiscoveryTailnetOnlineOnly = false;
          agents = [
            {
              name = "shell";
              command = "__shell__";
            }
          ];
          tools = [
            {
              name = "deploy";
              command = "just deploy";
            }
          ];
          prTtlSecs = 3600;
          watchPrIntervalSecs = 60;
          log = {
            level = "info";
            file = true;
            dir = "/tmp/thegn-hm/logs";
            rotationSizeMb = 8;
            maxFiles = 4;
            stderrCapMb = 12;
            format = "text";
          };
          keybinds = {new-worktree = "Ctrl w";};
          actions = [
            {
              name = "deploy";
              key = "Alt D";
              run = "just deploy";
              menu = true;
              hint = "Deploy";
            }
          ];
          sandbox = {
            enabled = true;
            backend = "podman";
            backendChain = ["podman"];
            image = "ghcr.io/you/devbox:latest";
            network = "nat";
            envPassthrough = ["PATH"];
            mounts = ["/tmp/thegn-hm:/tmp/thegn-hm:ro"];
            initScript = "true";
            devenv = false;
            onMissing = "warn";
            remote = {
              host = "user@devbox";
              port = 22;
              transport = "ssh";
              mode = "remote";
              remoteDir = "/tmp/thegn-hm/remote";
              forwardAgent = true;
            };
          };
        };
      }
    ];
    specialArgs = {inherit pkgs;};
  };

  evalWithOptions = options:
    lib.evalModules {
      modules = [
        stub
        (import ./hm-module.nix self)
        {
          programs.thegn =
            {
              enable = true;
              package = thegn;
            }
            // options;
        }
      ];
      specialArgs = {inherit pkgs;};
    };

  invalidThemeColor = evalWithOptions {themeColor = "24bit";};
  oversizedThemePanePadding = evalWithOptions {themePanePadding = 65536;};
  undersizedPreviewFetchTimeout = evalWithOptions {previewFetchTimeoutMs = 99;};
  oversizedPreviewMaxBodyBytes = evalWithOptions {previewMaxBodyBytes = 16777217;};
  undersizedUsagePollInterval = evalWithOptions {usagePollIntervalSecs = 59;};
  oversizedUsageHistoryDays = evalWithOptions {usageHistoryDays = 3651;};
  zeroSemanticIndexMaxFiles = evalWithOptions {semanticIndexMaxFiles = 0;};
  zeroSemanticMapBudgetLines = evalWithOptions {semanticMapBudgetLines = 0;};

  minimalConfig = minimal.config.xdg.configFile."thegn/config.toml".source;
  maximalConfig = maximal.config.xdg.configFile."thegn/config.toml".source;
in
  assert !(builtins.tryEval (builtins.deepSeq invalidThemeColor.config.programs.thegn.themeColor true)).success;
  assert !(builtins.tryEval (builtins.deepSeq oversizedThemePanePadding.config.programs.thegn.themePanePadding true)).success;
  assert !(builtins.tryEval (builtins.deepSeq undersizedPreviewFetchTimeout.config.programs.thegn.previewFetchTimeoutMs true)).success;
  assert !(builtins.tryEval (builtins.deepSeq oversizedPreviewMaxBodyBytes.config.programs.thegn.previewMaxBodyBytes true)).success;
  assert !(builtins.tryEval (builtins.deepSeq undersizedUsagePollInterval.config.programs.thegn.usagePollIntervalSecs true)).success;
  assert !(builtins.tryEval (builtins.deepSeq oversizedUsageHistoryDays.config.programs.thegn.usageHistoryDays true)).success;
  assert !(builtins.tryEval (builtins.deepSeq zeroSemanticIndexMaxFiles.config.programs.thegn.semanticIndexMaxFiles true)).success;
  assert !(builtins.tryEval (builtins.deepSeq zeroSemanticMapBudgetLines.config.programs.thegn.semanticMapBudgetLines true)).success;
  assert minimal.config.home.packages == [thegn];
  assert maximal.config.home.packages == [thegn];
    pkgs.runCommand "thegn-hm-module-eval" {} ''
      set -euo pipefail
      export HOME="$TMPDIR"
      export XDG_STATE_HOME="$TMPDIR/state"
      export XDG_CONFIG_HOME="$TMPDIR/config"
      export THEGN_NO_MIGRATE=1
      unset THEGN_LOG
      mkdir -p "$XDG_STATE_HOME" "$XDG_CONFIG_HOME"

      for config in ${minimalConfig} ${maximalConfig}; do
        cat "$config"
        ${thegn}/bin/thegn --config "$config" config validate
      done
      touch $out
    ''
