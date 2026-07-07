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
{
  pkgs,
  rustToolchain,
}: let
  inherit (pkgs) lib;

  # The lean iroh call-home agent (`sz-agent`, crate `superzej-agent`), baked into
  # the image so a Fly machine dials the compositor over iroh **alongside** sshd.
  # Built with the SAME stable toolchain the rest of the flake uses, scoped to the
  # one crate (`-p superzej-agent`) so we don't drag in the full host/svc build.
  # The compositor injects SUPERZEJ_HOME_NODE / SUPERZEJ_SANDBOX_AUTH /
  # SUPERZEJ_SANDBOX_ID into the machine env (see `FlyProvider`'s `IrohInject`);
  # the entrypoint launches this binary, which reads them on boot.
  szAgentPlatform = pkgs.makeRustPlatform {
    cargo = rustToolchain;
    rustc = rustToolchain;
  };
  szAgent = szAgentPlatform.buildRustPackage {
    pname = "sz-agent";
    version = "0.1.0";
    src = lib.cleanSourceWith {
      src = ../.;
      # Drop build artifacts so the store path is stable across rebuilds (same
      # filter `nix/package.nix` uses).
      filter = path: _type: let
        rel = lib.removePrefix (toString ../. + "/") (toString path);
      in
        !(lib.hasPrefix "target" rel
          || lib.hasPrefix "result" rel
          || lib.hasPrefix ".direnv" rel
          || lib.hasPrefix ".git/" rel);
    };
    cargoLock.lockFile = ../Cargo.lock;
    # Compile only the lean agent binary (not the host/svc/coverage targets).
    cargoBuildFlags = ["-p" "superzej-agent" "--bin" "sz-agent"];
    # superzej-core (the agent's one workspace dep) pulls the same vendored C
    # (libgit2/LMDB via fff-search → libz-sys), so mirror package.nix's inputs.
    nativeBuildInputs = [pkgs.pkg-config];
    buildInputs = [pkgs.zlib];
    # The PTY tests need a real /bin/sh + pty the hermetic sandbox lacks; `just
    # test` gates them. This derivation just compiles + installs the binary.
    doCheck = false;
  };

  # Baked toolchain — the SAME combined rust toolchain the flake's
  # `devShells.sandbox` uses (clippy/rustfmt included, single derivation so no
  # buildEnv collisions), present immediately with no rustup network dance. `just`
  # rounds out the lean sandbox shell (rust + just).
  toolEnv = pkgs.buildEnv {
    name = "superzej-fly-sandbox-env";
    paths = with pkgs; [
      # nix-first workflow
      nix
      direnv
      nix-direnv
      # baked rust (instant) + task runner
      rustToolchain
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
    # Privilege-separation dir modern sshd requires (owned root, 0755).
    mkdir -p /run/sshd /var/empty /root/.ssh /etc/ssh
    chmod 0755 /var/empty
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
    # iroh call-home: when the compositor injected the three SUPERZEJ_* env vars
    # (see FlyProvider's IrohInject), start the baked agent in the BACKGROUND so
    # its iroh reach comes up alongside sshd. sshd stays PID 1 (the reachability
    # model is unchanged). On an ssh-only machine the vars are absent, so we skip
    # the launch entirely rather than crash-loop a dial with no home.
    if [ -n "''${SUPERZEJ_HOME_NODE:-}" ]; then
      ${szAgent}/bin/sz-agent &
    fi
    # -e logs to stderr (→ Fly logs) so a startup failure is diagnosable.
    exec ${pkgs.openssh}/bin/sshd -D -e -f /etc/ssh/sshd_config
  '';

  etcFiles = pkgs.runCommand "superzej-fly-etc" {} ''
    mkdir -p $out/etc/nix
    # Include the unprivileged `sshd` privsep user/group — without it modern
    # OpenSSH refuses to start ("Privilege separation user sshd does not exist").
    cat > $out/etc/passwd <<EOF
    root:x:0:0:root:/root:${pkgs.bashInteractive}/bin/bash
    sshd:x:74:74:sshd privsep:/var/empty:/bin/false
    nobody:x:65534:65534:nobody:/nonexistent:/bin/false
    EOF
    cat > $out/etc/group <<EOF
    root:x:0:
    sshd:x:74:
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
      # The baked iroh call-home agent — also on PATH as `sz-agent` for debugging
      # (the entrypoint launches it by absolute store path).
      szAgent
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
