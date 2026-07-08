# The superzej multi-arch base sandbox image: nix (flakes on) + devenv/direnv +
# rust toolchain + node + the Claude Code CLI + the daily tools, with a populated
# /nix and a warmed ~/.cargo
# owned by uid 1000 (`superzej`, matching `--userns keep-id`). Built per-arch on
# native builders and pushed as a manifest list (see `just image-publish`); the
# host provisioner delivers the per-arch digest registry-lessly by default.
#
#   nix build .#sandbox-image        # streamLayeredImage -> a tar streamer
#   ./result | podman load           # local test load
#
# The /nix in this image IS the warm-volume seed: first mount of the
# `superzej-nix-store` named volume at /nix copy-ups the whole store — zero
# extra transfer (see superzej-core/src/host.rs VolumeSeed::ImageCopyUp).
{pkgs}: let
  user = "superzej";
  uid = "1000";
  gid = "1000";

  # What a fresh sandbox needs before any repo-specific devShell exists.
  rootEnv = pkgs.buildEnv {
    name = "superzej-sandbox-env";
    paths = with pkgs; [
      # nix-first workflow
      nix
      direnv
      nix-direnv
      # rust (rustup so repos pin their own toolchains; a stable default is
      # warmed at build time below)
      rustup
      # devenv (repo devShells) + node (agents) + the Claude Code CLI, baked so
      # a container on a remote host has the toolchain without per-worktree
      # install (the remote-OCI worktree path can't push the host's nix closure).
      devenv
      nodejs
      claude-code
      # the daily substrate
      bashInteractive
      zsh
      coreutils
      findutils
      gnugrep
      gnused
      gawk
      git
      openssh
      curl
      cacert
      gnutar
      gzip
      xz
      which
      procps
      shadow
      dtach
      rsync
    ];
    pathsToLink = ["/bin" "/share" "/etc"];
  };

  etcFiles = pkgs.runCommand "superzej-sandbox-etc" {} ''
    mkdir -p $out/etc/nix $out/etc/skel
    cat > $out/etc/passwd <<EOF
    root:x:0:0:root:/root:/bin/sh
    ${user}:x:${uid}:${gid}:superzej:/home/${user}:${pkgs.bashInteractive}/bin/bash
    nobody:x:65534:65534:nobody:/nonexistent:/bin/false
    EOF
    cat > $out/etc/group <<EOF
    root:x:0:
    ${user}:x:${gid}:
    nogroup:x:65534:
    EOF
    cat > $out/etc/nix/nix.conf <<EOF
    experimental-features = nix-command flakes
    require-sigs = false
    build-users-group =
    EOF
    echo 'nameserver 1.1.1.1' > $out/etc/resolv.conf.fallback
  '';
in
  pkgs.dockerTools.streamLayeredImage {
    name = "superzej-sandbox";
    tag = "latest";
    contents = [rootEnv etcFiles pkgs.dockerTools.usrBinEnv pkgs.dockerTools.binSh];

    # Root-owned store; the sandbox user gets writable HOME + tmp. `fakeRootCommands`
    # runs under fakeroot so the chowns stick in the layer.
    enableFakechroot = false;
    fakeRootCommands = ''
      mkdir -p ./home/${user}/.cargo ./home/${user}/.config/nix ./tmp ./workspace ./root
      chmod 1777 ./tmp
      cat > ./home/${user}/.config/nix/nix.conf <<EOF
      experimental-features = nix-command flakes
      require-sigs = false
      EOF
      chown -R ${uid}:${gid} ./home/${user} ./workspace
    '';

    config = {
      User = "${uid}:${gid}";
      WorkingDir = "/workspace";
      Env = [
        "HOME=/home/${user}"
        "USER=${user}"
        "PATH=/home/${user}/.cargo/bin:/usr/bin:/bin:/nix/var/nix/profiles/default/bin"
        "NIX_CONFIG=experimental-features = nix-command flakes"
        "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
        "NIX_SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
      ];
      Cmd = ["/bin/sh" "-l"];
      Labels = {
        "superzej.managed" = "true";
        "superzej.image.role" = "base";
        "org.opencontainers.image.source" = "https://github.com/blake/superzej";
        "org.opencontainers.image.description" = "superzej sandbox base: nix + direnv + rustup, uid-1000, warm-volume seedable";
      };
    };
  }
