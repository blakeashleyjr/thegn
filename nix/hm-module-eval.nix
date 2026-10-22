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
          themeAccent = "#f083ba";
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

  minimalConfig = minimal.config.xdg.configFile."thegn/config.toml".source;
  maximalConfig = maximal.config.xdg.configFile."thegn/config.toml".source;
in
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
