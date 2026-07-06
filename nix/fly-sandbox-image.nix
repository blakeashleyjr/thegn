# Fly.io boot image: the superzej sandbox toolchain (nix + direnv + a baked rust
# + just + the daily substrate) that runs **sshd as its entrypoint**, so a Fly
# machine boots straight into an ssh-reachable shell with the toolchain already
# present — no per-VM apt/nix install (the ~40s cold path the stock-ubuntu init
# pays). superzej's `FlyProvider` injects `/root/.ssh/authorized_keys` via the
# Machines `files` API and exposes `tcp/22`; this image supplies the sshd + tools.
#
#   nix build .#fly-sandbox-image     # streamLayeredImage -> a tar streamer
#   ./result | docker load            # local test load
#   # publish (see `just fly-image-publish`): tag + push to registry.fly.io/<app>,
#   # then set `[env.fly.provider] template = "image:registry.fly.io/<app>:latest"`.
#
# Runs as ROOT (unlike the uid-1000 base sandbox image): sshd must bind :22 and
# manage host keys; sessions land as root, matching the VPS reachability model.
{pkgs}: let
  # Baked toolchain — rust + just are present immediately (no rustup network
  # dance), mirroring the lean `devShells.sandbox` (rust + just). `rustup` is also
  # here so a repo can still pin its own toolchain on top.
  toolEnv = pkgs.buildEnv {
    name = "superzej-fly-sandbox-env";
    paths = with pkgs; [
      # nix-first workflow
      nix
      direnv
      nix-direnv
      # baked rust (instant) + repo-pinnable rustup + task runner
      cargo
      rustc
      clippy
      rustfmt
      rustup
      just
      # daily substrate
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
      rsync
    ];
    pathsToLink = ["/bin" "/share" "/etc"];
  };

  # PID-1 entrypoint: set up + exec sshd. Host keys are generated on first boot;
  # superzej's injected /root/.ssh/authorized_keys authenticates root by key only.
  entrypoint = pkgs.writeShellScript "sz-fly-entrypoint" ''
    set -e
    mkdir -p /run/sshd /root/.ssh /etc/ssh
    chmod 700 /root/.ssh
    [ -f /root/.ssh/authorized_keys ] && chmod 600 /root/.ssh/authorized_keys || true
    for t in rsa ed25519; do
      [ -f "/etc/ssh/ssh_host_''${t}_key" ] || \
        ${pkgs.openssh}/bin/ssh-keygen -q -t "$t" -f "/etc/ssh/ssh_host_''${t}_key" -N ""
    done
    cat > /etc/ssh/sshd_config <<EOF
    Port 22
    PermitRootLogin prohibit-password
    PubkeyAuthentication yes
    PasswordAuthentication no
    UsePAM no
    Subsystem sftp internal-sftp
    EOF
    exec ${pkgs.openssh}/bin/sshd -D -e -f /etc/ssh/sshd_config
  '';

  etcFiles = pkgs.runCommand "superzej-fly-etc" {} ''
    mkdir -p $out/etc/nix
    cat > $out/etc/passwd <<EOF
    root:x:0:0:root:/root:${pkgs.bashInteractive}/bin/bash
    nobody:x:65534:65534:nobody:/nonexistent:/bin/false
    EOF
    cat > $out/etc/group <<EOF
    root:x:0:
    nogroup:x:65534:
    EOF
    printf 'experimental-features = nix-command flakes\nrequire-sigs = false\nbuild-users-group =\n' > $out/etc/nix/nix.conf
  '';
in
  pkgs.dockerTools.streamLayeredImage {
    name = "superzej-fly-sandbox";
    tag = "latest";
    contents = [
      toolEnv
      etcFiles
      pkgs.dockerTools.usrBinEnv
      pkgs.dockerTools.binSh
    ];
    config = {
      Entrypoint = ["${entrypoint}"];
      Env = [
        "HOME=/root"
        "USER=root"
        "PATH=/bin:/usr/bin:/nix/var/nix/profiles/default/bin"
        "NIX_CONFIG=experimental-features = nix-command flakes"
        "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
        "NIX_SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
      ];
      ExposedPorts = {"22/tcp" = {};};
      Labels = {
        "superzej.managed" = "true";
        "superzej.image.role" = "fly-sandbox";
        "org.opencontainers.image.description" = "superzej Fly sandbox: sshd entrypoint + baked nix/rust/just toolchain";
      };
    };
  }
