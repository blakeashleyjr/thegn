{
  description = "thegn — terminal-native git-worktree IDE";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
    # The pinned yazi thegn drives for its bottom file-manager drawer, on its
    # OWN nixpkgs input so thegn bundles a specific yazi (+ its preview tools)
    # independent of the user's system and of the main `nixpkgs`. Bump it
    # deliberately with `nix flake update nixpkgs-yazi`.
    nixpkgs-yazi.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    # Splits dependency compilation into its own derivation, so a change to our
    # own crates reuses the ~600 already-compiled dependencies instead of
    # rebuilding them. With plain `buildRustPackage` everything lives in one
    # derivation, so touching a single line recompiles the entire tree.
    crane.url = "github:ipetkov/crane";
    # The muse e2e harness (`just e2e`) and interactive TUI driver (`muse
    # session` / `muse mcp`). Pinned as a non-flake source and built with the
    # same rust toolchain so `nix develop` and CI run an identical, reproducible
    # muse. Bump deliberately with `nix flake update muse` — the specs require
    # the `feat/agent-session` line (--ci/--artifacts/--case-timeout-ms,
    # `space` key name, kitty/modifyOtherKeys encoding).
    muse = {
      url = "github:blakeashleyjr/muse/14842037b3b14a72beee16c0e9d323342e9fe006";
      flake = false;
    };
    # The pre-commit/pre-push git hooks (`preCommit` below) — the tiered gate
    # that keeps commits cheap and stops broken code leaving the machine.
    #
    # This used to be wired through devenv (`devenv.nix`, deleted), which is a
    # wrapper around THIS SAME project. Consuming it directly is what collapsed
    # the repo back to ONE dev shell: devenv carried its own nixpkgs lock (~6
    # weeks off the flake's) and its own `languages.rust` toolchain (no
    # llvm-tools-preview, no cross rust-std), so the shell a contributor landed
    # in via `.envrc` could not run `just coverage` or `just check-cross`, and
    # the rustfmt gating a commit was a different build from the one `nix fmt`
    # ran. Every CI job runs `nix develop --command just <gate>`, so the flake
    # is the only environment that can be authoritative. Keep it that way — a
    # second shell definition re-opens all of it.
    git-hooks.url = "github:cachix/git-hooks.nix";
    git-hooks.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
    rust-overlay,
    nixpkgs-yazi,
    crane,
    muse,
    git-hooks,
  }:
  # NOT `eachDefaultSystem`: that set still carries `x86_64-darwin`, which the
  # pinned nixpkgs has dropped ("Nixpkgs 26.11 has dropped support for
  # x86_64-darwin"), so every Intel-mac attribute throws on evaluation and takes
  # `nix flake show` / `nix flake check` down with it. Apple silicon only —
  # CONTRIBUTING's macOS notes say the same thing to contributors.
    flake-utils.lib.eachSystem ["x86_64-linux" "aarch64-linux" "aarch64-darwin"] (system: let
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
      # Formatter binaries that treefmt.toml references. Bundled together so the
      # `formatter` wrapper, `checks.formatting` AND the treefmt git hook
      # (`preCommit` below) all use identical versions. This list is the single
      # source: the hook used to carry its own copy in devenv.nix, off a
      # different nixpkgs, which is how `nix fmt` ended up running rustfmt
      # 1.96.1 while the commit hook that gated it ran 1.97.1.
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
      # thegn drives THIS yazi for the file-manager drawer (a private binary
      # via THEGN_YAZI_BIN + a private YAZI_CONFIG_HOME), never the system one.
      yaziPkgs = import nixpkgs-yazi {inherit system;};
      yaziPinned = yaziPkgs.yazi;
      # yazi's preview/runtime deps (fzf + zoxide are already in runtimeDeps).
      # `poppler-utils` (pdftoppm/pdftotext) is selected by attrpath — its hyphen
      # makes it unusable as a bare identifier inside `with`.
      # `unar` is Linux-only here: on aarch64-darwin it fails to link (the
      # cctools `ld` dies with a Trace/BPT trap building XADMaster's `lsar`),
      # and because it is a devShell input that break took the WHOLE dev shell
      # with it — `nix develop` was unenterable on Apple silicon over an
      # optional archive-preview helper. Gate it rather than pin/patch it:
      # without it yazi loses archive previews and nothing else changes.
      yaziDeps =
        (with yaziPkgs; [
          file
          ffmpegthumbnailer
          jq
          fd
          ripgrep
          imagemagick
        ])
        ++ pkgs.lib.optionals pkgs.stdenv.isLinux [yaziPkgs.unar]
        ++ [yaziPkgs.poppler-utils];
      # Allowlisted build source — see nix/source.nix for why it is an
      # allowlist and what has to be on it.
      rootSrc = import ./nix/source.nix {inherit (pkgs) lib;} ./.;
      # The package source is just the filtered repo. This used to splice
      # private `apps/*` submodules in (local flake `self` sources carry only
      # gitlinks), which made the flake unevaluable for anyone without access to
      # those repos and broke `nix profile install github:…/thegn`. Nothing in
      # the workspace depends on them; the submodules are gone. Keep it that way.
      thegnSrc = rootSrc;
      # crane, pinned to the same toolchain everything else uses.
      craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;
      # The dependency tree, compiled ONCE into its own store path. Both channels
      # reuse it, and — the point of the exercise — so does every later build
      # whose Cargo.lock is unchanged: editing our own crates no longer
      # recompiles ~600 dependencies. Must be built from the same args as the
      # package below or cargo invalidates the artifacts.
      cargoCommonArgs = {
        src = thegnSrc;
        pname = "thegn";
        version = "0.1.0";
        nativeBuildInputs = [pkgs.pkg-config];
        buildInputs = [pkgs.zlib];
        cargoExtraArgs = "-p thegn-host --bin thegn";
        doCheck = false;
      };
      cargoArtifacts = craneLib.buildDepsOnly cargoCommonArgs;
      thegn = pkgs.callPackage ./nix/package.nix {
        src = thegnSrc;
        yazi = yaziPinned;
        inherit yaziDeps craneLib cargoArtifacts;
      };

      # The dev release channel (`nix build .#dev`): same source, built with the
      # host `dev` Cargo feature so experimental subsystems (remotes, AI/proxy,
      # observe, placement, non-GitHub trackers) are enabled instead of clamped
      # off. Installs as `thegn-dev`/`tg-dev`, so it coexists with a stable
      # install. `THEGN_CHANNEL=stable` still forces either binary to stable.
      thegnDev = pkgs.callPackage ./nix/package.nix {
        src = thegnSrc;
        yazi = yaziPinned;
        inherit yaziDeps craneLib cargoArtifacts;
        channel = "dev";
      };

      # The OpenSpec CLI thegn uses for spec-driven development of itself.
      # A hermetic, pinned build (see nix/openspec.nix) — no global npm install,
      # telemetry off — shared by the dev shell and `just openspec*`.
      openspec = pkgs.callPackage ./nix/openspec.nix {};

      # One rust-overlay toolchain (clippy/rustfmt/rust-analyzer included).
      rustToolchain = pkgs.rust-bin.stable.latest.default.override {
        # llvm-tools for `cargo llvm-cov` (just coverage). rust-src is NOT in the
        # `default` profile, and without it rust-analyzer has no stdlib sources
        # (no std go-to-definition, no completions inside core/alloc) — devenv's
        # `languages.rust` used to supply it via RUST_SRC_PATH, so it has to be
        # declared here now that the flake shell is the only one.
        extensions = ["llvm-tools-preview" "rust-src"];
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
      # Trimmed Linux-only toolchain for the `sprite-full` devShell. Same stable
      # rustc/cargo/clippy/rustfmt/llvm-tools COMPONENTS as `rustToolchain` (so
      # they share cache paths — a `sprite-full` fetch reuses `.#default`'s toolchain
      # store paths) but from the `minimal` profile, so it drops `rust-docs` AND the
      # darwin/windows cross-target `rust-std` sets — dead weight on a Linux sprite
      # that bloat the closure (both cross `rust-std` sets + docs were in the
      # from-scratch "wall of text"). `just check-cross` can't run on a sprite as a
      # result — by design.
      # The MSRV toolchain (`rust-version` in Cargo.toml), exposed as `cargo-1.89`
      # so `just check-msrv` is hermetic: no rustup, no network. `minimal` keeps
      # the closure small (it only needs to typecheck). Bump both together.
      msrvRustToolchain = pkgs.rust-bin.stable."1.89.0".minimal;
      msrvCargo = pkgs.writeShellScriptBin "cargo-1.89" ''
        exec ${msrvRustToolchain}/bin/cargo "$@"
      '';
      spriteRustToolchain = pkgs.rust-bin.stable.latest.minimal.override {
        extensions = ["clippy" "rustfmt" "llvm-tools-preview"];
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

      # Static x86_64-linux-musl `thegn` — the resident bridge agent pushed into
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
      thegnMusl = muslRustPlatform.buildRustPackage {
        pname = "thegn-musl";
        version = "0.1.0";
        src = thegnSrc;
        cargoLock.lockFile = ./Cargo.lock;
        # Force the musl target explicitly (env-only CARGO_BUILD_TARGET was being
        # overridden by buildRustPackage → a glibc host binary that can't run in a
        # bare microVM). `+crt-static` makes it fully static (no ld-musl loader
        # needed in the sandbox). Install from the cross target dir, not target/release.
        cargoBuildFlags = ["-p" "thegn-host" "--bin" "thegn" "--target" muslTarget];
        CARGO_BUILD_RUSTFLAGS = "-C target-feature=+crt-static";
        CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER = muslCc;
        CC_x86_64_unknown_linux_musl = muslCc;
        nativeBuildInputs = [muslCross.stdenv.cc];
        doCheck = false;
        installPhase = ''
          runHook preInstall
          mkdir -p $out/bin
          cp target/${muslTarget}/release/thegn $out/bin/thegn
          runHook postInstall
        '';
      };

      # `nix build .#default` / `nix profile install .#default` ships the host
      # `thegn` AND — on x86_64-linux — the static-musl `thegn-musl` bridge beside
      # it, so `bridge_sup::bridge_binary_path()` auto-discovers the bridge from ANY
      # install location (not just a dev symlink that happens to resolve into the
      # build tree, where `just bridge` drops it). Without an adjacent bridge,
      # host_cache/revtunnel silently no-op and sprites build the devShell from
      # source. Gated to x86_64-linux: the bridge is an x86_64-linux-musl artifact
      # (sprites are x86_64-linux, driven from an x86_64-linux host); cross-building
      # it from darwin/aarch64 isn't supported.
      defaultPkg =
        if system == "x86_64-linux"
        then
          thegn.overrideAttrs (old: {
            postInstall =
              (old.postInstall or "")
              + ''
                install -Dm755 ${thegnMusl}/bin/thegn $out/bin/thegn-musl
              '';
          })
        else thegn;

      # mingw-w64 cross C toolchain for `just check-cross`'s whole-workspace
      # windows-gnu leg. `cargo check` still runs build scripts, and libz-sys /
      # libgit2-sys compile C, so without a cross cc the check dies with
      # "implicit declaration of function 'lseek'".
      #
      # This used to live ONLY in devenv.nix, which meant `just check-cross`
      # passed in a devenv shell and failed under a bare `nix develop` — exactly
      # what CI runs (`nix develop --command just check-cross`), so the gate broke
      # the moment it moved to a runner without devenv (`704eee77`). There is now
      # one shell, so there is nothing left to keep in sync.
      mingwCrossEnv = pkgs.lib.optionalString pkgs.stdenv.isLinux ''
        export CC_x86_64_pc_windows_gnu="${pkgs.pkgsCross.mingwW64.stdenv.cc}/bin/x86_64-w64-mingw32-cc"
        export CXX_x86_64_pc_windows_gnu="${pkgs.pkgsCross.mingwW64.stdenv.cc}/bin/x86_64-w64-mingw32-c++"
        export AR_x86_64_pc_windows_gnu="${pkgs.pkgsCross.mingwW64.stdenv.cc}/bin/x86_64-w64-mingw32-ar"
        export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="${pkgs.pkgsCross.mingwW64.stdenv.cc}/bin/x86_64-w64-mingw32-cc"
      '';

      # ── git hooks ────────────────────────────────────────────────────────────
      # The tiered local gate. Generates the gitignored `.pre-commit-config.yaml`
      # store symlink and installs the prek stubs into the effective hooks dir;
      # `preCommit.shellHook` (wired into devShells.default below) does both on
      # shell entry. Ported from devenv.nix — same upstream module, one nixpkgs.
      #
      # NOT exposed in `checks`: git-hooks.nix's `run` derivation copies the whole
      # source tree, which is exactly the rebuild-on-any-file-change cost
      # nix/source.nix exists to prevent, and the pre-push hooks (clippy, `just
      # test`, `just smoke`) can't run in a network-less sandboxed build anyway.
      # `just ci` gates via `nix-build` (= `nix build .#thegn-nobridge`).
      preCommit = git-hooks.lib.${system}.run {
        src = ./.;
        # prek, not the Python `pre-commit` (which is the module's default and
        # what upstream now recommends migrating off). devenv selected prek, the
        # installed stubs in .git/hooks are prek's, and PREK_ALLOW_NO_CONFIG in
        # `hookExtras` / crates/thegn-core/src/sandbox.rs only means anything to
        # prek — so this keeps the hooks byte-comparable across the migration.
        package = pkgs.prek;
        # cargo/clippy come from `rustToolchain`, not nixpkgs. devenv got them
        # from `languages.rust` — a different toolchain from the one the shell
        # built with, so the hook's clippy disagreed with `just lint`'s.
        tools = {
          cargo = rustToolchain;
          clippy = rustToolchain;
          inherit (pkgs) rustfmt;
        };

        # Run the suite on merges too, not just plain commits. A clean auto-merge
        # of two individually-valid branches can produce a semantically broken
        # tree (e.g. one branch changes a fn signature while another adds a
        # now-stale call site — different files, so no text conflict, so the merge
        # succeeds). `git merge` fires `pre-merge-commit`, NOT `pre-commit`, so
        # without this the merge result is never linted. Listing pre-merge-commit
        # is also what makes the module install that hook.
        default_stages = ["pre-commit" "pre-merge-commit"];

        hooks = {
          # formatting — delegate ALL formatters to treefmt via treefmt.toml, the
          # single formatter config shared with `nix fmt` (the flake formatter).
          # `fmtPackages` is that shared list, so the hook and `nix fmt` cannot
          # drift in formatter versions.
          treefmt = {
            enable = true;
            settings.formatters = fmtPackages;
          };

          # linters — these are checks, not formatters, so they stay separate
          # hooks. shellcheck/yamllint are cheap + staged-file, so they stay on
          # pre-commit. clippy compiles the whole workspace, so it moves to
          # pre-push (see the tiering note below).
          clippy = {
            enable = true;
            stages = ["pre-push"];
            # `--offline` is the module's default but NOT what this repo ran
            # under devenv, and it makes the gate fail for a network reason
            # (unfetched registry entry) right after a dependency is added.
            # `just lint` / CI run plain `cargo clippy`, so match them.
            settings.offline = false;
          };
          shellcheck.enable = true;
          yamllint.enable = true;

          # ── Tiered gates ────────────────────────────────────────────────────
          # pre-commit stays CHEAP (formatting + shell/yaml lint) so commits are
          # near-instant. The correctness gates — clippy, the full test suite, and
          # smoke — run on pre-push (before code leaves the machine) and in CI via
          # `just ci`. This defers the semantic-merge check (a stale call site
          # across a clean auto-merge) from merge time to push time; it is still
          # caught before the merge is pushed, and always by CI.
          #
          # Coverage (`cargo llvm-cov`) is NOT on pre-push: it is an instrumented
          # full recompile into a separate target dir (the single heaviest gate)
          # and CI re-runs it anyway. It stays a CI-only gate via `just ci`. Run
          # it locally on demand with `just coverage` before opening a PR.
          #
          # git hooks run with GIT_DIR and GIT_INDEX_FILE set. This leaks into the
          # git subprocesses spawned by `cargo test`, causing spurious failures in
          # repository manipulation tests. Strip them via `env -u` so tests run in
          # a clean git environment. Likewise drop THEGN_SANDBOX: committing from a
          # shell running inside a live thegn bwrap sandbox leaks the =1 marker
          # into the runner and false-fails the sandbox argv tests. `just test`
          # runs cargo-nextest — one source of truth with CI. The doctest pass
          # is `just test-doc`, deliberately CI-only: it is a third
          # full-workspace compile and the repo has no runnable doctests, so
          # paying for it on every push bought nothing.
          cargo-test = {
            enable = true;
            name = "cargo test";
            entry = "env -u GIT_DIR -u GIT_INDEX_FILE -u GIT_WORK_TREE -u GIT_COMMON_DIR -u GIT_NAMESPACE -u GIT_OBJECT_DIRECTORY -u GIT_ALTERNATE_OBJECT_DIRECTORIES -u THEGN_SANDBOX just test";
            language = "system";
            pass_filenames = false;
            stages = ["pre-push"];
          };
          smoke = {
            enable = true;
            name = "smoke (hermetic CLI verbs)";
            # Same `env -u` scrub as the cargo-test hook above, and for a sharper
            # reason here. Hooks run with GIT_DIR/GIT_INDEX_FILE set, and smoke
            # builds its own throwaway repos — inheriting those points its git at
            # the REAL one. From the canonical checkout that merely made the
            # fixtures non-hermetic; from a linked worktree it tries to
            # force-update a `main` that is checked out elsewhere, and git
            # refuses:
            #
            #   fatal: cannot force update the branch 'main' used by worktree at …
            #
            # which failed `git push` for every worktree, i.e. exactly the
            # workflow this repo is built around.
            #
            # The scrub must be the WHOLE set (`util::GIT_ENV_VARS`), not just
            # the two obvious names: git rejects `GIT_WORK_TREE` without a
            # `GIT_DIR`, so removing half the pair swaps one failure for another
            #
            #   fatal: GIT_WORK_TREE not allowed without specifying GIT_DIR
            #
            # — which is exactly what a partial fix produced here.
            entry = "env -u GIT_DIR -u GIT_INDEX_FILE -u GIT_WORK_TREE -u GIT_COMMON_DIR -u GIT_NAMESPACE -u GIT_OBJECT_DIRECTORY -u GIT_ALTERNATE_OBJECT_DIRECTORIES -u THEGN_SANDBOX just smoke";
            language = "system";
            pass_filenames = false;
            stages = ["pre-push"];
          };
        };
      };

      # The two hook-adjacent bits of shell that git-hooks.nix itself doesn't do.
      # Skipped on CI: every job runs `nix develop --command just <gate>` in a
      # throwaway checkout whose hooks are never fired, so installing them there
      # is pure noise.
      hookExtras = ''
        if [ -z "''${CI:-}" ]; then
          # Backstop for a missing config: if a worktree's gitignored
          # .pre-commit-config.yaml symlink is absent or dangling (the
          # post-checkout seed hasn't run, or a flake re-lock left it pointing at
          # a gone store path), let prek SKIP its hooks rather than abort the
          # commit. Harmless where the config is present — prek runs the hooks as
          # usual; the real gate is pre-push (clippy/test/smoke). Mirrors the
          # PREK_ALLOW_NO_CONFIG injected into thegn sandboxes
          # (crates/thegn-core/src/sandbox.rs).
          export PREK_ALLOW_NO_CONFIG=1

          # Install the post-checkout hook into the effective (shared) hooks dir
          # so the prek hooks work in EVERY worktree. prek needs
          # .pre-commit-config.yaml in each worktree root, but the store symlink
          # is only materialized in the checkout where the shell is entered; the
          # hook seeds it into every other worktree on `git worktree add`. Copied
          # (not symlinked) so it doesn't depend on any one worktree's path, and
          # refreshed on every entry so it self-heals. See
          # test/git-hooks/post-checkout.sh.
          hooks_dir=$(git config core.hooksPath 2>/dev/null || true)
          [ -n "$hooks_dir" ] || hooks_dir=$(git rev-parse --git-common-dir 2>/dev/null)/hooks
          if [ -d "$hooks_dir" ] && [ -f test/git-hooks/post-checkout.sh ]; then
            install -m 0755 test/git-hooks/post-checkout.sh "$hooks_dir/post-checkout"
          fi
          unset hooks_dir
        fi
      '';

      # Shared dev-shell shellHook (mold linker, sccache on a per-mount-ns socket,
      # CARGO_BUILD_JOBS headroom, pinned yazi, OpenSpec seeding). Used by BOTH the
      # full `default` shell and the trimmed `sprite-full` shell so they never drift.
      devShellHook = ''
        export PATH="$PWD/target/debug:$PATH"
        # Point pkg-config at the nix zlib/openssl .pc files. Without this,
        # PKG_CONFIG_PATH is empty and pkg-config falls back to its host
        # default search path (/usr/lib/.../pkgconfig). On hosts whose
        # /usr/lib/pkgconfig advertises `includedir=/usr/include` (e.g. Sprite
        # microVMs), libz-sys/openssl-sys then export that path and libgit2-sys
        # compiles its vendored C with `-I /usr/include`, dragging in the host
        # glibc headers ahead of nix's — which fails outright when the host
        # /usr/include is a partial dev tree (missing bits/types/once_flag.h).
        # Prepending the nix .pc dirs makes the nix libs win; LIBZ_SYS_STATIC
        # vendors zlib as a belt-and-suspenders. Hermetic on every host.
        # (nix ships zlib's .pc under share/pkgconfig, openssl's under lib/pkgconfig.)
        export PKG_CONFIG_PATH="${pkgs.zlib.dev}/lib/pkgconfig:${pkgs.zlib.dev}/share/pkgconfig:${pkgs.openssl.dev}/lib/pkgconfig''${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
        export LIBZ_SYS_STATIC=1
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
        # Bound the sccache cache so it can't creep unbounded on the dev box
        # (the compile cache is disk; a full fs makes target/ writes fail with
        # ENOSPC/EROFS mid-build). Honor an already-set value.
        export SCCACHE_CACHE_SIZE="''${SCCACHE_CACHE_SIZE:-20G}"
        # Pin the sccache server to a per-mount-namespace socket. thegn
        # development happens inside sandboxes (bwrap; the AI agent's own
        # bwrap) that bind-mount this worktree writable into a fresh mount
        # namespace. sccache's default server is a long-lived daemon reached
        # over the shared loopback endpoint — so a sandbox reuses a server
        # left over from a *different* (often now-defunct) namespace whose
        # view of this worktree's target/ is stale/read-only, and every
        # compile dies with "Read-only file system (os error 30)". Keying the
        # server socket to our mount-namespace inode gives each namespace its
        # own server (lazily spawned here, where target/ is writable) and
        # never contacts a foreign one. Guarded: only when sccache is present,
        # neither endpoint var is already set (CI wires its own), and /proc is
        # available (skips non-Linux). The short /tmp path stays under the
        # AF_UNIX SUN_LEN (~108) limit.
        if command -v sccache >/dev/null 2>&1 \
          && [ -z "''${SCCACHE_SERVER_UDS:-}" ] && [ -z "''${SCCACHE_SERVER_PORT:-}" ]; then
          _mnt_ns=$(readlink /proc/self/ns/mnt 2>/dev/null | tr -dc '0-9')
          if [ -n "$_mnt_ns" ]; then
            export SCCACHE_SERVER_UDS="/tmp/sccache-$(id -u)-$_mnt_ns.sock"
          fi
          unset _mnt_ns
        fi
        # Leave headroom so heavy builds don't peg the machine (parallel
        # rustc/codegen jobs); computed here since Nix eval can't see nproc.
        #
        # Capped at 8 rather than `nproc - 2`. This is a WORKTREE-oriented tool
        # and several worktrees build at once, so the old rule handed EACH of
        # them all-but-two cores: three concurrent `just test` runs meant ~66
        # rustc on a 24-core box, which is how the machine ended up pinned at
        # 100% with the desktop starved. At 8, two concurrent worktrees sum to
        # the `[sandbox.limits] cpu_total` ceiling instead of each claiming it.
        # Still overridable for a deliberate one-off: `CARGO_BUILD_JOBS=20 just build`.
        if [ -z "''${CARGO_BUILD_JOBS:-}" ]; then
          _jobs=$(nproc 2>/dev/null || echo 4)
          [ "$_jobs" -gt 8 ] && _jobs=8
          [ "$_jobs" -lt 1 ] && _jobs=1
          export CARGO_BUILD_JOBS="$_jobs"
        fi
        # Point dev thegn at the pinned yazi (the package wires this too).
        export THEGN_YAZI_BIN="${yaziPinned}/bin/yazi"
        # Spec-driven development (OpenSpec): telemetry off, no host writes.
        export OPENSPEC_TELEMETRY=0 DO_NOT_TRACK=1
        # Seed the Claude Code /opsx commands (gitignored, regenerable) if a
        # fresh worktree lacks them. Cheap; idempotent.
        if [ ! -d .claude/commands/opsx ] && [ -f openspec/config.yaml ]; then
          openspec init --tools claude --profile core --force >/dev/null 2>&1 || true
        fi
        # Quiet podman→docker compatibility (DOCKER_HOST + guarded ~/.docker
        # self-heal). Read-only tolerant so the sandbox's read-only /home bind
        # never turns it into "ln: … Read-only file system" noise. Kept in its own
        # file so the `sprite-full` shell (which reuses this hook) shares one impl.
        source ${./nix/dev-docker-shim.sh}
        echo "thegn dev shell — 'cargo build', 'just host', 'just smoke', 'nix fmt', 'just openspec'"
      '';
    in {
      packages =
        {
          default = defaultPkg;
          thegn = defaultPkg;
          # The host binary WITHOUT the adjacent x86_64-linux musl bridge.
          #
          # `default` builds the workspace twice on x86_64-linux — once natively
          # and once cross-compiled to musl for `thegn-musl` — so it costs
          # roughly double what the shipped stable binary costs. The bridge only
          # serves provider microVMs, which are dev-channel-only, so the routine
          # CI gate builds this instead (`just nix-build`) and the full install
          # is verified on demand (`just nix-build-full`, and before a release).
          thegn-nobridge = thegn;
          # The dev release channel (`nix build .#dev` / `nix run .#dev`): the
          # same host with experimental subsystems enabled, as `thegn-dev`.
          dev = thegnDev;
          thegn-dev = thegnDev;
          # The pinned yazi thegn drives for the file-manager drawer.
          yazi = yaziPinned;
          # The muse e2e harness (`nix run .#muse`, also on the dev-shell PATH).
          muse = musePkg;
          # The OpenSpec CLI for spec-driven development (`nix run .#openspec`).
          openspec = openspec;
        }
        # Linux-only artifacts. Not merely unbuildable on darwin — the two images
        # fail to EVALUATE there (`shadow`/`procps` refuse a darwin hostPlatform),
        # which would take `nix flake show` / `nix flake check` down with them on a
        # Mac. The musl bridge is an x86_64-linux artifact cross-built from a Linux
        # host; see `defaultPkg` above.
        // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
          # Static musl bridge binary (`nix build .#thegn-musl`) — pushed into
          # provider microVMs as the resident agent (8-B.3).
          thegn-musl = thegnMusl;
          # The multi-arch base sandbox image (per-arch; `just image-build` loads
          # it locally, CI pushes both arches + a manifest list).
          sandbox-image = import ./nix/sandbox-image.nix {pkgs = imagePkgs;};
          # Fly.io boot image: sshd entrypoint + baked toolchain, so a Fly machine
          # boots straight into a reachable shell (`template = "image:<ref>"`).
          fly-sandbox-image = import ./nix/fly-sandbox-image.nix {inherit pkgs rustToolchain;};
        };

      # `nix fmt` formats every tracked file via treefmt.toml.
      formatter = treefmtWrapper;

      checks = {
        # `nix flake check` gates on a clean build, formatting, and clippy.
        build = thegn;
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
        clippy = thegn.overrideAttrs (old: {
          pname = "thegn-clippy";
          nativeBuildInputs = (old.nativeBuildInputs or []) ++ [pkgs.clippy];
          buildPhase = "cargo clippy --all-targets --offline -- -D warnings";
          installPhase = "touch $out";
          doCheck = false;
          dontFixup = true;
        });
      };

      # Lean shell for sandboxes/sprites: ONLY what's needed to build + run
      # thegn (`cargo build`, `just build`/`just host`). Deliberately omits the
      # full dev closure — yazi + preview deps, openspec, muse, python,
      # hyperfine, the lint/format stack — which a build sandbox doesn't need and
      # which dominate the devShell's size (the slow part to seed/fetch on a fresh
      # sprite). Anything missing is one `nix shell nixpkgs#<tool>` away in-pane
      # (see the shellHook). Selected per-sandbox via `[sandbox] devshell =
      # "sandbox"` → `THEGN_DEVSHELL` → the repo `.envrc`'s `use flake` ref.
      devShells.sandbox = pkgs.mkShell {
        # zsh so a sandbox/sprite pane (which enters THIS devShell via
        # THEGN_DEVSHELL=sandbox) has a real login shell — the pane's shell probe
        # finds it instead of dropping to a bare `/bin/sh`.
        packages = [rustToolchain pkgs.just pkgs.zsh];
        shellHook = ''
          export PATH="$PWD/target/debug:$PATH"
          echo "thegn sandbox shell (lean: rust + just). Need a tool? Ephemeral:"
          echo "  nix shell nixpkgs#<tool>   |   persistent: nix profile install nixpkgs#<tool>"
        '';
      };

      # Lean shell for CI jobs that only build + test (today: the macOS gate).
      # The full devShell pulls openspec, whose `pnpm install` was OOM-killed on
      # the 7 GB macOS runner — that failure is the whole reason the macOS job
      # was disabled, and it was building a tool the job never invokes. This
      # carries only what `just build` and `just test` actually use, so the job
      # both passes and gets cheaper. Keep it in sync with those two recipes.
      devShells.ci = pkgs.mkShell {
        packages = [
          rustToolchain
          pkgs.just
          # `just test` runs `cargo nextest run --workspace`.
          pkgs.cargo-nextest
          # libgit2-sys → libz-sys wants pkg-config + zlib; sqlite is vendored.
          pkgs.pkg-config
          pkgs.zlib
        ];
      };

      devShells.default = pkgs.mkShell {
        packages = with pkgs;
          [
            # rust toolchain (clippy/rustfmt/rust-analyzer + wasm32-wasip1 target)
            rustToolchain
            # `cargo-1.89`: the pinned MSRV toolchain behind `just check-msrv`
            msrvCargo
            # task runner + formatter (treefmt wrapper with all formatters on PATH)
            just
            treefmtWrapper
            # line-coverage for `just coverage`
            cargo-llvm-cov
            # faster test runner (`just test`)
            cargo-nextest
            # compilation cache (RUSTC_WRAPPER in shellHook below): shares crate
            # artifacts across thegn's many cold-target/ worktrees + branch
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
            # smoke.sh validates `--json` output with python's json module
            python3
            # a login shell for sandbox panes: thegn injects this devShell's PATH
            # into bwrap/OCI panes (which ship no zsh of their own), so the pane's
            # shell probe finds zsh instead of dropping to a bare `/bin/sh`.
            zsh
            # runtime tools thegn shells out to
            git
            fzf
            gum
            lazygit
            delta
            gh
            # the e2e harness (`just e2e`) and the interactive TUI driver (`muse session`)
            musePkg
            # spec-driven development CLI (`openspec`, `just openspec*`)
            openspec
          ]
          # The same pinned yazi as the package, so the drawer's preview tools
          # resolve on PATH and `just host` runs the version thegn ships.
          ++ [yaziPinned]
          ++ yaziDeps
          # Faster linker, wired via CARGO_TARGET_*_RUSTFLAGS in shellHook
          # below. Linux-only in nixpkgs — gate it so the shell evaluates on
          # macOS (where the default ld64 is used instead).
          ++ pkgs.lib.optionals pkgs.stdenv.isLinux [pkgs.mold]
          # prek + the hook binaries (see `preCommit` above). ONLY the default
          # shell installs git hooks — sandbox/sprite panes run inside a
          # read-only /nix with PREK_ALLOW_NO_CONFIG already injected, and must
          # never reach out to install hooks in the host's shared hooks dir.
          ++ preCommit.enabledPackages;
        # hookExtras runs AFTER preCommit.shellHook: git-hooks.nix rewrites
        # core.hooksPath as its last act, and the post-checkout installer has to
        # land in whatever dir it settled on.
        shellHook = devShellHook + mingwCrossEnv + preCommit.shellHook + hookExtras;
      };

      # Trimmed "full replica" shell for sprites / provider sandboxes: everything
      # `just ci` needs MINUS the weight that dominates the closure on a fresh
      # Firecracker microVM — rust-docs + darwin/windows cross-targets (via
      # `spriteRustToolchain`), the `muse` e2e harness (a from-source Rust compile),
      # `act`, `hyperfine`, and `python`. Keeps the full lint/format/coverage
      # stack, openspec, yazi + preview deps, and the git tooling thegn shells out
      # to. Selected via `[env.sprites.sandbox] devshell = "sprite-full"` →
      # THEGN_DEVSHELL → the repo `.envrc`'s `use flake` ref. `just check-cross` /
      # `just e2e` can't run here (no cross targets / no muse) — by design; a missing
      # tool is one `nix shell nixpkgs#<tool>` away in-pane. Reuses `devShellHook`
      # (sccache/mold/yazi/openspec) so it never drifts from `default`.
      devShells.sprite-full = pkgs.mkShell {
        packages = with pkgs;
          [
            spriteRustToolchain
            just
            treefmtWrapper
            cargo-llvm-cov
            cargo-nextest
            sccache
            shellcheck
            yamllint
            taplo
            cargo-deny
            cargo-machete
            zsh
            git
            fzf
            gum
            lazygit
            delta
            gh
            openspec
          ]
          ++ [yaziPinned]
          ++ yaziDeps
          ++ pkgs.lib.optionals pkgs.stdenv.isLinux [pkgs.mold];
        shellHook = devShellHook;
      };
    })
    // {
      # home-manager module — installs thegn AND renders ~/.config/thegn/config.toml
      # from typed options. Imported as:
      #   imports = [ inputs.thegn.homeManagerModules.default ];
      homeManagerModules.default = import ./nix/hm-module.nix self;
      # nix-darwin module — installs the binary system-wide. Deliberately thin:
      # nix-darwin has no per-user config-file mechanism, so configuration stays
      # home-manager's job (the two compose). Imported as:
      #   imports = [ inputs.thegn.darwinModules.default ];
      darwinModules.default = import ./nix/darwin-module.nix self;
    };
}
