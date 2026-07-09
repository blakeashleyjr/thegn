{
  description = "superzej — terminal-native git-worktree IDE";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
    # The pinned yazi superzej drives for its bottom file-manager drawer, on its
    # OWN nixpkgs input so superzej bundles a specific yazi (+ its preview tools)
    # independent of the user's system and of the main `nixpkgs`. Bump it
    # deliberately with `nix flake update nixpkgs-yazi`.
    nixpkgs-yazi.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    # Embedded app crates are git submodules. Local flake `self` sources contain
    # only gitlinks for submodules, so package builds need them as explicit
    # non-flake inputs and then copy them into Cargo's expected path-dep dirs.
    termiteChat = {
      url = "github:blakeashleyjr/termite-chat/26b0aebfb8284cf7d1dfd76dcbb786c96eeface2";
      flake = false;
    };
    # The muse visual-regression e2e harness (`just e2e`). Pinned as a non-flake
    # source and built with the same rust toolchain so `nix develop` and CI run
    # an identical, reproducible muse. Bump deliberately with `nix flake update muse`.
    muse = {
      url = "github:blakeashleyjr/muse/65672ef7e3a8c03809da8b47deeb616c2ea54d68";
      flake = false;
    };
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
    rust-overlay,
    nixpkgs-yazi,
    termiteChat,
    muse,
  }:
    flake-utils.lib.eachDefaultSystem (system: let
      pkgs = import nixpkgs {
        inherit system;
        overlays = [(import rust-overlay)];
      };
      # Same nixpkgs but permitting the (unfree) Claude Code CLI — scoped to the
      # sandbox base image only, so the dev shell / everything else stays free.
      imagePkgs = import nixpkgs {
        inherit system;
        overlays = [(import rust-overlay)];
        config.allowUnfreePredicate = pkg: builtins.elem (pkgs.lib.getName pkg) ["claude-code"];
      };
      # Formatter binaries that treefmt.toml references.  Bundled together so
      # both the `formatter` wrapper and `checks.formatting` use identical
      # versions — no drift between `nix fmt` and the devenv pre-commit hook.
      fmtPackages = with pkgs; [
        # Use pkgs.rustfmt (not rustToolchain) so the formatter version tracks
        # nixpkgs-unstable, independent of the rust-overlay pin.
        rustfmt
        alejandra
        shfmt
        taplo
        yamlfmt
        prettier
      ];
      # `nix fmt` wrapper: reads treefmt.toml from the source tree, with all
      # formatter binaries pre-wired onto PATH.
      treefmtWrapper = pkgs.writeShellScriptBin "treefmt" ''
        export PATH="${pkgs.lib.makeBinPath fmtPackages}:$PATH"
        exec ${pkgs.treefmt}/bin/treefmt \
          --config-file="$(${pkgs.git}/bin/git rev-parse --show-toplevel)/treefmt.toml" \
          "$@"
      '';
      # The pinned yazi + its preview/runtime tools, from `nixpkgs-yazi` so the
      # exact versions are frozen in flake.lock independently of the main nixpkgs.
      # superzej drives THIS yazi for the file-manager drawer (a private binary
      # via SUPERZEJ_YAZI_BIN + a private YAZI_CONFIG_HOME), never the system one.
      yaziPkgs = import nixpkgs-yazi {inherit system;};
      yaziPinned = yaziPkgs.yazi;
      # yazi's preview/runtime deps (fzf + zoxide are already in runtimeDeps).
      # `poppler-utils` (pdftoppm/pdftotext) is selected by attrpath — its hyphen
      # makes it unusable as a bare identifier inside `with`.
      yaziDeps =
        (with yaziPkgs; [
          file
          ffmpegthumbnailer
          unar
          jq
          fd
          ripgrep
          imagemagick
        ])
        ++ [yaziPkgs.poppler-utils];
      rootSrc = pkgs.lib.cleanSourceWith {
        src = ./.;
        # Drop build artifacts so the store path is stable across rebuilds.
        filter = path: _type: let
          rel = pkgs.lib.removePrefix (toString ./. + "/") (toString path);
        in
          !(pkgs.lib.hasPrefix "target" rel
            || pkgs.lib.hasPrefix "result" rel
            || pkgs.lib.hasPrefix ".direnv" rel
            || pkgs.lib.hasPrefix ".git/" rel);
      };
      # Local flake sources do not materialize git submodule contents in `self`,
      # but Cargo path dependencies need these embedded app crates at package
      # build time. Compose an explicit source tree with the submodule sources
      # copied into the paths declared by crates/superzej-host/Cargo.toml.
      superzejSrc = pkgs.runCommand "superzej-source" {} ''
        mkdir -p $out
        cp -R ${rootSrc}/. $out/
        chmod -R u+w $out

        rm -rf $out/apps/termite-chat
        mkdir -p $out/apps
        cp -R ${termiteChat} $out/apps/termite-chat
      '';
      superzej = pkgs.callPackage ./nix/package.nix {
        src = superzejSrc;
        yazi = yaziPinned;
        inherit yaziDeps;
      };

      # The OpenSpec CLI superzej uses for spec-driven development of itself.
      # A hermetic, pinned build (see nix/openspec.nix) — no global npm install,
      # telemetry off — shared by the dev shell and `just openspec*`.
      openspec = pkgs.callPackage ./nix/openspec.nix {};

      # One rust-overlay toolchain (clippy/rustfmt/rust-analyzer included).
      rustToolchain = pkgs.rust-bin.stable.latest.default.override {
        # llvm-tools for `cargo llvm-cov` (just coverage).
        extensions = ["llvm-tools-preview"];
        # macOS + Windows targets for `just check-cross`: the metrics + media
        # crates are C-dep-free leaves, so `cargo check --target` typechecks the
        # per-OS code (sysinfo/battery; MPRIS/SMTC/mpv/AppleScript players) on
        # this Linux box without a cross C toolchain (check never links). The
        # `windows` crate cross-checks fine on -gnu — no -msvc target needed.
        # This is the cross-platform regression gate.
        targets = [
          "aarch64-apple-darwin"
          "x86_64-pc-windows-gnu"
        ];
      };
      # The muse e2e harness, built from the pinned source with the same stable
      # toolchain. Pure-Rust (no system libs / git deps), so a vendored
      # `cargoLock.lockFile` build needs no cargoHash.
      musePlatform = pkgs.makeRustPlatform {
        cargo = rustToolchain;
        rustc = rustToolchain;
      };
      musePkg = musePlatform.buildRustPackage {
        pname = "muse";
        version = "0.1.0";
        src = muse;
        cargoLock.lockFile = "${muse}/Cargo.lock";
        cargoBuildFlags = ["-p" "muse-cli"];
        # The harness's own conformance tests aren't relevant to building the bin.
        doCheck = false;
      };

      # Static x86_64-linux-musl `szhost` — the resident bridge agent pushed into
      # Firecracker provider envs (Sprites). Self-contained (musl libc + bundled
      # sqlite + rustls TLS — no openssl), so it runs in a bare microVM. Built via
      # the cross stdenv's musl cc with +crt-static; a bare binary (no yazi/git
      # PATH wrapping — the bridge only speaks the stdio protocol on stdin/stdout).
      muslTarget = "x86_64-unknown-linux-musl";
      muslCross = pkgs.pkgsCross.musl64;
      rustMusl = pkgs.rust-bin.stable.latest.default.override {
        targets = [muslTarget];
      };
      muslRustPlatform = pkgs.makeRustPlatform {
        cargo = rustMusl;
        rustc = rustMusl;
      };
      muslCc = "${muslCross.stdenv.cc}/bin/${muslCross.stdenv.cc.targetPrefix}cc";
      szhostMusl = muslRustPlatform.buildRustPackage {
        pname = "szhost-musl";
        version = "0.1.0";
        src = superzejSrc;
        cargoLock.lockFile = ./Cargo.lock;
        # Force the musl target explicitly (env-only CARGO_BUILD_TARGET was being
        # overridden by buildRustPackage → a glibc host binary that can't run in a
        # bare microVM). `+crt-static` makes it fully static (no ld-musl loader
        # needed in the sandbox). Install from the cross target dir, not target/release.
        cargoBuildFlags = ["-p" "superzej-host" "--bin" "szhost" "--target" muslTarget];
        CARGO_BUILD_RUSTFLAGS = "-C target-feature=+crt-static";
        CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER = muslCc;
        CC_x86_64_unknown_linux_musl = muslCc;
        nativeBuildInputs = [muslCross.stdenv.cc];
        doCheck = false;
        installPhase = ''
          runHook preInstall
          mkdir -p $out/bin
          cp target/${muslTarget}/release/szhost $out/bin/szhost
          runHook postInstall
        '';
      };
    in {
      packages.default = superzej;
      packages.superzej = superzej;
      # The pinned yazi superzej drives for the file-manager drawer.
      packages.yazi = yaziPinned;
      # The muse e2e harness (`nix run .#muse`, also on the dev-shell PATH).
      packages.muse = musePkg;
      # Static musl bridge binary (`nix build .#szhost-musl`) — pushed into
      # provider microVMs as the resident agent (8-B.3).
      packages.szhost-musl = szhostMusl;
      # The OpenSpec CLI for spec-driven development (`nix run .#openspec`).
      packages.openspec = openspec;
      # The multi-arch base sandbox image (per-arch; `just image-build` loads it
      # locally, CI pushes both arches + a manifest list — see hosts-as-resources).
      packages.sandbox-image = import ./nix/sandbox-image.nix {pkgs = imagePkgs;};
      # Fly.io boot image: sshd entrypoint + baked toolchain, so a Fly machine
      # boots straight into a reachable shell (`template = "image:<ref>"`).
      packages.fly-sandbox-image = import ./nix/fly-sandbox-image.nix {inherit pkgs rustToolchain;};

      # `nix fmt` formats every tracked file via treefmt.toml.
      formatter = treefmtWrapper;

      checks = {
        # `nix flake check` gates on a clean build, formatting, and clippy.
        build = superzej;
        formatting =
          pkgs.runCommand "treefmt-check" {
            buildInputs = fmtPackages ++ [pkgs.treefmt pkgs.git];
          } ''
            set -euo pipefail
            cp -r ${self} src
            chmod -R u+w src
            cd src
            treefmt --config-file=${self}/treefmt.toml \
              --no-cache --fail-on-change --tree-root .
            touch $out
          '';
        clippy = superzej.overrideAttrs (old: {
          pname = "superzej-clippy";
          nativeBuildInputs = (old.nativeBuildInputs or []) ++ [pkgs.clippy];
          buildPhase = "cargo clippy --all-targets --offline -- -D warnings";
          installPhase = "touch $out";
          doCheck = false;
          dontFixup = true;
        });
      };

      # Lean shell for sandboxes/sprites: ONLY what's needed to build + run
      # superzej (`cargo build`, `just build`/`just host`). Deliberately omits the
      # full dev closure — yazi + preview deps, openspec, muse, python+pyte,
      # hyperfine, the lint/format stack — which a build sandbox doesn't need and
      # which dominate the devShell's size (the slow part to seed/fetch on a fresh
      # sprite). Anything missing is one `nix shell nixpkgs#<tool>` away in-pane
      # (see the shellHook). Selected per-sandbox via `[sandbox] devshell =
      # "sandbox"` → `SUPERZEJ_DEVSHELL` → the repo `.envrc`'s `use flake` ref.
      devShells.sandbox = pkgs.mkShell {
        packages = [rustToolchain pkgs.just];
        shellHook = ''
          export PATH="$PWD/target/debug:$PATH"
          echo "superzej sandbox shell (lean: rust + just). Need a tool? Ephemeral:"
          echo "  nix shell nixpkgs#<tool>   |   persistent: nix profile install nixpkgs#<tool>"
        '';
      };

      devShells.default = pkgs.mkShell {
        packages = with pkgs;
          [
            # rust toolchain (clippy/rustfmt/rust-analyzer + wasm32-wasip1 target)
            rustToolchain
            # task runner + formatter (treefmt wrapper with all formatters on PATH)
            just
            treefmtWrapper
            # line-coverage for `just coverage`
            cargo-llvm-cov
            # faster test runner (`just test`) + faster linker (mold, wired via
            # CARGO_TARGET_*_RUSTFLAGS in shellHook below)
            cargo-nextest
            mold
            # compilation cache (RUSTC_WRAPPER in shellHook below): shares crate
            # artifacts across superzej's many cold-target/ worktrees + branch
            # switches. Dev-shell only — the packaged `nix build` never enters here.
            sccache
            # linters
            shellcheck
            yamllint
            taplo
            # dependency gates (`just deps-audit`): advisories/licenses/dupes
            # (cargo-deny) + unused dependencies (cargo-machete)
            cargo-deny
            cargo-machete
            # startup benchmarks (`just bench`)
            hyperfine
            # run the GitHub Actions CI workflow locally in Docker/podman
            # (`just act`). Heavy (each job installs nix in-container); the fast
            # path for local checks is `just ci` / `just lint|test|smoke`.
            act
            # pty visual-regression harnesses (test/*.py reconstruct the screen)
            (python3.withPackages (ps: with ps; [pyte]))
            # runtime tools superzej shells out to
            git
            fzf
            gum
            lazygit
            delta
            gh
            # visual-regression e2e harness (`just e2e`)
            musePkg
            # spec-driven development CLI (`openspec`, `just openspec*`)
            openspec
          ]
          # The same pinned yazi as the package, so the drawer's preview tools
          # resolve on PATH and `just host` runs the version superzej ships.
          ++ [yaziPinned]
          ++ yaziDeps;
        shellHook = ''
          export PATH="$PWD/target/debug:$PATH"
          # Link with mold on the linux-gnu host triple — cuts incremental link
          # time for every cargo invocation (build/clippy/test/coverage), so the
          # pre-push gate and all `nix develop --command just …` CI jobs are
          # cheaper. Scoped to this triple so `check-cross` (macOS/Windows/wasm)
          # is unaffected; the packaged `nix build` never enters this shell.
          export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=-fuse-ld=mold"
          # Compilation cache. sccache reuses per-crate rustc output across cold
          # worktrees / branch switches; it and Cargo incremental are mutually
          # exclusive, so CARGO_INCREMENTAL=0 lets it work (the fast single-crate
          # iterative path is `just quick <crate>`).
          export RUSTC_WRAPPER=sccache
          export CARGO_INCREMENTAL=0
          # Leave headroom so heavy builds don't peg the machine (parallel
          # rustc/codegen jobs); computed here since Nix eval can't see nproc.
          if [ -z "''${CARGO_BUILD_JOBS:-}" ]; then
            _jobs=$(nproc 2>/dev/null || echo 4)
            if [ "$_jobs" -gt 2 ]; then export CARGO_BUILD_JOBS=$((_jobs - 2)); else export CARGO_BUILD_JOBS=1; fi
          fi
          # Point dev superzej at the pinned yazi (the package wires this too).
          export SUPERZEJ_YAZI_BIN="${yaziPinned}/bin/yazi"
          # Spec-driven development (OpenSpec): telemetry off, no host writes.
          export OPENSPEC_TELEMETRY=0 DO_NOT_TRACK=1
          # Seed the Claude Code /opsx commands (gitignored, regenerable) if a
          # fresh worktree lacks them. Cheap; idempotent.
          if [ ! -d .claude/commands/opsx ] && [ -f openspec/config.yaml ]; then
            openspec init --tools claude --profile core --force >/dev/null 2>&1 || true
          fi
          echo "superzej dev shell — 'cargo build', 'just host', 'just smoke', 'nix fmt', 'just openspec'"
        '';
      };
    })
    // {
      # home-manager module. Imported as:
      #   imports = [ inputs.superzej.homeManagerModules.default ];
      homeManagerModules.default = import ./nix/hm-module.nix self;
    };
}
