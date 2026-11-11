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
          gitOverrideGpg = true;
          gitMergeGuard = false;
          gitAutoFetch = false;
          gitAutoFetchIntervalSecs = 600;
          gitAutoFetchMinIntervalSecs = 120;
          gitAutoFetchNotify = true;
          gitAutoFetchColocated = true;
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
          statsDiskIcon = "D";
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

  minimalConfig = minimal.config.xdg.configFile."thegn/config.toml".source;
  maximalConfig = maximal.config.xdg.configFile."thegn/config.toml".source;
in
  assert !(builtins.tryEval (builtins.deepSeq invalidThemeColor.config.programs.thegn.themeColor true)).success;
  assert !(builtins.tryEval (builtins.deepSeq oversizedThemePanePadding.config.programs.thegn.themePanePadding true)).success;
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
