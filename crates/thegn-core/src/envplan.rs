//! Backend-agnostic environment provisioning.
//!
//! The north star (see the `transparent-env-provisioning` design): a sandbox or
//! remote should reach a *working* dev environment by reproducing the **same
//! declarations that make `local` work** — `flake.nix`/`devenv.nix` + `.envrc`,
//! or `.tool-versions`/language manifests — not via a per-provider recipe. This
//! module is the substrate-agnostic, pure half:
//!
//!   1. [`detect`] reads a worktree's common env-defining files → [`EnvRequirements`].
//!   2. [`plan`] compiles those into an ordered, fidelity-tiered [`EnvPlan`] of
//!      [`ProvisionStep`]s (clone, install Nix, activate the devShell, sync
//!      dotfiles, checkpoint).
//!
//! The host applies the plan to a placement via its exec/fs APIs (the only
//! backend-specific seam). Everything here is pure + unit-tested.

use std::fmt::Write as _;
use std::path::Path;

use crate::toolchain_activation::DetectedToolchainFiles;
use crate::util::sh_quote;

/// What a repo declares about its dev environment, gleaned from common files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvRequirements {
    /// `flake.nix` exists and mentions a `devShell` (Nix flake dev environment).
    pub nix_flake_devshell: bool,
    /// `devenv.nix` exists (devenv.sh dev environment).
    pub devenv: bool,
    /// `.envrc` exists (direnv activation).
    pub direnv: bool,
    /// `.envrc` uses `use flake` (so direnv activates the flake devShell).
    pub direnv_uses_flake: bool,
    /// `.tool-versions` (asdf) or a `mise` config — version-pinned toolchains.
    pub tool_versions: bool,
    /// Normalized, deterministic mise/asdf config and pin files. This retains
    /// the names (never their contents) for trust/cache identity construction.
    pub toolchain_files: DetectedToolchainFiles,
    /// Classic (non-flake) Nix: `shell.nix` or `default.nix`, with no flake
    /// devShell / devenv outranking it. Folded into [`Tier::Nix`]; the devShell
    /// warm uses `nix-shell --run true` instead of `nix develop`.
    pub nix_classic: bool,
    /// Language ecosystems detected from manifests (for the non-Nix tiers).
    pub languages: Vec<Language>,
    /// A `devcontainer.json` is present. Presence-only: the full parse needs
    /// the file *contents* (which the remote probe can't emit), so callers use
    /// this as a signal to read + parse via [`crate::devcontainer`]. Does not
    /// affect [`EnvRequirements::tier`] — a devcontainer sources the image /
    /// lifecycle *before* provisioning; the in-container env still tiers
    /// normally.
    pub devcontainer: bool,
}

/// A language ecosystem detected from a manifest file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Node,
    Python,
    Rust,
    Go,
    Ruby,
    Deno,
    Jvm,
}

impl EnvRequirements {
    /// The highest-fidelity tier this repo supports.
    pub fn tier(&self) -> Tier {
        if self.nix_flake_devshell || self.devenv || self.nix_classic {
            Tier::Nix
        } else if self.tool_versions {
            Tier::ToolVersions
        } else if !self.languages.is_empty() {
            Tier::SynthNix
        } else {
            Tier::Bare
        }
    }
}

/// Provisioning fidelity tiers, best → fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Nix flake/devenv/classic present — install Nix, reproduce the devShell.
    Nix,
    /// `.tool-versions`/mise — install mise and `mise install`.
    ToolVersions,
    /// Only language manifests — synthesize a Nix devShell for the detected
    /// languages (batteries included). [`plan`] downgrades this to
    /// [`Tier::ToolVersions`] (mode `mise`) or [`Tier::Languages`] (mode `off`
    /// or Nix disallowed) per `[toolchain]`.
    SynthNix,
    /// Only language manifests — best-effort native runtimes (apt).
    Languages,
    /// Nothing declared — just the repo + a shell.
    Bare,
}

/// Detect a worktree's environment requirements from the files it ships. Pure
/// over the filesystem: reads a handful of well-known paths, never executes.
/// The remote-detection probe: emits the same facts [`detect`] reads from the
/// local fs as `KEY=VALUE` lines, for worktrees that live on a HOST (ssh envs)
/// where the repo files aren't local. Run with `cd <workdir> && sh -c ...`;
/// parse with [`detect_from_probe`]. Extend both together.
pub const DETECT_PROBE_SCRIPT: &str = r#"
[ -f flake.nix ] && grep -Eq 'devShells?' flake.nix && echo FLAKE_DEVSHELL=1
[ -f devenv.nix ] && echo DEVENV=1
[ -f .envrc ] && echo DIRENV=1
[ -f .envrc ] && grep -Eq 'use[ _]flake' .envrc && echo DIRENV_FLAKE=1
emit_toolchain_file() {
  kind="$1"; file="$2"
  no_symlink_components "$file" || return 0
  [ -f "$file" ] && [ -r "$file" ] && [ ! -L "$file" ] || return 0
  printf 'TOOLCHAIN_%s=%s\n' "$kind" "$file"
}
no_symlink_components() {
  path="$1"
  while [ "$path" != "." ] && [ "$path" != "/" ] && [ -n "$path" ]; do
    [ ! -L "$path" ] || return 1
    case "$path" in
      */*) path="${path%/*}"; [ -n "$path" ] || path="." ;;
      *) path="." ;;
    esac
  done
}
for file in mise.toml .mise.toml mise.local.toml mise/config.toml .mise/config.toml .config/mise.toml .config/mise/config.toml; do
  emit_toolchain_file CONFIG "$file"
done
if [ -d conf.d ] && [ ! -L conf.d ]; then
  for file in conf.d/*.toml; do
    [ -e "$file" ] || continue
    emit_toolchain_file CONFIG "$file"
  done
fi
case "${MISE_ENV:-}" in
  ''|*[!A-Za-z0-9_-]*) ;;
  *) emit_toolchain_file CONFIG "mise.${MISE_ENV}.toml" ;;
esac
for file in .tool-versions .nvmrc .node-version .python-version .ruby-version .go-version .java-version; do
  emit_toolchain_file PIN "$file"
done
{ [ -f shell.nix ] || [ -f default.nix ]; } && echo NIX_CLASSIC=1
[ -f package.json ] && echo LANG_NODE=1
{ [ -f pyproject.toml ] || [ -f requirements.txt ] || [ -f setup.py ]; } && echo LANG_PYTHON=1
[ -f Cargo.toml ] && echo LANG_RUST=1
[ -f go.mod ] && echo LANG_GO=1
[ -f Gemfile ] && echo LANG_RUBY=1
{ [ -f deno.json ] || [ -f deno.jsonc ]; } && echo LANG_DENO=1
{ [ -f build.sbt ] || [ -f project/build.properties ] || [ -f .scala-version ]; } && echo LANG_JVM=1
{ [ -f .devcontainer/devcontainer.json ] || [ -f .devcontainer.json ] || find .devcontainer -mindepth 2 -maxdepth 2 -type f -name devcontainer.json -print -quit 2>/dev/null | grep -q .; } && echo DEVCONTAINER=1
true
"#;

/// Parse [`DETECT_PROBE_SCRIPT`] output into the same [`EnvRequirements`]
/// shape [`detect`] produces locally. Unknown keys are ignored.
pub fn detect_from_probe(out: &str) -> EnvRequirements {
    let has = |k: &str| out.lines().any(|l| l.trim() == format!("{k}=1"));
    let toolchain_files = DetectedToolchainFiles::from_probe(out);
    let mut languages = Vec::new();
    for (key, lang) in [
        ("LANG_NODE", Language::Node),
        ("LANG_PYTHON", Language::Python),
        ("LANG_RUST", Language::Rust),
        ("LANG_GO", Language::Go),
        ("LANG_RUBY", Language::Ruby),
        ("LANG_DENO", Language::Deno),
        ("LANG_JVM", Language::Jvm),
    ] {
        if has(key) {
            languages.push(lang);
        }
    }
    EnvRequirements {
        nix_flake_devshell: has("FLAKE_DEVSHELL"),
        devenv: has("DEVENV"),
        direnv: has("DIRENV"),
        direnv_uses_flake: has("DIRENV_FLAKE"),
        tool_versions: has("TOOL_VERSIONS") || !toolchain_files.is_empty(),
        toolchain_files,
        nix_classic: has("NIX_CLASSIC"),
        languages,
        devcontainer: has("DEVCONTAINER"),
    }
}

pub fn detect(worktree: &Path) -> EnvRequirements {
    let mise_env = std::env::var("MISE_ENV").ok();
    detect_with_mise_env(worktree, mise_env.as_deref())
}

/// Deterministic variant of [`detect`] for callers that already captured the
/// target's ambient `MISE_ENV` (and for tests that must not mutate process env).
pub fn detect_with_mise_env(worktree: &Path, mise_env: Option<&str>) -> EnvRequirements {
    let read = |name: &str| std::fs::read_to_string(worktree.join(name)).ok(); // best-effort: optional input: a missing/unreadable manifest just means 'not detected'
    let exists = |name: &str| worktree.join(name).exists();

    let flake = read("flake.nix");
    let nix_flake_devshell = flake
        .as_deref()
        .is_some_and(|s| s.contains("devShell") || s.contains("devShells"));

    let envrc = read(".envrc");
    let direnv = envrc.is_some();
    let direnv_uses_flake = envrc
        .as_deref()
        .is_some_and(|s| s.contains("use flake") || s.contains("use_flake"));

    let toolchain_files = DetectedToolchainFiles::detect(worktree, mise_env);
    let tool_versions = !toolchain_files.is_empty();

    let mut languages = Vec::new();
    let mut push = |l: Language| {
        if !languages.contains(&l) {
            languages.push(l);
        }
    };
    if exists("package.json") {
        push(Language::Node);
    }
    if exists("pyproject.toml") || exists("requirements.txt") || exists("setup.py") {
        push(Language::Python);
    }
    if exists("Cargo.toml") {
        push(Language::Rust);
    }
    if exists("go.mod") {
        push(Language::Go);
    }
    if exists("Gemfile") {
        push(Language::Ruby);
    }
    if exists("deno.json") || exists("deno.jsonc") {
        push(Language::Deno);
    }
    if exists("build.sbt") || exists("project/build.properties") || exists(".scala-version") {
        push(Language::Jvm);
    }

    let devenv = exists("devenv.nix");
    // Classic (non-flake) Nix only counts when no flake devShell / devenv
    // outranks it — those already own the Nix tier with `nix develop`.
    let nix_classic =
        (exists("shell.nix") || exists("default.nix")) && !nix_flake_devshell && !devenv;

    // Keep presence detection in lockstep with devcontainer selection,
    // including `.devcontainer/<variant>/devcontainer.json` layouts.
    let devcontainer = !crate::devcontainer_select::candidates(worktree).is_empty();

    EnvRequirements {
        nix_flake_devshell,
        devenv,
        direnv,
        direnv_uses_flake,
        tool_versions,
        toolchain_files,
        nix_classic,
        languages,
        devcontainer,
    }
}

/// Why a host dotfile would misbehave when transplanted into a sandbox/remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PitfallKind {
    /// References an absolute `/nix/store/<hash>-…` (or `/run/current-system/…`)
    /// path that won't exist in a non-Nix / different-closure sandbox. The
    /// classic home-manager-rc breakage.
    AbsentStorePath,
    /// Calls a command the rc assumes is installed but isn't declared in
    /// `[sandbox.home] tools` (drives the `tool-parity` suggestion).
    MissingTool,
}

/// One reason a specific dotfile may not work as-is in a sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotfilePitfall {
    /// The dotfile basename, e.g. `.zshrc`.
    pub file: String,
    pub kind: PitfallKind,
    /// The offending store path (for `AbsentStorePath`) or command (`MissingTool`).
    pub detail: String,
}

/// Extract the distinct top-level `/nix/store/<32-char-hash>-<name>` (and
/// `/run/current-system/...`) roots referenced in `text`. Pure over a string —
/// the host file read happens in the caller. Used to (a) decide whether a dotfile
/// is portable, and (b) compute the closure to reproduce under host-parity.
pub fn store_roots_in(text: &str) -> Vec<String> {
    let mut roots: Vec<String> = Vec::new();
    let mut push = |s: String| {
        if !roots.contains(&s) {
            roots.push(s);
        }
    };
    // Scan byte-wise for the two prefixes; take the path up to the next char that
    // can't be in a store path (whitespace, quotes, parens, etc.).
    let stop =
        |c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '(' | ')' | ':' | ';' | ',');
    for prefix in ["/nix/store/", "/run/current-system"] {
        let mut hay = text;
        while let Some(i) = hay.find(prefix) {
            let rest = &hay[i..];
            let end = rest.find(stop).unwrap_or(rest.len());
            let full = &rest[..end];
            if prefix == "/nix/store/" {
                // Reduce to the store root: /nix/store/<hash>-<name> (first 4 path
                // components: "", "nix", "store", "<hash>-<name>").
                let root: String = full.split('/').take(4).collect::<Vec<_>>().join("/");
                // Only accept a plausible store entry (has a hash-name segment).
                if root.len() > "/nix/store/".len() {
                    push(root);
                }
            } else {
                push(full.to_string());
            }
            hay = &rest[end..];
        }
    }
    roots
}

/// Scan a dotfile's `contents` for transplant pitfalls. `declared_tools` are the
/// `[sandbox.home] tools` already slated for install (so a referenced-and-declared
/// tool isn't flagged). Pure + unit-tested; detection only — never edits the file.
pub fn scan_dotfile(name: &str, contents: &str, declared_tools: &[String]) -> Vec<DotfilePitfall> {
    let mut out = Vec::new();
    for root in store_roots_in(contents) {
        out.push(DotfilePitfall {
            file: name.to_string(),
            kind: PitfallKind::AbsentStorePath,
            detail: root,
        });
    }
    for tool in referenced_tools(contents) {
        // Ignore shell builtins + ubiquitous base/coreutils tools — they're in
        // every sandbox, so flagging them is noise. Only genuine "did you forget
        // to install this?" tools (atuin, starship, zoxide, …) should surface.
        if is_ubiquitous_tool(&tool) {
            continue;
        }
        if declared_tools.iter().any(|t| t == &tool) {
            continue;
        }
        out.push(DotfilePitfall {
            file: name.to_string(),
            kind: PitfallKind::MissingTool,
            detail: tool,
        });
    }
    out
}

/// Is `t` a shell builtin or a base/coreutils tool present in essentially every
/// environment? Used to suppress no-signal `MissingTool` findings (a rc calling
/// `$(dirname …)` shouldn't read as "install dirname").
fn is_ubiquitous_tool(t: &str) -> bool {
    matches!(
        t,
        // shell builtins / keywords
        "cd" | "echo" | "export" | "eval" | "source" | "alias" | "set" | "unset"
            | "command" | "test" | "printf" | "read" | "local" | "return" | "exit"
            | "if" | "then" | "fi" | "for" | "do" | "done" | "case" | "esac" | "function"
        // coreutils + ubiquitous base tools
            | "cat" | "tr" | "cut" | "sed" | "awk" | "grep" | "egrep" | "fgrep"
            | "ls" | "rm" | "cp" | "mv" | "mkdir" | "rmdir" | "ln" | "touch" | "chmod"
            | "dirname" | "basename" | "realpath" | "readlink" | "mktemp" | "env"
            | "head" | "tail" | "wc" | "sort" | "uniq" | "tee" | "xargs" | "find"
            | "date" | "sleep" | "true" | "false" | "seq" | "tput" | "stty" | "uname"
            | "which" | "type" | "hash" | "id" | "whoami" | "hostname" | "sh" | "bash"
            | "git" | "ssh" | "curl" | "wget" | "sudo" | "kill" | "ps" | "pwd"
    )
}

/// Heuristic: tool names the rc clearly depends on at startup — `eval "$(X …)"`,
/// `source <(X …)`, and `command -v X`. Conservative (only these high-signal
/// shapes) to avoid flagging every word. Deduped, order-stable.
fn referenced_tools(contents: &str) -> Vec<String> {
    let mut tools: Vec<String> = Vec::new();
    let mut push = |t: &str| {
        let t = t.trim();
        if !t.is_empty()
            && t.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            && !tools.iter().any(|x| x == t)
        {
            tools.push(t.to_string());
        }
    };
    for line in contents.lines() {
        let l = line.trim();
        if l.starts_with('#') {
            continue;
        }
        // `eval "$(X ...)"` / `eval "$(X)"`
        for marker in ["$(", "<("] {
            let mut hay = l;
            while let Some(i) = hay.find(marker) {
                let rest = &hay[i + marker.len()..];
                if let Some(first) = rest.split_whitespace().next() {
                    let first = first.trim_end_matches(')');
                    push(first);
                }
                hay = rest;
            }
        }
        // `command -v X`
        if let Some(i) = l.find("command -v ")
            && let Some(t) = l[i + "command -v ".len()..].split_whitespace().next()
        {
            push(t);
        }
    }
    tools
}

/// Inputs the host supplies to compile a concrete plan (the bits `detect` can't
/// know: where the repo lives, where to put it, what to carry over).
#[derive(Debug, Clone)]
pub struct PlanOpts {
    /// Absolute workdir inside the sandbox (the cloned repo root). e.g. `/workspace`.
    pub workdir: String,
    /// Git remote to clone (`None` ⇒ skip the clone step, e.g. `data = "sync"`).
    pub origin: Option<String>,
    /// Branch to check out after cloning.
    pub branch: Option<String>,
    /// Host dotfile basenames to upload into the sandbox `$HOME` (e.g. `.zshrc`).
    pub dotfiles: Vec<String>,
    /// Personal CLI tools to install (`[sandbox.home] tools`) — Nix-first with a
    /// native package-manager fallback.
    pub tools: Vec<String>,
    /// Optional dotfiles repo to clone + bootstrap in the sandbox.
    pub dotfiles_repo: String,
    /// Bring-your-own setup commands (already resolved — `setup` plus the
    /// contents/path of `setup_script`), run in order after tools/dotfiles.
    pub setup: Vec<String>,
    /// Coding-agent CLIs to make work out-of-the-box in the sandbox (e.g.
    /// `["claude", "codex"]`): known ones get an installer step; all get their
    /// host config/credential dirs uploaded (`StepKind::AgentConfigs`).
    pub agents: Vec<String>,
    /// Allow installing Nix for the Nix tier (the heavy, highest-fidelity path).
    pub allow_nix: bool,
    /// Checkpoint the sandbox after a successful provision (one-time cost).
    pub checkpoint: bool,
    /// Which Nix installer to run (speedup): official (default) or Determinate.
    pub nix_installer: crate::config::NixInstaller,
    /// `http-connections`/`max-substitution-jobs` to set in the sandbox nix.conf
    /// (`None` ⇒ leave Nix defaults) — parallelizes the download-bound devShell build.
    pub nix_parallel: Option<u32>,
    /// Optional project binary cache so the devShell is a download, not a build.
    pub binary_cache: Option<BinaryCache>,
    /// How hard to reproduce the host shell (`[sandbox.home] strategy`). Gates the
    /// personal-layer steps: `Clean` emits none; `Portable`/`ToolParity` keep the
    /// current ordering (the caller pre-filters `dotfiles` to portable ones);
    /// `HostParity` additionally reproduces the host nix closure (experimental).
    pub strategy: crate::config::ShellStrategy,
    /// `HostParity` only: the user's home-manager flake ref for the in-sandbox
    /// `home-manager switch` fallback (empty ⇒ disabled).
    pub nix_home_flake: String,
    /// `HostParity` only: host `/nix/store` roots referenced by the uploaded
    /// dotfiles (detected on the host via [`store_roots_in`]); their closure is
    /// made resolvable in the sandbox so the exact dotfiles work. Empty ⇒ none.
    pub home_store_roots: Vec<String>,
    /// `HostParity` only: push the closure straight from the host store into the
    /// sandbox store over the WSS ssh tunnel (no hosted cache — the host *is* the
    /// cache). Set by the host when `connect = "ssh"` and no `binary_cache_url`.
    /// Emits a host-executed [`StepKind::HomeClosurePush`] instead of an
    /// in-sandbox substitute step.
    pub home_closure_p2p: bool,
    /// `HostParity` + p2p only: store paths to `nix profile install` in the
    /// sandbox after the push, so the shell + prompt tools (zsh/starship/atuin/…)
    /// land on `PATH` by name. Empty ⇒ none.
    pub home_profile_installs: Vec<String>,
    /// Opt-in: carry the host's atuin credentials + config into the sandbox so its
    /// shell history joins atuin sync. Emits an `atuin_sync` step after `tools`.
    pub atuin: bool,
    /// Deprecated compatibility flag. Host-side devShell transfer is disabled;
    /// target-side provisioning is the only source of this behavior.
    pub push_devshell: bool,
    /// Opt-in: skip the blocking `devshell` warm/build step during provisioning
    /// (and the p2p push + cache push that feed it). The repo's devShell then
    /// builds lazily in-pane on first `direnv`/`nix develop` instead of gating the
    /// loading screen on a multi-minute toolchain build. Trades a prebuilt (and
    /// checkpointed) devShell for a shell that comes up immediately.
    pub skip_devshell_warm: bool,
    /// Host path of the local worktree to bring to full parity inside the
    /// sandbox after the origin clone: local unpushed commits (a thin git
    /// bundle), uncommitted tracked changes (a `git diff HEAD` patch), and
    /// untracked non-ignored files (a tar) are captured on the host and replayed
    /// over the clone. `None` ⇒ pristine origin checkout only (e.g. a pool spare,
    /// or a non-`in_env` data mode). Emits a `local_parity` step after `clone`.
    pub local_parity: Option<String>,
    /// Snapshot id to overlay AFTER local parity: the worktree was hibernated
    /// (compute destroyed, state captured to the `[lifecycle.snapshot]` store)
    /// and this fresh sandbox must resume from that snapshot — its
    /// `reset --hard` wins tracked/commit state over the parity mirror, which
    /// is strictly older by construction. Emits a `snapshot_restore` step.
    pub snapshot_restore: Option<String>,
    /// Sandbox-side URL of the host's embedded nix binary cache (the loopback the
    /// reverse tunnel binds, e.g. `http://127.0.0.1:8484`), or `None`. When set,
    /// it's baked into the sandbox nix.conf as an `extra-substituters` entry.
    pub host_cache_url: Option<String>,
    /// Installation public key used to verify the embedded host cache's `Sig:`
    /// lines. Used only with `host_cache_url`; signature verification stays on.
    pub host_cache_key: Option<String>,
    /// `[toolchain]` — how the [`Tier::SynthNix`] tier resolves language
    /// manifests into a synthesized Nix devShell (mode + per-language package
    /// overrides). The default (`mode = "auto"`) keeps SynthNix; `mise`/`off`
    /// downgrade it (see [`plan`]).
    pub toolchain: crate::toolchain::ToolchainConfig,
    /// Install the GitHub token credential helper in the `git_auth` step.
    /// False (the default) unless `[sandbox.git]` grants a token
    /// ([`crate::config::SandboxGitConfig::admits_git_token`]): credentials stay
    /// with thegn, and the step configures no token helper or https rewrite.
    pub git_token: bool,
    /// Seed the sandbox repository from a host-side `git bundle` of the local
    /// branch instead of cloning `origin` inside the sandbox (THE-2021): the
    /// sandbox needs no network credential to get the code. `None` ⇒ the
    /// in-sandbox clone (pool spares, and the `clone` transport). Emits a
    /// host-executed [`StepKind::BundleSeed`] in place of the `clone` step.
    pub bundle_seed: Option<BundleSeed>,
}

/// Where a [`StepKind::BundleSeed`] step reads the repository from on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleSeed {
    /// Host path of the local checkout (a git worktree) to bundle.
    pub worktree: String,
    /// Local branch to bundle and check out in the sandbox.
    pub branch: String,
}

/// A Nix binary-cache substituter (and optional push) for fast devShells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryCache {
    /// Substituter URL (e.g. `https://cache.example.org` or `s3://…`).
    pub url: String,
    /// Public key trusting the cache (required to substitute from it).
    pub key: String,
    /// Push the built devShell closure to the cache during provisioning.
    pub push: bool,
}

impl Default for PlanOpts {
    fn default() -> Self {
        PlanOpts {
            workdir: "/workspace".to_string(),
            origin: None,
            branch: None,
            dotfiles: Vec::new(),
            tools: Vec::new(),
            dotfiles_repo: String::new(),
            setup: Vec::new(),
            agents: Vec::new(),
            allow_nix: true,
            checkpoint: true,
            nix_installer: crate::config::NixInstaller::Official,
            nix_parallel: None,
            binary_cache: None,
            strategy: crate::config::ShellStrategy::default(),
            nix_home_flake: String::new(),
            home_store_roots: Vec::new(),
            home_closure_p2p: false,
            home_profile_installs: Vec::new(),
            atuin: false,
            push_devshell: false,
            skip_devshell_warm: false,
            local_parity: None,
            snapshot_restore: None,
            host_cache_key: None,
            host_cache_url: None,
            toolchain: crate::toolchain::ToolchainConfig::default(),
            git_token: false,
            bundle_seed: None,
        }
    }
}

/// One ordered step in a provisioning run. The `id` is stable (for idempotence +
/// the loading screen); `label` is human-facing; `kind` tells the applier how to
/// run it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionStep {
    pub id: String,
    pub label: String,
    pub kind: StepKind,
}

/// How the host materializes a step against a placement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepKind {
    /// Run this `/bin/sh` script in the sandbox (via the exec API). Scripts are
    /// written to be idempotent so a re-provision is safe.
    Exec(String),
    /// Upload these host dotfile basenames (relative to the host `$HOME`) into
    /// the sandbox `$HOME` (via the fs API).
    Dotfiles(Vec<String>),
    /// Upload these coding agents' host config/credential dirs into the sandbox
    /// `$HOME` so the agent (claude code, codex, …) is logged-in there. The
    /// applier maps each id to its paths via [`agent_config_paths`].
    AgentConfigs(Vec<String>),
    /// Snapshot the sandbox so the (heavy) install is paid once.
    Checkpoint,
    /// Host-executed (not in-sandbox): push the closure of these host `/nix/store`
    /// roots straight into the sandbox store over the WSS ssh tunnel, using the
    /// host's `nix`. The host store is an ephemeral binary cache — no hosted cache
    /// or signing key. Runs after Nix is installed in the sandbox, before the
    /// profile-install + dotfiles steps.
    HomeClosurePush(Vec<String>),
    /// Host-executed: carry the host's atuin credentials + config into the sandbox
    /// (`~/.config/atuin/config.toml` dereferenced + `~/.local/share/atuin/{key,
    /// session}`), so its shell history joins atuin's own sync. Opt-in
    /// (`[sandbox.home] atuin = true`). Best-effort; the history DBs are NOT copied
    /// (the sync server reconciles those).
    AtuinSync,
    /// Host-executed: bring the sandbox clone to full parity with the LOCAL
    /// worktree — replay unpushed commits (a thin `git bundle`), uncommitted
    /// tracked changes (`git diff HEAD`), and untracked non-ignored files. The
    /// applier reads the host worktree (`worktree`), uploads the captured
    /// artifacts, and execs the replay in `workdir`. Best-effort: a failure
    /// leaves the pristine origin checkout in place.
    LocalParity { worktree: String, workdir: String },
    /// Host-executed: overlay a hibernation snapshot from the
    /// `[lifecycle.snapshot]` store onto the fresh clone (fetch bundle +
    /// `reset --hard` + apply patch + untar). NOT best-effort — a failed
    /// restore fails the step so the hibernated work is never silently lost.
    SnapshotRestore {
        worktree: String,
        workdir: String,
        snapshot_id: String,
    },
    /// Host-executed: seed the sandbox repository from a `git bundle` of the
    /// local `branch` in `worktree` (uploaded over the provider files API and
    /// cloned into `workdir`), in place of the in-sandbox origin clone. NOT
    /// best-effort: without it the sandbox has no repository.
    BundleSeed {
        worktree: String,
        workdir: String,
        branch: String,
    },
}

/// The compiled, backend-agnostic provisioning plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvPlan {
    pub steps: Vec<ProvisionStep>,
    /// The detected tier this plan targets (for logging / the loading screen).
    pub tier: Tier,
}

/// Basename of the provision marker [`EnvPlan::marker_path`] drops in the
/// workdir. The `git_exclude` step hides it from `git status` (THE-2043).
pub const PROVISION_MARKER: &str = ".thegn-provisioned";

/// Step ids a shell does not need. Prompt-first runs them after the pane's
/// prompt (agents' CLIs and logins, shell history, the base checkpoint).
pub const DEFERRED_STEP_IDS: &[&str] = &[
    "agents_install",
    "agents_config",
    "atuin_sync",
    "checkpoint",
];

/// Whether a provisioning step can run after the shell prompt appears.
pub fn is_deferred_step(id: &str) -> bool {
    DEFERRED_STEP_IDS.contains(&id)
}

/// Marker contents: done, and foreground-done-with-deferred-pending.
pub const MARKER_DONE: &[u8] = b"ok\n";
pub const MARKER_DEFERRED: &[u8] = b"deferred\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerState {
    Done,
    Deferred,
}

/// `Deferred` iff the bytes, ASCII-whitespace-trimmed, equal `deferred`;
/// anything else (legacy `ok`, empty, unknown) is `Done`.
pub fn marker_state(bytes: &[u8]) -> MarkerState {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    if &bytes[start..end] == b"deferred" {
        MarkerState::Deferred
    } else {
        MarkerState::Done
    }
}

impl EnvPlan {
    /// Split this plan into prompt-critical and post-prompt steps, preserving
    /// plan order within both lists. A warm spare is never split.
    pub fn split_deferred(&self, defer: bool) -> (Vec<ProvisionStep>, Vec<ProvisionStep>) {
        if !defer {
            return (self.steps.clone(), Vec::new());
        }
        self.steps
            .iter()
            .cloned()
            .partition(|step| !is_deferred_step(&step.id))
    }

    /// Sentinel file the applier drops after a successful run. It contains
    /// [`MARKER_DONE`] when all steps completed or [`MARKER_DEFERRED`] when
    /// only the foreground steps completed. Lives directly in the workdir
    /// (the repo checkout root), so the `git_exclude` provisioning step lists
    /// it in the checkout's `.git/info/exclude` to keep it out of `git status`.
    pub fn marker_path(workdir: &str) -> String {
        format!("{}/{PROVISION_MARKER}", workdir.trim_end_matches('/'))
    }
}

/// Whether this plan cannot be usable without Nix, so a failed install must
/// halt provisioning (the `nix_verify` step) instead of reporting "up" with no
/// `nix`: the repo declares a Nix dev environment (flake devShell, `use flake`,
/// devenv), or the options ask for a devShell push / the host Nix cache.
fn nix_required(req: &EnvRequirements, opts: &PlanOpts) -> bool {
    let host_cache = opts
        .host_cache_url
        .as_deref()
        .is_some_and(|u| !u.trim().is_empty());
    opts.push_devshell
        || host_cache
        || req.nix_flake_devshell
        || req.direnv_uses_flake
        || req.devenv
}

/// The fatal post-install check: `nix` resolves on `PATH` after sourcing the
/// same profile hooks the install step sources, else the step fails with the
/// cause instead of letting the plan finish without Nix (THE-2045).
fn nix_verify_step() -> ProvisionStep {
    ProvisionStep {
        id: "nix_verify".into(),
        label: "Verify Nix".into(),
        kind: StepKind::Exec(
            "export HOME=${HOME:-/root}; \
             [ -r \"$HOME/.nix-profile/etc/profile.d/nix.sh\" ] && . \"$HOME/.nix-profile/etc/profile.d/nix.sh\" 2>/dev/null || true; \
             [ -r /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh ] && . /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh 2>/dev/null || true; \
             export PATH=\"$HOME/.nix-profile/bin:/nix/var/nix/profiles/default/bin:$PATH\"; \
             command -v nix >/dev/null 2>&1 || { echo \"nix is not installed: the Install Nix step failed (see its output above)\"; exit 1; }"
                .into(),
        ),
    }
}

/// Compile [`EnvRequirements`] + [`PlanOpts`] into an ordered [`EnvPlan`]. Pure:
/// produces shell strings, runs nothing.
pub fn plan(req: &EnvRequirements, opts: &PlanOpts) -> EnvPlan {
    let wd = sh_quote(&opts.workdir);
    // Resolve the effective tier: SynthNix (languages-only, batteries-included
    // synthesized Nix devShell) is gated by `[toolchain] mode` + `allow_nix` —
    // `mise` keeps the mise behavior (ToolVersions), `off`/no-Nix falls back to
    // today's best-effort native runtimes (Languages).
    use crate::toolchain::ToolchainMode;
    let tier = match req.tier() {
        Tier::SynthNix => match opts.toolchain.mode {
            ToolchainMode::Mise => Tier::ToolVersions,
            ToolchainMode::Off => Tier::Languages,
            ToolchainMode::Auto | ToolchainMode::Nix if !opts.allow_nix => Tier::Languages,
            ToolchainMode::Auto | ToolchainMode::Nix => Tier::SynthNix,
        },
        t => t,
    };
    let mut steps = Vec::new();
    let needs_nix = nix_required(req, opts);

    // 1. Workspace dir.
    steps.push(ProvisionStep {
        id: "workspace".into(),
        label: "Prepare workspace".into(),
        kind: StepKind::Exec(format!("mkdir -p {wd}")),
    });

    // 2. Git auth + clone. Only when `opts.git_token` (`[sandbox.git]` grants a
    // token) does the auth step install a runtime-conditional credential
    // helper: if a GitHub token is present in the exec env (carried from the host
    // via `passthrough_env_remote` — GH_TOKEN/GITHUB_TOKEN), configure git to use
    // it for HTTPS clone/push; a no-op without a token. Persisted in ~/.gitconfig
    // so it survives the checkpoint and works for later pushes from the shell.
    let seeded = opts.bundle_seed.as_ref();
    if opts.origin.is_some() || seeded.is_some() {
        steps.push(ProvisionStep {
            id: "git_auth".into(),
            label: "Configure git credentials".into(),
            kind: StepKind::Exec(git_auth_script(opts.git_token)),
        });
    }
    // Seed the repo from a host-side bundle (no in-sandbox credential needed),
    // or clone it into the workdir (idempotent — skips if already a repo).
    let repo_step = match (seeded, opts.origin.as_deref()) {
        (Some(seed), _) => Some(ProvisionStep {
            id: "bundle_seed".into(),
            label: "Seed repository (git bundle)".into(),
            kind: StepKind::BundleSeed {
                worktree: seed.worktree.clone(),
                workdir: opts.workdir.clone(),
                branch: seed.branch.clone(),
            },
        }),
        (None, Some(origin)) => {
            let clone = crate::remote::provision_repo_script(origin, opts.branch.as_deref());
            Some(ProvisionStep {
                id: "clone".into(),
                label: "Clone repository".into(),
                kind: StepKind::Exec(format!("cd {wd} && {clone}")),
            })
        }
        (None, None) => None,
    };
    if let Some(repo_step) = repo_step {
        steps.push(repo_step);
        // Hide the provision marker (dropped in the checkout root) from `git
        // status` (THE-2043). Idempotent (`grep -qxF`) and best-effort: a
        // checkout without a `.git` dir, or an unwritable one, is skipped.
        steps.push(ProvisionStep {
            id: "git_exclude".into(),
            label: "Hide thegn marker from git".into(),
            kind: StepKind::Exec(git_exclude_script(&wd)),
        });
        // Bring the clone to full parity with the local worktree (unpushed
        // commits + uncommitted + untracked). Runs before the toolchain tier so
        // a locally-modified flake.lock / .envrc drives the devShell build. Only
        // for a real worktree (a pool spare passes `None` — it stays generic
        // until a claim rebranches it).
        if let Some(local) = opts.local_parity.as_deref() {
            steps.push(ProvisionStep {
                id: "local_parity".into(),
                label: "Mirror local changes".into(),
                kind: StepKind::LocalParity {
                    worktree: local.to_string(),
                    workdir: opts.workdir.clone(),
                },
            });
        }
        // Overlay the hibernation snapshot AFTER parity: the snapshot carries
        // the sandbox's own last state (captured at destroy time), which
        // supersedes the local mirror for tracked/commit state; untracked
        // files from both survive the untar.
        if let Some(snap) = opts.snapshot_restore.as_deref() {
            steps.push(ProvisionStep {
                id: "snapshot_restore".into(),
                label: "Restore hibernated work".into(),
                kind: StepKind::SnapshotRestore {
                    worktree: opts.local_parity.clone().unwrap_or_default(),
                    workdir: opts.workdir.clone(),
                    snapshot_id: snap.to_string(),
                },
            });
        }
    }

    // 3. Tier-specific toolchain.
    match tier {
        Tier::Nix => {
            if opts.allow_nix {
                steps.push(ProvisionStep {
                    id: "nix".into(),
                    label: "Install Nix".into(),
                    kind: StepKind::Exec(nix_install_script(
                        opts.nix_installer,
                        opts.nix_parallel,
                        opts.binary_cache.as_ref(),
                        opts.host_cache_url.as_deref(),
                        opts.host_cache_key.as_deref(),
                    )),
                });
                if needs_nix {
                    steps.push(nix_verify_step());
                }
                if req.direnv {
                    steps.push(ProvisionStep {
                        id: "direnv".into(),
                        label: "Install direnv".into(),
                        kind: StepKind::Exec(direnv_install_script()),
                    });
                }
                // The from-scratch devShell BUILD is the single longest, most CPU/
                // network-bound part of a provision. When `skip_devshell_warm` is set
                // it's omitted: the loading screen no longer blocks on it, and the
                // devShell instead resolves lazily in-pane on first `direnv`/`nix
                // develop` (a local store hit when the scoped push above seeded it).
                if !opts.skip_devshell_warm {
                    steps.push(ProvisionStep {
                        id: "devshell".into(),
                        label: "Build dev shell".into(),
                        kind: StepKind::Exec(devshell_warm_script(&opts.workdir, req)),
                    });
                    // Optional: push the freshly-built devShell closure to the project
                    // binary cache so later sandboxes download it instead of building.
                    if let Some(c) = opts
                        .binary_cache
                        .as_ref()
                        .filter(|c| c.push && !c.url.trim().is_empty())
                    {
                        steps.push(ProvisionStep {
                            id: "cache_push".into(),
                            label: "Push devShell to cache".into(),
                            kind: StepKind::Exec(cache_push_script(&opts.workdir, c)),
                        });
                    }
                }
            }
        }
        Tier::ToolVersions => {
            // Installation is an explicit, trust-gated provider operation.
            // Core records the tier but never constructs a vendor command.
        }
        Tier::SynthNix => {
            // Batteries included: install Nix, then synthesize + warm a
            // devShell flake covering the detected languages (only reached
            // when `opts.allow_nix` — see the tier resolution above).
            steps.push(ProvisionStep {
                id: "nix".into(),
                label: "Install Nix".into(),
                kind: StepKind::Exec(nix_install_script(
                    opts.nix_installer,
                    opts.nix_parallel,
                    opts.binary_cache.as_ref(),
                    opts.host_cache_url.as_deref(),
                    opts.host_cache_key.as_deref(),
                )),
            });
            if needs_nix {
                steps.push(nix_verify_step());
            }
            let packages = crate::toolchain::packages_for(&req.languages, &opts.toolchain);
            steps.push(ProvisionStep {
                id: "toolchain".into(),
                label: "Synthesize toolchain (nix)".into(),
                kind: StepKind::Exec(crate::toolchain::synth_dir_script(&packages)),
            });
        }
        Tier::Languages => {
            steps.push(ProvisionStep {
                id: "languages".into(),
                label: "Install language runtimes".into(),
                kind: StepKind::Exec(languages_install_script(&req.languages)),
            });
        }
        Tier::Bare => {}
    }
    // Did the toolchain tier already install Nix? Host-parity needs it on any tier.
    let nix_installed = matches!(tier, Tier::Nix | Tier::SynthNix) && opts.allow_nix;

    // 4. Personal layer (`[sandbox.home]`) — generic, repo-independent. Runs
    // after the project toolchain so the tool installer can use Nix when present.
    // The `strategy` gates it: `Clean` ships no personal dotfiles/tools/setup at
    // all (just the rc-free shell); the others keep the ordering below. (Agents
    // are orthogonal to the shell rc and run regardless.) The caller is
    // responsible for pre-filtering `opts.dotfiles` to portable ones under
    // `Portable`/`ToolParity`; `HostParity` passes them unfiltered + reproduces
    // the host closure (below) so the absolute store paths resolve.
    use crate::config::ShellStrategy;
    let personal = opts.strategy != ShellStrategy::Clean;
    if personal && !opts.tools.is_empty() {
        steps.push(ProvisionStep {
            id: "tools".into(),
            label: "Install personal tools".into(),
            kind: StepKind::Exec(tools_install_script(&opts.tools)),
        });
    }
    if personal && !opts.dotfiles_repo.trim().is_empty() {
        steps.push(ProvisionStep {
            id: "dotfiles_repo".into(),
            label: "Set up dotfiles".into(),
            kind: StepKind::Exec(dotfiles_repo_script(opts.dotfiles_repo.trim())),
        });
    }
    if personal && !opts.setup.is_empty() {
        steps.push(ProvisionStep {
            id: "setup".into(),
            label: "Run setup".into(),
            kind: StepKind::Exec(opts.setup.join("\n")),
        });
    }
    // Coding agents out-of-the-box: install known CLIs, then upload their
    // config/creds so they're logged-in in the sandbox.
    if !opts.agents.is_empty() {
        steps.push(ProvisionStep {
            id: "agents_install".into(),
            label: "Install coding agents".into(),
            kind: StepKind::Exec(agents_install_script(&opts.agents)),
        });
        steps.push(ProvisionStep {
            id: "agents_config".into(),
            label: "Sync agent logins".into(),
            kind: StepKind::AgentConfigs(opts.agents.clone()),
        });
    }

    // atuin shell-history sync (opt-in): carry the host's atuin creds + config so
    // the sandbox's history joins atuin's own sync. After `tools` (atuin must be
    // installed) and before the checkpoint. Best-effort, host-executed.
    if personal && opts.atuin {
        steps.push(ProvisionStep {
            id: "atuin_sync".into(),
            label: "Sync atuin history".into(),
            kind: StepKind::AtuinSync,
        });
    }

    // 4b. Host-parity (experimental): make the host nix closure the dotfiles
    // reference resolvable in the sandbox, BEFORE uploading them, so the exact
    // host rc works unchanged. Cache-first (substitute the detected store roots),
    // else `home-manager switch` from the user's home flake. Dormant unless the
    // caller (host) populated the carriers — so a default `Portable` plan is
    // unchanged. Must come before the dotfiles upload + the checkpoint.
    if opts.strategy == ShellStrategy::HostParity
        && (!opts.home_store_roots.is_empty()
            || !opts.nix_home_flake.trim().is_empty()
            || !opts.home_profile_installs.is_empty())
    {
        // Host-parity needs `nix` (to substitute the closure / run home-manager /
        // receive the p2p push). The Nix tier already installed it; on any other
        // tier, install it now.
        if !nix_installed && opts.allow_nix {
            steps.push(ProvisionStep {
                id: "nix".into(),
                label: "Install Nix".into(),
                kind: StepKind::Exec(nix_install_script(
                    opts.nix_installer,
                    opts.nix_parallel,
                    opts.binary_cache.as_ref(),
                    opts.host_cache_url.as_deref(),
                    opts.host_cache_key.as_deref(),
                )),
            });
            if needs_nix {
                steps.push(nix_verify_step());
            }
        }
        if opts.home_closure_p2p {
            // P2P: the host pushes its store straight into the sandbox (host-side,
            // after the Nix install above), then we install the shell + tools by
            // store path so they're on `PATH` by name.
            if !opts.home_store_roots.is_empty() {
                steps.push(ProvisionStep {
                    id: "home_closure_push".into(),
                    label: "Transfer home closure (p2p)".into(),
                    kind: StepKind::HomeClosurePush(opts.home_store_roots.clone()),
                });
            }
            if !opts.home_profile_installs.is_empty() {
                steps.push(ProvisionStep {
                    id: "home_profile".into(),
                    label: "Install shell + prompt tools".into(),
                    kind: StepKind::Exec(home_profile_install_script(&opts.home_profile_installs)),
                });
            }
        } else if !opts.home_store_roots.is_empty() {
            steps.push(ProvisionStep {
                id: "home_closure".into(),
                label: "Sync home shell closure".into(),
                kind: StepKind::Exec(home_substitute_script(
                    &opts.home_store_roots,
                    opts.binary_cache.as_ref(),
                )),
            });
        } else if !opts.nix_home_flake.trim().is_empty() {
            steps.push(ProvisionStep {
                id: "home_switch".into(),
                label: "Activate home-manager".into(),
                kind: StepKind::Exec(home_switch_script(opts.nix_home_flake.trim())),
            });
        }
    }

    // 5. Dotfiles (opportunistic). Skipped entirely under `Clean`.
    if personal && !opts.dotfiles.is_empty() {
        steps.push(ProvisionStep {
            id: "dotfiles".into(),
            label: "Sync dotfiles".into(),
            kind: StepKind::Dotfiles(opts.dotfiles.clone()),
        });
    }

    // 5. Checkpoint so the heavy install is one-time.
    if opts.checkpoint {
        steps.push(ProvisionStep {
            id: "checkpoint".into(),
            label: "Snapshot environment".into(),
            kind: StepKind::Checkpoint,
        });
    }

    EnvPlan { steps, tier }
}

/// The `git_exclude` step script for the (already shell-quoted) workdir `wd`:
/// append `/<marker>` to the checkout's `.git/info/exclude` unless the exact
/// line is already there. Ends `|| true`, so it never fails provisioning.
fn git_exclude_script(wd: &str) -> String {
    format!(
        "cd {wd} && [ -d .git ] && mkdir -p .git/info && {{ grep -qxF '/{PROVISION_MARKER}' .git/info/exclude 2>/dev/null || printf '%s\\n' '/{PROVISION_MARKER}' >> .git/info/exclude; }} || true"
    )
}

/// Escape a value for embedding in the **single-quoted `printf` format string**
/// that writes `nix.conf` (`printf '{conf}' > …`). Two hazards: a `%` is read by
/// `printf` as a conversion directive (a presigned/S3 cache URL with `%2F`… would
/// corrupt or error the output), and a `'` closes the shell single-quote (turning
/// the generated script into a syntax error). `%` → `%%`; `'` → `'\''`.
fn printf_fmt_escape(s: &str) -> String {
    s.replace('%', "%%").replace('\'', "'\\''")
}

/// Single-user Nix install (no systemd/daemon — sprites are minimal microVMs),
/// idempotent: a no-op if `nix` is already on `PATH`. Enables flakes.
fn nix_install_script(
    installer: crate::config::NixInstaller,
    parallel: Option<u32>,
    cache: Option<&BinaryCache>,
    host_cache_url: Option<&str>,
    host_cache_key: Option<&str>,
) -> String {
    use crate::config::NixInstaller;
    // Persisted nix.conf lines (static, safe to bake — NOT secrets; the token auth
    // stays runtime-only in nix_runtime_prelude). These apply to the devShell build
    // (the longest step) AND interactive in-pane `nix develop`/`direnv`.
    let mut conf = String::from("experimental-features = nix-command flakes\\n");
    if let Some(n) = parallel {
        // Parallelize the download-bound devShell substitution — the biggest win.
        write!(
            conf,
            "http-connections = {n}\\nmax-substitution-jobs = {n}\\n"
        )
        .expect("writing to String cannot fail");
    }
    if let Some(c) = cache.filter(|c| !c.url.trim().is_empty()) {
        write!(
            conf,
            "extra-substituters = {}\\n",
            printf_fmt_escape(c.url.trim())
        )
        .expect("writing to String cannot fail");
        if !c.key.trim().is_empty() {
            write!(
                conf,
                "extra-trusted-public-keys = {}\\n",
                printf_fmt_escape(c.key.trim())
            )
            .expect("writing to String cannot fail");
        }
    }
    // `nix` accumulates `extra-substituters` lines, so the host cache coexists
    // with `cache` above. Trust only its installation key and keep signature
    // verification enforced even when the key is absent.
    if let Some(url) = host_cache_url.map(str::trim).filter(|u| !u.is_empty()) {
        write!(conf, "extra-substituters = {}\\n", printf_fmt_escape(url))
            .expect("writing to String cannot fail");
        if let Some(key) = host_cache_key.map(str::trim).filter(|key| !key.is_empty()) {
            write!(
                conf,
                "extra-trusted-public-keys = {}\\n",
                printf_fmt_escape(key)
            )
            .expect("writing to String cannot fail");
        }
        conf.push_str("require-sigs = true\\n");
        // Graceful degradation for a cache reachable ONLY over the per-sprite reverse
        // tunnel. Two failure modes made `host_cache` unusable: (1) the in-sprite
        // build can start BEFORE the tunnel is up, so nix would hang on the OS connect
        // default dialing `127.0.0.1:8484`; (2) a large NAR can STALL if the mux
        // desyncs mid-transfer, and — worse — nix HARD-FAILS a path the narinfo
        // claimed but the substituter never delivered. These three make the host
        // cache a fast, safe accelerator that degrades instead of wedging:
        //   • connect-timeout        — fall through in 5s when the tunnel isn't
        //                              listening yet (vs a multi-minute OS hang).
        //   • stalled-download-timeout — abandon a desynced/stalled NAR in 30s (vs
        //                              nix's 300s default) so the supervisor's tunnel
        //                              reconnect + a retry can recover quickly.
        //   • fallback = true        — on any substitution failure, build the path
        //                              from source (or the next substituter) instead
        //                              of erroring the whole `nix develop`/`direnv`.
        // Scoped to the host-cache config so the public-cache-only path is untouched.
        conf.push_str("connect-timeout = 5\\n");
        conf.push_str("stalled-download-timeout = 30\\n");
        conf.push_str("fallback = true\\n");
    }
    // Readiness wait (sudo + DNS + real egress) shared by both installers; the
    // sprite user has passwordless sudo and the installer creates /nix via sudo.
    // When we run as root with no sudo (fly), `prep_nix_root` creates `/nix`
    // (root-owned, 0755) itself, so the installer skips its own `sudo mkdir`
    // (THE-2053). It acts only when uid is 0 and `/nix` is missing.
    // `claim_egress`: DNS alone is a FALSE ready-signal on a cold Fly microVM — the
    // local resolver answers `getent` seconds before outbound TCP/TLS to the CDN is
    // actually up, so the install fires into a dead network and `curl -fsSL` (silent)
    // fails with no diagnostic (the exact cold-boot 127 seen on fresh sprites). Gate
    // on an actual reachability probe (a 5s-bounded HTTPS GET of the tiny install
    // script) so we don't start until egress genuinely works. 60×2s ceiling covers a
    // slow cold boot; the whole loop short-circuits the instant the probe succeeds.
    let wait = "n=0; while [ $n -lt 60 ]; do \
           { sudo -n true 2>/dev/null || [ \"$(id -u)\" = 0 ]; } && \
             getent hosts nixos.org >/dev/null 2>&1 && \
             curl -fsS --max-time 5 -o /dev/null https://nixos.org/nix/install 2>/dev/null && break; \
           n=$((n+1)); sleep 2; \
         done";
    // Official: single-user (`--no-daemon`), POSIX sh (download then run — no
    // process substitution under dash). The proven default + fallback.
    // `--retry`: the readiness probe above proves egress once, but a cold Fly
    // network can still flap mid-run; the retry flags (with `--retry-all-errors`, so
    // even a 5xx/28-timeout retries, not just connection refusals) make the tiny
    // script fetch resilient instead of a silent one-shot. The 25MB tarball fetch is
    // inside the vendored installer (no flags to pass), so the do_install retry loop
    // + this egress gate are its safety net.
    let official = "curl -fsSL --retry 5 --retry-connrefused --retry-delay 2 --retry-all-errors \
             https://nixos.org/nix/install -o /tmp/nix-install.sh && \
           sh /tmp/nix-install.sh --no-daemon --yes";
    let install = match installer {
        NixInstaller::Official => official.to_string(),
        // Determinate (faster Rust installer). `--init none` = no systemd (a bare
        // microVM); falls back to the official single-user installer on ANY failure
        // so a determinate-incompatibility never wedges provisioning.
        NixInstaller::Determinate => format!(
            "(curl -fsSL --retry 5 --retry-connrefused --retry-delay 2 --retry-all-errors \
                 https://install.determinate.systems/nix -o /tmp/nix-ds.sh && \
               sh /tmp/nix-ds.sh install linux --no-confirm --init none) || ({official})"
        ),
    };
    // FAIL-FAST: the final `command -v nix` sets the exit code. Source BOTH the
    // single-user (`~/.nix-profile`) and daemon/system (`/nix/var/nix/profiles/
    // default`, where Determinate `--init none` lands) profile hooks + put both on
    // PATH first — otherwise a successful Determinate install reports a false
    // failure (its `nix` isn't under `~/.nix-profile`), printing "Nix was installed
    // successfully!" while the step shows exit 127.
    // `claim_store`: a non-root sandbox user CANNOT use a root-owned multi-user
    // store with no daemon ("/nix/var/nix/db not writable") — it silently breaks
    // every `nix profile install` (→ no tools, no starship). Determinate `--init
    // none` (and some base images) leave exactly that. With passwordless sudo and no
    // daemon, claim the whole store for SINGLE-USER use (chown to us): `nix profile`
    // then works directly, no daemon to keep alive, and it survives checkpoints
    // (FS ownership is captured) unlike a manually-started daemon. Run it BEFORE the
    // early-exit (a base image may ship a root store) and AFTER install (Determinate
    // leaves one). The early-exit now requires the db be WRITABLE, not just `nix`
    // present, so a present-but-unusable store falls through to the claim.
    // `ensure_tools`: the OFFICIAL installer unpacks a `.xz` binary tarball, so a
    // minimal microVM without `xz` (or `tar`/`curl`) fails mid-install with a
    // shell "command not found" (exit 127) AFTER printing "downloading Nix …
    // binary tarball" — the exact false failure seen on fresh sprites. Best-effort
    // install the prerequisites via whatever package manager is present (needs
    // passwordless sudo when non-root; a no-op otherwise), then re-check each
    // tool and name what is still missing (THE-2045) so a failed install says
    // why. The caller keeps `ensure_tools || true`: the install still runs (a
    // probe can be wrong), and the fatal `nix_verify` step reports the outcome.
    // `do_install` +
    // `{wait}` are wrapped in a bounded retry so a transient network/DNS flap on a
    // cold sandbox doesn't red-X the step; each attempt re-checks `command -v nix`
    // so a partial prior attempt that actually landed short-circuits.
    format!(
        "claim_store() {{ if [ \"$(id -u)\" != 0 ] && [ -d /nix/var/nix/db ] && [ \"$(stat -c %u /nix/var/nix/db 2>/dev/null)\" != \"$(id -u)\" ] && [ ! -S /nix/var/nix/daemon-socket/socket ] && command -v sudo >/dev/null 2>&1 && sudo -n true 2>/dev/null; then sudo chown -R \"$(id -u):$(id -g)\" /nix 2>/dev/null || true; fi; }}; \
         ensure_tools() {{ miss=; for t in xz tar curl; do command -v \"$t\" >/dev/null 2>&1 || miss=\"$miss $t\"; done; [ -z \"$miss\" ] && return 0; echo \"nix: installing missing prerequisites:$miss\"; if command -v sudo >/dev/null 2>&1 && sudo -n true 2>/dev/null; then S='sudo -n'; else S=; fi; if command -v apt-get >/dev/null 2>&1; then $S apt-get update -y && $S apt-get install -y xz-utils tar curl ca-certificates; elif command -v apk >/dev/null 2>&1; then $S apk add --no-cache xz tar curl ca-certificates; elif command -v dnf >/dev/null 2>&1; then $S dnf install -y xz tar curl ca-certificates; elif command -v yum >/dev/null 2>&1; then $S yum install -y xz tar curl ca-certificates; fi; miss=; for t in xz tar curl; do command -v \"$t\" >/dev/null 2>&1 || miss=\"$miss $t\"; done; [ -z \"$miss\" ] && return 0; echo \"nix: prerequisites still missing:$miss (no usable package manager or no root/sudo)\"; return 1; }}; \
         do_install() {{ {install}; [ -r \"$HOME/.nix-profile/etc/profile.d/nix.sh\" ] && . \"$HOME/.nix-profile/etc/profile.d/nix.sh\" 2>/dev/null || true; [ -r /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh ] && . /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh 2>/dev/null || true; export PATH=\"$HOME/.nix-profile/bin:/nix/var/nix/profiles/default/bin:$PATH\"; claim_store; command -v nix >/dev/null 2>&1; }}; \
         prep_nix_root() {{ [ -e /nix ] && return 0; [ \"$(id -u)\" = 0 ] || return 0; mkdir -m 0755 /nix && chown 0:0 /nix; }}; \
         export HOME=${{HOME:-/root}}; \
         export USER=${{USER:-$(id -un 2>/dev/null || echo root)}}; \
         claim_store; \
         if command -v nix >/dev/null 2>&1 && {{ [ \"$(stat -c %u /nix/var/nix/db 2>/dev/null)\" = \"$(id -u)\" ] || [ -S /nix/var/nix/daemon-socket/socket ] || [ \"$(id -u)\" = 0 ]; }}; then exit 0; fi; \
         {wait}; \
         ensure_tools || true; \
         prep_nix_root || true; \
         i=1; while :; do do_install && break; [ $i -ge 3 ] && break; echo \"nix: install attempt $i failed; retrying in 5s\"; i=$((i+1)); sleep 5; done; \
         mkdir -p \"$HOME/.config/nix\"; \
         printf '{conf}' > \"$HOME/.config/nix/nix.conf\"; \
         command -v nix >/dev/null 2>&1"
    )
}

/// The healing half of [`nix_runtime_prelude`]: the `nix_idle` and
/// `nix_store_heal` functions, then the guarded `/homeless-shelter` purge and
/// store heal. A macro (not a `const`) so `concat!` can splice it into both the
/// prelude and [`DIRENV_HEAL_LIB`]. Contains NO single quote, because the
/// direnv install writes it inside a single-quoted `printf` argument, and is
/// safe under `set -eu` (direnv evaluates its libs with errexit).
macro_rules! nix_heal_sh {
    () => {
        "nix_idle() { if command -v pgrep >/dev/null 2>&1; then ! pgrep -x \"nix|nix-build|nix-shell|nix-store|nix-instantiate|nix-daemon\" >/dev/null 2>&1; else for c in /proc/[0-9]*/comm; do case \"$(cat \"$c\" 2>/dev/null)\" in nix|nix-build|nix-shell|nix-store|nix-instantiate|nix-daemon) return 1;; esac; done; return 0; fi; }; \
         nix_store_heal() ( \
           PATH=\"$HOME/.nix-profile/bin:/nix/var/nix/profiles/default/bin:$PATH\"; \
           empty=$(find /nix/store -maxdepth 1 -name \"*.drv\" -size 0 2>/dev/null) || empty=; \
           [ -n \"$empty\" ] || exit 0; \
           n=0; \
           for d in $empty; do \
             refs=$(nix-store --query --referrers-closure \"$d\" 2>/dev/null) || refs=; \
             nix-store --delete --ignore-liveness ${refs:-$d} >/dev/null 2>&1 || true; \
             n=$((n+1)); \
           done; \
           echo \"nix: healed $n empty .drv file(s) in /nix/store\" >&2; \
           exit 0 \
         ); \
         if nix_idle; then \
           sudo -n rm -rf /homeless-shelter 2>/dev/null || true; \
           rm -rf /homeless-shelter 2>/dev/null || true; \
           nix_store_heal; \
         fi; "
    };
}

/// An sh prelude for any step that runs `nix` builds. Three jobs:
///
/// 1. **Private flake inputs**: export `NIX_CONFIG` with a GitHub `access-tokens`
///    line from the env token (GH_TOKEN, else GITHUB_TOKEN). nix's flake-input
///    fetcher does NOT use git's credential helper, so a private `github:org/repo`
///    input 404s without this even though `git clone` of the repo authenticates.
///    Runtime-only (never written to nix.conf), so the token isn't baked into the
///    checkpoint (same principle as the git credential helper).
/// 2. **`/homeless-shelter` purge**: single-user nix in a microVM can't use the
///    build sandbox, so it uses `/homeless-shelter` as a throwaway `$HOME` during
///    each build and REFUSES to start if a leftover one exists. A build interrupted
///    by the sprite suspending on idle leaves it behind, wedging every later nix
///    build. Remove it up front — `sudo` first (a leftover from the pre-`claim_store`
///    root era is root-owned and a plain `rm` can't touch it), then a plain `rm`.
/// 3. **Empty `.drv` heal** (`nix_store_heal`): the same interruption can leave
///    a zero-byte `.drv` registered in the store, after which every evaluation
///    that touches it fails `file is empty`. Delete those paths (and only their
///    referrers) so nix regenerates them.
///
/// 2 and 3 run only when no nix process is alive (THE-2046): a running build
/// owns `/homeless-shelter`, and a `.drv` mid-write is briefly empty. The guard
/// matches process NAMES (`pgrep -x`), never full command lines: a step
/// script's own `sh -c` argv names `nix develop`, so `pgrep -f` would always
/// find the script itself and the purge would never run.
///
/// Trailing `;` so it composes as a prefix; a no-op for the token half when no
/// token is present.
pub(crate) fn nix_runtime_prelude() -> &'static str {
    concat!(
        "tok=\"${GH_TOKEN:-${GITHUB_TOKEN:-}}\"; \
         if [ -n \"$tok\" ]; then export NIX_CONFIG=\"access-tokens = github.com=$tok\"; fi; ",
        nix_heal_sh!(),
    )
}

/// The direnv library hook the `direnv` step installs as
/// `~/.config/direnv/lib/thegn-nix-heal.sh`. direnv sources `lib/*.sh` before
/// every `.envrc`, so an in-pane `use flake` after an interrupted build heals
/// the store and `/homeless-shelter` exactly like a provisioning step does.
const DIRENV_HEAL_LIB: &str = concat!(
    "# thegn: heal a wedged single-user Nix before direnv builds (THE-2046).\n",
    nix_heal_sh!(),
);

/// Install direnv + hook it into the login shells, so entering the workdir
/// activates the flake devShell exactly like local. Idempotent.
fn direnv_install_script() -> String {
    // apt-FIRST: a fresh sprite's freshly-claimed Nix store makes `nix profile
    // install` slow (~10s+); the native package is ~2s and direnv is just a PATH
    // shim (no Nix integration needed here). Nix is the fallback for base images
    // without apt.
    // The rc-hook append is BEST-EFFORT: a read-only login rc (home-manager /
    // nix-managed `~/.zshrc`, as on the sprite login user) makes `>> "$rc"` fail
    // "Permission denied" — which used to exit the whole step 1 and paint a red X
    // on the loading screen, even though direnv installed fine AND the hook is
    // redundant there (the pane enters direnv via `direnv exec`, see
    // `shell_snippet`). So: skip an unwritable rc (`[ -w ]`), swallow any write
    // error, and end on `true` so the step only fails if direnv itself is unusable.
    format!(
        "{}command -v direnv >/dev/null 2>&1 \
           || (export DEBIAN_FRONTEND=noninteractive; apt-get update -y >/dev/null 2>&1 && apt-get install -y direnv >/dev/null 2>&1) \
           || nix profile install nixpkgs#direnv 2>/dev/null; \
         for sh in bash zsh; do \
           rc=\"$HOME/.${{sh}}rc\"; \
           [ -e \"$rc\" ] || touch \"$rc\" 2>/dev/null || true; \
           [ -w \"$rc\" ] || continue; \
           grep -q 'direnv hook' \"$rc\" 2>/dev/null || \
             printf '\\neval \"$(direnv hook %s)\"\\n' \"$sh\" >> \"$rc\" 2>/dev/null || true; \
         done; \
         lib=\"${{XDG_CONFIG_HOME:-$HOME/.config}}/direnv/lib\"; \
         mkdir -p \"$lib\" 2>/dev/null && printf '%s\\n' '{lib}' > \"$lib/thegn-nix-heal.sh\" 2>/dev/null || true; \
         true",
        nix_runtime_prelude(),
        lib = DIRENV_HEAL_LIB,
    )
}

/// The **repo-independent** provisioning prefix as `(step id, script)` pairs —
/// what `thegn env image bake` runs on a throwaway VPS before snapshotting
/// it into a reusable base image: the Nix install (the multi-minute step every
/// cold provision pays) + direnv. Deliberately NO workspace/clone/dotfiles —
/// those are per-worktree and still run at provision time (the bake never
/// writes the provision marker).
pub fn bake_scripts(
    installer: crate::config::NixInstaller,
    parallel: Option<u32>,
) -> Vec<(&'static str, String)> {
    vec![
        (
            "nix",
            nix_install_script(installer, parallel, None, None, None),
        ),
        ("direnv", direnv_install_script()),
    ]
}

/// Prepare the target-side dev environment so the first interactive shell is
/// useful. With direnv+flake we evaluate once; a flake devShell (even
/// alongside a `devenv.nix`) warms with `nix develop`; only a PURE-devenv repo
/// (no flake devShell) installs the heavy `devenv` CLI; otherwise `nix-shell`
/// (classic) or `nix develop`.
fn devshell_warm_script(workdir: &str, req: &EnvRequirements) -> String {
    let wd = sh_quote(workdir);
    let tok = nix_runtime_prelude();
    // `set +e`: this is a BEST-EFFORT pre-warm (the devShell builds lazily in the
    // pane if this is skipped), so no sub-command failure may abort it — the
    // sandbox's `/bin/sh -l` can run with `set -e`, under which an unguarded group
    // failure (e.g. `nix profile install devenv`) aborts the otherwise
    // `|| true`-terminated script and surfaces a spurious "exit 2". Source both nix
    // profiles + a trailing `true` so the step always reports success.
    let nixsh = format!(
        "set +e; {tok}[ -r \"$HOME/.nix-profile/etc/profile.d/nix.sh\" ] && . \"$HOME/.nix-profile/etc/profile.d/nix.sh\" 2>/dev/null || true; \
         [ -r /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh ] && . /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh 2>/dev/null || true; \
         export PATH=\"$HOME/.nix-profile/bin:/nix/var/nix/profiles/default/bin:$PATH\""
    );
    // Each arm picks the warm command (its real exit status kept). The shared
    // tail runs it with output captured; on `file is empty` (a zero-byte `.drv`
    // left by an interrupted build) it heals the store and retries once
    // (THE-2046). The step still always ends `true`.
    let warm = if req.direnv && req.direnv_uses_flake {
        "direnv exec . true || nix develop --command true"
    } else if req.devenv && !req.nix_flake_devshell {
        // Pure-devenv (no flake devShell to fall back on): the `devenv` CLI is the
        // only entry, so install it if absent. NOTE this `nix profile install` is
        // heavy — a repo that ALSO ships a flake devShell skips it and warms via
        // `nix develop` below (the flake gives the same toolchain without paying
        // for devenv's closure, whose from-source realise can OOM-kill — exit 137,
        // which restarts the VM — or time out a pooled provider microVM and paint
        // a spurious "Build dev shell" failure on the loading screen).
        "{ command -v devenv >/dev/null 2>&1 || nix profile install nixpkgs#devenv; }; devenv shell true"
    } else if req.nix_classic {
        // Classic (non-flake) nix: `shell.nix`/`default.nix` — warm with
        // `nix-shell`, which evaluates exactly what an interactive shell will.
        "nix-shell --run true"
    } else {
        "nix develop --command true"
    };
    format!(
        "{nixsh}; cd {wd} 2>/dev/null || true; \
         thegn_warm() {{ {warm}; }}; \
         out=$(thegn_warm 2>&1) || {{ printf '%s\\n' \"$out\" | grep -q 'file is empty' && nix_idle && nix_store_heal && thegn_warm >/dev/null 2>&1; }}; \
         true"
    )
}

/// Push the project's built devShell closure to a binary cache so later sandboxes
/// substitute it instead of rebuilding. Best-effort (a missing signing key or an
/// unreachable cache must not fail provisioning). Resolves the current system so
/// the right `devShells.<system>.default` attribute is copied.
fn cache_push_script(workdir: &str, cache: &BinaryCache) -> String {
    let wd = sh_quote(workdir);
    let url = sh_quote(cache.url.trim());
    format!(
        "{tok}[ -r \"$HOME/.nix-profile/etc/profile.d/nix.sh\" ] && . \"$HOME/.nix-profile/etc/profile.d/nix.sh\" 2>/dev/null || true; \
         export PATH=\"$HOME/.nix-profile/bin:$PATH\"; \
         cd {wd} && sys=$(nix eval --impure --raw --expr builtins.currentSystem 2>/dev/null || echo x86_64-linux); \
         nix copy --to {url} \".#devShells.$sys.default\" 2>/dev/null || true",
        tok = nix_runtime_prelude(),
    )
}

/// Host-parity (experimental): make the host `/nix/store` closure that the
/// dotfiles reference resolvable in the sandbox, so the EXACT host rc works. Best
/// effort — realise each detected root via the configured cache (else the default
/// substituters, which cover most nixpkgs tools). Per-path `|| true` so a missing
/// path degrades the rc gracefully instead of wedging provisioning.
fn home_substitute_script(roots: &[String], cache: Option<&BinaryCache>) -> String {
    let tok = nix_runtime_prelude();
    let subst = match cache.filter(|c| !c.url.trim().is_empty()) {
        Some(c) => format!(
            " --option extra-substituters {} --option extra-trusted-public-keys {}",
            sh_quote(c.url.trim()),
            sh_quote(c.key.trim()),
        ),
        None => String::new(),
    };
    let realise: String = roots
        .iter()
        .map(|r| {
            format!(
                "nix-store --realise{subst} {} 2>/dev/null || true; ",
                sh_quote(r)
            )
        })
        .collect();
    format!(
        "{tok}[ -r \"$HOME/.nix-profile/etc/profile.d/nix.sh\" ] && . \"$HOME/.nix-profile/etc/profile.d/nix.sh\" 2>/dev/null || true; \
         export PATH=\"$HOME/.nix-profile/bin:$PATH\"; {realise}true"
    )
}

/// Host-parity fallback: rebuild the user's home environment in the sandbox from
/// their home-manager flake. Heavier than the closure copy and the store hashes
/// may diverge from the host, so home-manager renders its OWN rc (the host rc is
/// skipped by the caller in this mode). Best-effort.
fn home_switch_script(flake: &str) -> String {
    let tok = nix_runtime_prelude();
    format!(
        "{tok}[ -r \"$HOME/.nix-profile/etc/profile.d/nix.sh\" ] && . \"$HOME/.nix-profile/etc/profile.d/nix.sh\" 2>/dev/null || true; \
         export PATH=\"$HOME/.nix-profile/bin:$PATH\"; \
         nix run home-manager -- switch --flake {} 2>/dev/null || true",
        sh_quote(flake),
    )
}

/// Host-parity p2p: after the host has pushed the closure into the sandbox store,
/// `nix profile install` the pushed store paths so the shell + prompt tools
/// (zsh/starship/atuin/…) resolve by name on `PATH`. The paths already exist in
/// the store (the push delivered their full closure), so this is a cheap symlink
/// into `~/.nix-profile`, not a download. Per-path `|| true` so one bad path can't
/// abort the rest.
fn home_profile_install_script(installs: &[String]) -> String {
    let tok = nix_runtime_prelude();
    let body: String = installs
        .iter()
        .map(|p| format!("nix profile install {} 2>/dev/null || true; ", sh_quote(p)))
        .collect();
    format!(
        "{tok}[ -r \"$HOME/.nix-profile/etc/profile.d/nix.sh\" ] && . \"$HOME/.nix-profile/etc/profile.d/nix.sh\" 2>/dev/null || true; \
         export PATH=\"$HOME/.nix-profile/bin:$PATH\"; {body}true"
    )
}

/// Best-effort native runtimes for detected languages (Ubuntu/apt base).
fn languages_install_script(langs: &[Language]) -> String {
    let mut pkgs: Vec<&str> = Vec::new();
    for l in langs {
        match l {
            Language::Node => pkgs.push("nodejs npm"),
            Language::Python => pkgs.push("python3 python3-pip python3-venv"),
            Language::Rust => pkgs.push("cargo rustc"),
            Language::Go => pkgs.push("golang"),
            Language::Ruby => pkgs.push("ruby"),
            // No deno package in the Debian/Ubuntu repos — the SynthNix tier
            // covers it; the apt fallback can only skip it.
            Language::Deno => {}
            Language::Jvm => pkgs.push("default-jdk"),
        }
    }
    if pkgs.is_empty() {
        return "true".into();
    }
    format!(
        "export DEBIAN_FRONTEND=noninteractive; apt-get update -y && apt-get install -y {} || true",
        pkgs.join(" ")
    )
}

/// Map a tool name to its native-package-manager package where it differs from
/// the common/nixpkgs name (the apt convention; close enough for apk/dnf).
fn native_pkg_name(tool: &str) -> &str {
    match tool {
        "fd" => "fd-find",
        "rg" => "ripgrep",
        "bat" => "bat",
        "nvim" => "neovim",
        other => other,
    }
}

/// Install the personal CLI tools, **Nix-first** (`nix profile install
/// nixpkgs#<tool>`, consistent names across any base image) with a **native
/// package-manager fallback** (apt/apk/dnf). Per-tool best-effort: a tool that
/// can't be installed warns and is skipped rather than failing the step.
fn tools_install_script(tools: &[String]) -> String {
    // Build the `case` mapping nix-name → native-name for the fallback path.
    let cases: String = tools
        .iter()
        .map(|t| {
            let t = t.trim();
            format!("{}) echo {} ;; ", sh_quote(t), sh_quote(native_pkg_name(t)))
        })
        .collect();
    let list: String = tools
        .iter()
        .map(|t| sh_quote(t.trim()))
        .collect::<Vec<_>>()
        .join(" ");
    // POSIX sh; non-fatal per tool; ends with `true` so the step succeeds even if
    // some tools are unavailable.
    format!(
        "have() {{ command -v \"$1\" >/dev/null 2>&1; }}; \
         nat() {{ case \"$1\" in {cases}*) echo \"$1\" ;; esac; }}; \
         nat_install() {{ \
           if have apt-get; then export DEBIAN_FRONTEND=noninteractive; apt-get update -y >/dev/null 2>&1; apt-get install -y \"$@\"; \
           elif have apk; then apk add --no-cache \"$@\"; \
           elif have dnf; then dnf install -y \"$@\"; \
           else return 1; fi; }}; \
         for t in {list}; do \
           if have nix; then nix profile install \"nixpkgs#$t\" 2>/dev/null && continue; fi; \
           nat_install \"$(nat \"$t\")\" 2>/dev/null || echo \"thegn: skipped tool $t (no installer)\" >&2; \
         done; true"
    )
}

/// Coding agents thegn knows how to reproduce in a sandbox. Used to
/// AUTO-DETECT which agents the host has (so a sandbox reaches "exact local
/// parity" without per-sandbox config) — each is probed on the host PATH /
/// config locations. Known installers exist for a subset (see
/// `agents_install_script`); the rest still get their config uploaded
/// ([`agent_config_paths`]) and rely on a `setup` recipe to install the binary.
pub fn known_agents() -> &'static [&'static str] {
    &[
        "claude",
        "codex",
        "pi",
        "hermes",
        "gemini",
        "aider",
        "opencode",
        "amp",
        "goose",
        "cursor-agent",
    ]
}

/// The host config/credential paths (relative to `$HOME`) for a coding agent —
/// uploaded into the sandbox `$HOME` so the agent is logged-in there. Returns
/// `(files, dirs)`. Unknown agents fall back to the conventional dotfile/dir
/// locations so a custom agent (e.g. `hermes`) still gets its config carried.
pub fn agent_config_paths(agent: &str) -> (Vec<String>, Vec<String>) {
    match agent.trim() {
        "claude" | "claude-code" => (
            vec![".claude.json".into()],
            vec![".claude".into(), ".config/claude".into()],
        ),
        "codex" => (vec![], vec![".codex".into(), ".config/codex".into()]),
        // pi keeps its config + user extensions/skills under ~/.pi
        // (e.g. ~/.pi/agent/{extensions,skills}/); carry the whole tree.
        "pi" => (
            vec![".pi.json".into()],
            vec![".pi".into(), ".config/pi".into()],
        ),
        other => (
            vec![format!(".{other}.json")],
            vec![format!(".{other}"), format!(".config/{other}")],
        ),
    }
}

/// The minimal set of files (relative to `$HOME`) that are **sufficient** for
/// an agent to be authenticated and functional in a sandbox — i.e. what must be
/// uploaded even when the full config tree walk runs out of time. This is an
/// explicit allowlist (not a blocklist) so it stays correct regardless of how
/// large the rest of the agent config directory gets.
///
/// It lists only credentials and settings. Caches (e.g. Claude's
/// `mcp-needs-auth-cache.json`) are not needed to authenticate, so they are
/// left to the best-effort tree walk.
pub fn agent_auth_critical_files(agent: &str) -> Vec<String> {
    match agent.trim() {
        "claude" | "claude-code" => vec![
            ".claude.json".into(),
            ".claude/.credentials.json".into(),
            ".claude/settings.json".into(),
            ".claude/settings.local.json".into(),
        ],
        // `auth.json` holds codex's login and `config.toml` its config; the two
        // json paths are older layouts kept for compatibility.
        "codex" => vec![
            ".codex/auth.json".into(),
            ".codex/config.toml".into(),
            ".codex/config.json".into(),
            ".config/codex/config.json".into(),
        ],
        "pi" => vec![".pi.json".into()],
        "hermes" => vec![
            ".hermes/auth.json".into(),
            ".hermes/config.yaml".into(),
            ".hermes/.env".into(),
        ],
        other => vec![
            format!(".{other}.json"),
            format!(".{other}/config.json"),
            format!(".config/{other}/config.json"),
        ],
    }
}

/// Install known coding-agent CLIs (best-effort, idempotent). Known agents get a
/// real installer; unknown ones are left to the user's `setup`/`tools` (their
/// config is still uploaded). Per-agent failures are non-fatal and logged to
/// `~/.cache/thegn-agent-install.log` in the sandbox. `thegn agent run`'s
/// preflight names the missing CLI, and the log says why. The real cause of a
/// live exit 127 is UNVERIFIED.
fn agents_install_script(agents: &[String]) -> String {
    let mut lines = vec![
        "have() { command -v \"$1\" >/dev/null 2>&1; }".to_string(),
        "log=\"$HOME/.cache/thegn-agent-install.log\"".to_string(),
        "mkdir -p \"$HOME/.cache\" 2>/dev/null".to_string(),
        "true 2>/dev/null >\"$log\" || log=/dev/null".to_string(),
        "npm_g() { if ! have npm; then echo \"npm not found: cannot install $*\" >>\"$log\"; return 1; fi; npm install -g --no-audit --no-fund --no-update-notifier --prefer-offline --loglevel=error \"$@\" >>\"$log\" 2>&1; }".to_string(),
        "t0=$(date +%s 2>/dev/null || echo 0)".to_string(),
        "pkgs=\"\"".to_string(),
        "hermes_pid=\"\"".to_string(),
        "missing=\"\"".to_string(),
    ];
    let mut known_programs = Vec::new();
    for a in agents {
        let line = match a.trim() {
            "claude" | "claude-code" => {
                known_programs.push("claude");
                "have claude || pkgs=\"$pkgs @anthropic-ai/claude-code\"".to_string()
            }
            "codex" => {
                known_programs.push("codex");
                "have codex || pkgs=\"$pkgs @openai/codex\"".to_string()
            }
            // thegn's own agent; pi >=0.80 bundles its extension deps.
            "pi" => {
                known_programs.push("pi");
                "have pi || pkgs=\"$pkgs @earendil-works/pi-coding-agent\"".to_string()
            }
            // Nous Research hermes-agent (Python/uv) — its own curl installer.
            "hermes" => {
                known_programs.push("hermes");
                "{ have hermes || (curl -fsSL https://hermes-agent.nousresearch.com/install.sh | bash) >>\"$log\" 2>&1 || true; echo \"thegn-agent-install: hermes $(( $(date +%s) - t0 ))s\" >>\"$log\"; } & hermes_pid=$!".to_string()
            }
            other => format!("# {other}: install via [sandbox.home] setup/tools"),
        };
        lines.push(line);
    }
    // shellcheck disable=SC2086: package names are intentionally word-split.
    // One npm per package, all at once: the three CLIs share nothing but the
    // global prefix, and a cold sprite spends most of the install on network
    // fetches, so the wall time drops to the slowest package. Any failure
    // (including a prefix race) falls back to the old sequential install, so
    // the worst case is the previous behaviour.
    lines.push("if [ -n \"$pkgs\" ]; then t1=$(date +%s 2>/dev/null || echo 0); pids=\"\"; for p in $pkgs; do npm_g \"$p\" & pids=\"$pids $!\"; done; failed=0; for pid in $pids; do wait \"$pid\" || failed=1; done; if [ \"$failed\" = 1 ]; then npm_g $pkgs || for p in $pkgs; do npm_g \"$p\" || true; done; fi; echo \"thegn-agent-install: npm$pkgs $(( $(date +%s) - t1 ))s\" >>\"$log\"; fi".into());
    lines.push("[ -z \"$hermes_pid\" ] || wait \"$hermes_pid\"".into());
    for program in known_programs {
        lines.push(format!("have {program} || missing=\"$missing {program}\""));
    }
    lines.push("if [ -n \"$missing\" ]; then echo \"thegn: agent CLI not installed:$missing (log: $log)\" >>\"$log\"; echo \"thegn: agent CLI not installed:$missing (log: $log)\" >&2; fi".into());
    lines.push("true".into());
    lines.join("; ")
}

/// Configure git to authenticate HTTPS GitHub clone/push using a token from the
/// environment, IF one is present (GH_TOKEN, else GITHUB_TOKEN). Runtime-
/// conditional + idempotent; the helper reads the token at git-run time (so it's
/// never baked into config or the checkpoint). Also marks the workdir a safe
/// directory and pins `core.sshCommand` to `StrictHostKeyChecking=accept-new` so
/// a fresh sandbox with no `known_hosts` doesn't fail `Host key verification
/// failed` on first contact.
///
/// When a token IS present it ALSO rewrites SSH GitHub origins to HTTPS
/// (`url.https://github.com/.insteadOf git@github.com:` and the `ssh://` form):
/// a sandbox has the token but NOT a GitHub-authorized SSH key, so an
/// `git@github.com:…` origin (the common local remote) would otherwise clone-
/// fail with `Permission denied (publickey)` even after the host key is
/// trusted. The rewrite routes it through the token credential helper instead.
/// Without a token the origin is left untouched (public HTTPS / agent-forwarded
/// SSH still work). All persisted to `~/.gitconfig` so later pushes/pulls from
/// the shell work too.
///
/// `git_token = false` (the default policy) omits the token helper and the
/// https rewrite entirely: the sandbox gets no git credential from thegn.
fn git_auth_script(git_token: bool) -> String {
    // The in-sandbox `core.sshCommand` host-key policy comes from the one
    // chokepoint (`SandboxBootstrap` class), never a literal here — see
    // `crate::hostkey` and the host-key ratchet.
    let ssh_cmd = format!(
        "ssh {}",
        crate::hostkey::host_key_opts_str(
            crate::hostkey::HostKeyClass::SandboxBootstrap,
            &crate::hostkey::HostKeyContext::default(),
        )
    );
    let mut s = ensure_git_script();
    if git_token {
        s.push_str("tok=\"${GH_TOKEN:-${GITHUB_TOKEN:-}}\"; ");
    }
    s.push_str(
        "git config --global --add safe.directory '*' 2>/dev/null || true; \
         git config --global core.sshCommand '",
    );
    s.push_str(&ssh_cmd);
    s.push_str(
        "' 2>/dev/null || true; \
         [ -n \"${THEGN_GIT_NAME:-}\" ] && git config --global user.name \"$THEGN_GIT_NAME\" 2>/dev/null || true; \
         [ -n \"${THEGN_GIT_EMAIL:-}\" ] && git config --global user.email \"$THEGN_GIT_EMAIL\" 2>/dev/null || true; ",
    );
    if git_token {
        s.push_str(
            "if [ -n \"$tok\" ]; then \
           git config --global credential.helper \
             '!f() { test \"$1\" = get && printf \"username=x-access-token\\npassword=%s\\n\" \"${GH_TOKEN:-$GITHUB_TOKEN}\"; }; f'; \
           git config --global --unset-all url.'https://github.com/'.insteadOf 2>/dev/null || true; \
           git config --global --add url.'https://github.com/'.insteadOf 'git@github.com:' 2>/dev/null || true; \
           git config --global --add url.'https://github.com/'.insteadOf 'ssh://git@github.com/' 2>/dev/null || true; \
         fi; ",
        );
    }
    s.push_str("true");
    s
}

/// Install git (plus CA certificates and an ssh client) when the sandbox image
/// lacks it, through whichever native package manager exists (as root, or via
/// passwordless sudo). A stock provider image (fly's default `ubuntu:24.04`)
/// ships without git, and every `git config` in [`git_auth_script`] is
/// best-effort, so without this the credentials step silently configured
/// nothing and the clone died `git: not found` (exit 127). Fatal, with a
/// message naming the cause, when git is still missing afterwards.
fn ensure_git_script() -> String {
    "if ! command -v git >/dev/null 2>&1; then \
       if [ \"$(id -u)\" = 0 ]; then S=; \
       elif command -v sudo >/dev/null 2>&1 && sudo -n true 2>/dev/null; then S='sudo -n'; \
       else S=; fi; \
       echo 'thegn: git is not installed in the sandbox; installing it' >&2; \
       if command -v apt-get >/dev/null 2>&1; then \
         $S env DEBIAN_FRONTEND=noninteractive apt-get update -y >/dev/null \
           && $S env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends git ca-certificates openssh-client >/dev/null; \
       elif command -v apk >/dev/null 2>&1; then $S apk add --no-cache git ca-certificates openssh-client >/dev/null; \
       elif command -v dnf >/dev/null 2>&1; then $S dnf install -y git ca-certificates openssh-clients >/dev/null; \
       elif command -v yum >/dev/null 2>&1; then $S yum install -y git ca-certificates openssh-clients >/dev/null; \
       fi; \
       command -v git >/dev/null 2>&1 || { echo 'thegn: git is missing in the sandbox and no package manager could install it' >&2; exit 1; }; \
     fi; "
        .to_string()
}

/// Clone a dotfiles repo into `~/.dotfiles` (idempotent) and run its bootstrap
/// (`install.sh`/`bootstrap.sh`/`setup.sh`) if present. Non-fatal.
fn dotfiles_repo_script(repo: &str) -> String {
    let rq = sh_quote(repo);
    format!(
        "d=\"$HOME/.dotfiles\"; \
         [ -d \"$d/.git\" ] || git clone {rq} \"$d\" 2>&1; \
         cd \"$d\" 2>/dev/null && for s in install.sh bootstrap.sh setup.sh; do \
           [ -f \"$s\" ] && sh \"$s\" && break; \
         done; true"
    )
}

#[cfg(test)]
mod tests {

    #[test]
    fn detect_probe_round_trips_the_local_detector_shape() {
        let out =
            "FLAKE_DEVSHELL=1\nDIRENV=1\nDIRENV_FLAKE=1\nLANG_PYTHON=1\nLANG_DENO=1\nLANG_JVM=1\n";
        let req = detect_from_probe(out);
        assert!(req.nix_flake_devshell && req.direnv && req.direnv_uses_flake);
        assert_eq!(
            req.languages,
            vec![Language::Python, Language::Deno, Language::Jvm]
        );
        assert_eq!(req.tier(), Tier::Nix);

        let bare = detect_from_probe("junk\nWHAT=1\n");
        assert_eq!(bare.tier(), Tier::Bare);
        let synth = detect_from_probe("LANG_NODE=1\n");
        assert_eq!(synth.tier(), Tier::SynthNix);
        let classic = detect_from_probe("NIX_CLASSIC=1\n");
        assert!(classic.nix_classic);
        assert_eq!(classic.tier(), Tier::Nix);
    }

    #[test]
    fn local_and_remote_toolchain_detection_have_full_name_parity() {
        let d = tmp("toolchain_parity");
        let config = [
            "mise.toml",
            ".mise.toml",
            "mise.local.toml",
            "mise/config.toml",
            ".mise/config.toml",
            ".config/mise.toml",
            ".config/mise/config.toml",
            "conf.d/20-last.toml",
            "conf.d/01-first.toml",
            "mise.ci.toml",
        ];
        let pins = [
            ".tool-versions",
            ".nvmrc",
            ".node-version",
            ".python-version",
            ".ruby-version",
            ".go-version",
            ".java-version",
        ];
        for name in config.iter().chain(&pins) {
            if let Some(parent) = d.join(name).parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            write(&d, name, "declared");
        }
        let local = detect_with_mise_env(&d, Some("ci"));
        let probe = config
            .iter()
            .map(|name| format!("TOOLCHAIN_CONFIG={name}"))
            .chain(pins.iter().map(|name| format!("TOOLCHAIN_PIN={name}")))
            .collect::<Vec<_>>()
            .join("\n");
        let remote = detect_from_probe(&probe);
        assert_eq!(local.toolchain_files, remote.toolchain_files);
        assert!(local.tool_versions && remote.tool_versions);

        // Pin the probe's discovery vocabulary beside the parity assertion so
        // extending only the local list cannot make this test pass vacuously.
        for name in config.iter().chain(&pins) {
            if name.starts_with("conf.d/") || *name == "mise.ci.toml" {
                continue;
            }
            assert!(DETECT_PROBE_SCRIPT.contains(name), "probe omits {name}");
        }
        assert!(DETECT_PROBE_SCRIPT.contains("conf.d/*.toml"));
        assert!(DETECT_PROBE_SCRIPT.contains("MISE_ENV"));
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn plan_opts_default_is_bare_and_clone_free() {
        let opts = PlanOpts::default();
        assert!(opts.origin.is_none());
        let plan = plan(&EnvRequirements::default(), &opts);
        assert!(plan.steps.iter().all(|s| s.id != "clone"));
    }
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!("tg-envplan-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d); // best-effort: test setup: fresh scratch dir
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn detect_nix_flake_and_direnv() {
        let d = tmp("nix_flake");
        write(&d, "flake.nix", "{ outputs = { devShells.default = 1; }; }");
        write(&d, ".envrc", "use flake\n");
        let r = detect(&d);
        assert!(r.nix_flake_devshell);
        assert!(r.direnv);
        assert!(r.direnv_uses_flake);
        assert_eq!(r.tier(), Tier::Nix);
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn detect_devenv_is_nix_tier() {
        let d = tmp("devenv");
        write(&d, "devenv.nix", "{ }\n");
        let r = detect(&d);
        assert!(r.devenv);
        assert_eq!(r.tier(), Tier::Nix);
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn detect_languages_and_tool_versions() {
        let d = tmp("languages");
        write(&d, "package.json", "{}");
        write(&d, "Cargo.toml", "[package]");
        write(&d, ".tool-versions", "node 20\n");
        let r = detect(&d);
        assert!(r.languages.contains(&Language::Node));
        assert!(r.languages.contains(&Language::Rust));
        assert!(r.tool_versions);
        // tool-versions outranks bare language manifests.
        assert_eq!(r.tier(), Tier::ToolVersions);
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn bare_repo_is_bare_tier() {
        let d = tmp("bare");
        write(&d, "README.md", "hi");
        assert_eq!(detect(&d).tier(), Tier::Bare);
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn detect_deno_json() {
        let d = tmp("deno_json");
        write(&d, "deno.json", "{}");
        let r = detect(&d);
        assert_eq!(r.languages, vec![Language::Deno]);
        assert_eq!(r.tier(), Tier::SynthNix);
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn detect_deno_jsonc() {
        let d = tmp("deno_jsonc");
        write(&d, "deno.jsonc", "// jsonc\n{}");
        assert_eq!(detect(&d).languages, vec![Language::Deno]);
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn detect_jvm_from_build_sbt() {
        let d = tmp("jvm_sbt");
        write(&d, "build.sbt", "name := \"x\"\n");
        assert_eq!(detect(&d).languages, vec![Language::Jvm]);
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn detect_jvm_from_nested_build_properties() {
        let d = tmp("jvm_props");
        std::fs::create_dir_all(d.join("project")).unwrap();
        write(&d, "project/build.properties", "sbt.version=1.10.0\n");
        let r = detect(&d);
        assert_eq!(r.languages, vec![Language::Jvm]);
        assert_eq!(r.tier(), Tier::SynthNix);
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn detect_jvm_from_scala_version() {
        let d = tmp("jvm_scala_version");
        write(&d, ".scala-version", "3.4.2\n");
        assert_eq!(detect(&d).languages, vec![Language::Jvm]);
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn detect_shell_nix_only_is_classic_nix_tier() {
        let d = tmp("shell_nix");
        write(
            &d,
            "shell.nix",
            "{ pkgs ? import <nixpkgs> {} }: pkgs.mkShell {}",
        );
        // A language manifest alongside must not outrank the classic nix shell.
        write(&d, "package.json", "{}");
        let r = detect(&d);
        assert!(r.nix_classic);
        assert!(!r.nix_flake_devshell && !r.devenv);
        assert_eq!(r.tier(), Tier::Nix);
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn detect_default_nix_is_classic_nix_tier() {
        let d = tmp("default_nix");
        write(&d, "default.nix", "{ }: { }");
        let r = detect(&d);
        assert!(r.nix_classic);
        assert_eq!(r.tier(), Tier::Nix);
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn flake_devshell_or_devenv_outrank_classic_nix() {
        let d = tmp("flake_over_classic");
        write(&d, "flake.nix", "{ outputs = { devShells.default = 1; }; }");
        write(&d, "shell.nix", "{ }: { }");
        let r = detect(&d);
        assert!(r.nix_flake_devshell && !r.nix_classic);
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&d);
        let d2 = tmp("devenv_over_classic");
        write(&d2, "devenv.nix", "{ }\n");
        write(&d2, "default.nix", "{ }: { }");
        let r2 = detect(&d2);
        assert!(r2.devenv && !r2.nix_classic);
        assert_eq!(r2.tier(), Tier::Nix);
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&d2);
    }

    #[test]
    fn tier_ranking_matrix() {
        // Nix (incl. classic) > ToolVersions > SynthNix > Bare.
        let both = EnvRequirements {
            nix_classic: true,
            tool_versions: true,
            languages: vec![Language::Node],
            ..Default::default()
        };
        assert_eq!(both.tier(), Tier::Nix);
        let tv = EnvRequirements {
            tool_versions: true,
            languages: vec![Language::Node],
            ..Default::default()
        };
        assert_eq!(tv.tier(), Tier::ToolVersions);
        let langs = EnvRequirements {
            languages: vec![Language::Python, Language::Deno],
            ..Default::default()
        };
        assert_eq!(langs.tier(), Tier::SynthNix);
        assert_eq!(EnvRequirements::default().tier(), Tier::Bare);
    }

    #[test]
    fn classic_nix_warm_uses_nix_shell() {
        let req = EnvRequirements {
            nix_classic: true,
            ..Default::default()
        };
        let s = devshell_warm_script("/workspace", &req);
        assert!(s.contains("nix-shell --run true"), "{s}");
        assert!(!s.contains("nix develop"), "{s}");
        // ...and it flows through a full plan's devshell step.
        let p = plan(&req, &PlanOpts::default());
        assert_eq!(p.tier, Tier::Nix);
        let dev = p.steps.iter().find(|s| s.id == "devshell").unwrap();
        assert!(matches!(&dev.kind, StepKind::Exec(s) if s.contains("nix-shell --run true")));
    }

    #[test]
    fn flake_devshell_warm_prefers_nix_develop_over_devenv_install() {
        // A repo can ship BOTH a flake devShell and a devenv.nix (devenv's flake
        // integration). The warm must use the flake (`nix develop`) and NEVER the
        // heavy `nix profile install nixpkgs#devenv`, whose closure can OOM-kill /
        // time out a pooled provider microVM and paint a spurious splash failure.
        let req = EnvRequirements {
            nix_flake_devshell: true,
            devenv: true,
            ..Default::default()
        };
        let s = devshell_warm_script("/workspace", &req);
        assert!(s.contains("nix develop --command true"), "{s}");
        assert!(!s.contains("nix profile install nixpkgs#devenv"), "{s}");
        // Pure-devenv (no flake devShell) still installs the CLI — it's the only entry.
        let pure = EnvRequirements {
            devenv: true,
            ..Default::default()
        };
        let p = devshell_warm_script("/workspace", &pure);
        assert!(p.contains("nix profile install nixpkgs#devenv"), "{p}");
    }

    #[test]
    fn plan_synth_nix_installs_nix_then_synthesizes_toolchain() {
        let req = EnvRequirements {
            languages: vec![Language::Python, Language::Node],
            ..Default::default()
        };
        let p = plan(&req, &PlanOpts::default());
        assert_eq!(p.tier, Tier::SynthNix);
        let ids: Vec<&str> = p.steps.iter().map(|s| s.id.as_str()).collect();
        let nix = ids.iter().position(|i| *i == "nix").expect("nix step");
        let tc = ids
            .iter()
            .position(|i| *i == "toolchain")
            .expect("toolchain step");
        assert!(nix < tc, "nix before toolchain: {ids:?}");
        assert!(!ids.contains(&"languages"), "no apt fallback: {ids:?}");
        // The toolchain step targets the hashed synth dir for this package set.
        let pkgs = crate::toolchain::packages_for(&req.languages, &PlanOpts::default().toolchain);
        let dir = crate::toolchain::synth_dir(&pkgs);
        let StepKind::Exec(script) = &p.steps[tc].kind else {
            panic!("toolchain step is Exec");
        };
        assert!(script.contains(&dir), "targets {dir}: {script}");
    }

    #[test]
    fn plan_toolchain_mode_mise_keeps_tier_without_implicit_install() {
        let req = EnvRequirements {
            languages: vec![Language::Python],
            ..Default::default()
        };
        let opts = PlanOpts {
            toolchain: crate::toolchain::ToolchainConfig {
                mode: crate::toolchain::ToolchainMode::Mise,
                ..Default::default()
            },
            ..Default::default()
        };
        let p = plan(&req, &opts);
        assert_eq!(p.tier, Tier::ToolVersions);
        let ids: Vec<&str> = p.steps.iter().map(|s| s.id.as_str()).collect();
        assert!(!ids.contains(&"mise"), "install must be explicit: {ids:?}");
        assert!(!ids.contains(&"toolchain") && !ids.contains(&"languages"));
    }

    #[test]
    fn plan_toolchain_mode_off_falls_back_to_languages_tier() {
        let req = EnvRequirements {
            languages: vec![Language::Python],
            ..Default::default()
        };
        let opts = PlanOpts {
            toolchain: crate::toolchain::ToolchainConfig {
                mode: crate::toolchain::ToolchainMode::Off,
                ..Default::default()
            },
            ..Default::default()
        };
        let p = plan(&req, &opts);
        assert_eq!(p.tier, Tier::Languages);
        let ids: Vec<&str> = p.steps.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&"languages"), "apt fallback: {ids:?}");
        assert!(!ids.contains(&"toolchain") && !ids.contains(&"nix"));
    }

    #[test]
    fn plan_synth_nix_requires_allow_nix() {
        let req = EnvRequirements {
            languages: vec![Language::Go],
            ..Default::default()
        };
        let opts = PlanOpts {
            allow_nix: false,
            ..Default::default()
        };
        let p = plan(&req, &opts);
        assert_eq!(p.tier, Tier::Languages);
        assert!(!p.steps.iter().any(|s| s.id == "nix" || s.id == "toolchain"));
    }

    #[test]
    fn plan_synth_nix_deduplicates_host_parity_nix_install() {
        // SynthNix already installs nix; host-parity must not add a second step.
        let req = EnvRequirements {
            languages: vec![Language::Node],
            ..Default::default()
        };
        let opts = PlanOpts {
            strategy: crate::config::ShellStrategy::HostParity,
            home_store_roots: vec!["/nix/store/abc-zsh".into()],
            ..Default::default()
        };
        let p = plan(&req, &opts);
        assert_eq!(
            p.steps.iter().filter(|s| s.id == "nix").count(),
            1,
            "exactly one nix install"
        );
    }

    #[test]
    fn plan_nix_repo_has_clone_nix_devshell_checkpoint_in_order() {
        let req = EnvRequirements {
            nix_flake_devshell: true,
            direnv: true,
            direnv_uses_flake: true,
            ..Default::default()
        };
        let opts = PlanOpts {
            origin: Some("git@github.com:o/r.git".into()),
            ..Default::default()
        };
        let p = plan(&req, &opts);
        let ids: Vec<&str> = p.steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "workspace",
                "git_auth",
                "clone",
                "git_exclude",
                "nix",
                "nix_verify",
                "direnv",
                "devshell",
                "checkpoint"
            ]
        );
        assert_eq!(p.tier, Tier::Nix);
        // The clone step cds into the workdir first.
        let clone = p.steps.iter().find(|s| s.id == "clone").unwrap();
        assert!(matches!(&clone.kind, StepKind::Exec(s) if s.starts_with("cd ")));
    }

    #[test]
    fn nix_step_gates_on_real_egress_and_retries_fetch() {
        // Regression: a cold Fly microVM answers DNS before outbound TLS is up, so a
        // DNS-only readiness gate + a no-retry `curl -fsSL` fired the install into a
        // dead network and failed silently (the cold-boot "exit 127"). The gate must
        // probe ACTUAL egress and the fetch must retry.
        let req = EnvRequirements {
            nix_flake_devshell: true,
            ..Default::default()
        };
        let opts = PlanOpts {
            origin: Some("git@github.com:o/r.git".into()),
            ..Default::default()
        };
        let p = plan(&req, &opts);
        let nix = p.steps.iter().find(|s| s.id == "nix").expect("nix step");
        let StepKind::Exec(script) = &nix.kind else {
            panic!("nix step is Exec");
        };
        // Readiness gate probes real reachability, not just DNS resolution.
        assert!(
            script.contains("curl -fsS --max-time 5 -o /dev/null https://nixos.org/nix/install"),
            "readiness gate must probe actual egress: {script}"
        );
        assert!(
            script.contains("getent hosts nixos.org"),
            "readiness gate still resolves DNS first: {script}"
        );
        // The install-script fetch retries transient cold-boot failures.
        assert!(
            script.contains("--retry 5")
                && script.contains("--retry-connrefused")
                && script.contains("--retry-all-errors"),
            "install fetch must retry all transient errors: {script}"
        );
    }

    #[test]
    fn plan_skips_nix_when_disallowed() {
        let req = EnvRequirements {
            nix_flake_devshell: true,
            ..Default::default()
        };
        let opts = PlanOpts {
            allow_nix: false,
            origin: None,
            checkpoint: false,
            ..Default::default()
        };
        let ids: Vec<String> = plan(&req, &opts).steps.into_iter().map(|s| s.id).collect();
        assert_eq!(ids, ["workspace"]); // no clone (no origin), no nix, no checkpoint
    }

    #[test]
    fn plan_dotfiles_step_present_when_requested() {
        let opts = PlanOpts {
            dotfiles: vec![".zshrc".into(), ".gitconfig".into()],
            checkpoint: false,
            ..Default::default()
        };
        let p = plan(&EnvRequirements::default(), &opts);
        let df = p.steps.iter().find(|s| s.id == "dotfiles").unwrap();
        assert!(matches!(&df.kind, StepKind::Dotfiles(v) if v.len() == 2));
    }

    #[test]
    fn plan_personal_layer_steps_after_toolchain_before_checkpoint() {
        let req = EnvRequirements {
            nix_flake_devshell: true,
            direnv: true,
            direnv_uses_flake: true,
            ..Default::default()
        };
        let opts = PlanOpts {
            origin: Some("git@github.com:o/r.git".into()),
            tools: vec!["fd".into(), "fzf".into()],
            dotfiles_repo: "git@github.com:me/dots".into(),
            setup: vec!["echo hi".into()],
            dotfiles: vec![".zshrc".into()],
            ..Default::default()
        };
        let p = plan(&req, &opts);
        let ids: Vec<&str> = p.steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "workspace",
                "git_auth",
                "clone",
                "git_exclude",
                "nix",
                "nix_verify",
                "direnv",
                "devshell",
                "tools",
                "dotfiles_repo",
                "setup",
                "dotfiles",
                "checkpoint"
            ]
        );
    }

    #[test]
    fn plan_opts_default_strategy_is_portable() {
        assert_eq!(
            PlanOpts::default().strategy,
            crate::config::ShellStrategy::Portable
        );
    }

    #[test]
    fn plan_ignores_legacy_host_devshell_push() {
        let req = EnvRequirements {
            nix_flake_devshell: true,
            direnv: true,
            direnv_uses_flake: true,
            ..Default::default()
        };
        let opts = PlanOpts {
            push_devshell: true,
            ..Default::default()
        };
        let p = plan(&req, &opts);
        let ids: Vec<&str> = p.steps.iter().map(|s| s.id.as_str()).collect();
        assert!(
            !ids.contains(&"devshell_push"),
            "legacy host push is inert: {ids:?}"
        );
    }

    #[test]
    fn plan_no_devshell_push_when_off() {
        let req = EnvRequirements {
            nix_flake_devshell: true,
            ..Default::default()
        };
        let p = plan(&req, &PlanOpts::default());
        assert!(!p.steps.iter().any(|s| s.id == "devshell_push"));
    }

    #[test]
    fn plan_skip_devshell_warm_omits_build_without_approval_step() {
        let req = EnvRequirements {
            nix_flake_devshell: true,
            direnv: true,
            direnv_uses_flake: true,
            ..Default::default()
        };
        // Skip drops the from-source BUILD (+ its cache push). It must not add a
        // generated approval command or a host-side closure push.
        let opts = PlanOpts {
            push_devshell: true,
            skip_devshell_warm: true,
            binary_cache: Some(BinaryCache {
                url: "https://cache.example.org".into(),
                key: "k".into(),
                push: true,
            }),
            ..Default::default()
        };
        let p = plan(&req, &opts);
        let ids: Vec<&str> = p.steps.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&"nix"), "nix still installed: {ids:?}");
        assert!(
            !ids.contains(&"devshell") && !ids.contains(&"cache_push"),
            "skip_devshell_warm omits the build + cache push: {ids:?}"
        );
        assert!(
            !ids.contains(&"devshell_push"),
            "host push is removed: {ids:?}"
        );
        assert!(
            !ids.contains(&"direnv_allow"),
            "approval is never generated: {ids:?}"
        );
        assert!(p.steps.iter().all(|step| match &step.kind {
            StepKind::Exec(script) => !script.contains("direnv allow"),
            _ => true,
        }));
    }

    #[test]
    fn plan_warms_devshell_by_default() {
        let req = EnvRequirements {
            nix_flake_devshell: true,
            direnv: true,
            ..Default::default()
        };
        let p = plan(&req, &PlanOpts::default());
        assert!(
            p.steps.iter().any(|s| s.id == "devshell"),
            "devshell warm present by default"
        );
    }

    #[test]
    fn plan_atuin_sync_after_tools_before_checkpoint_when_opted_in() {
        let opts = PlanOpts {
            tools: vec!["atuin".into()],
            atuin: true,
            checkpoint: true,
            ..Default::default()
        };
        let p = plan(&EnvRequirements::default(), &opts);
        let ids: Vec<&str> = p.steps.iter().map(|s| s.id.as_str()).collect();
        let tools = ids.iter().position(|i| *i == "tools").expect("tools");
        let atuin = ids
            .iter()
            .position(|i| *i == "atuin_sync")
            .expect("atuin_sync step present when opted in");
        let chk = ids
            .iter()
            .position(|i| *i == "checkpoint")
            .expect("checkpoint");
        assert!(
            tools < atuin && atuin < chk,
            "atuin_sync after tools, before checkpoint: {ids:?}"
        );
        assert!(matches!(
            p.steps.iter().find(|s| s.id == "atuin_sync").unwrap().kind,
            StepKind::AtuinSync
        ));
    }

    #[test]
    fn plan_no_atuin_sync_when_off_or_clean() {
        // Off by default.
        let p = plan(&EnvRequirements::default(), &PlanOpts::default());
        assert!(!p.steps.iter().any(|s| s.id == "atuin_sync"));
        // Even opted-in, `Clean` ships no personal layer.
        let clean = PlanOpts {
            atuin: true,
            strategy: crate::config::ShellStrategy::Clean,
            ..Default::default()
        };
        let pc = plan(&EnvRequirements::default(), &clean);
        assert!(!pc.steps.iter().any(|s| s.id == "atuin_sync"));
    }

    #[test]
    fn plan_clean_strategy_emits_no_personal_layer() {
        let req = EnvRequirements::default(); // Bare
        let opts = PlanOpts {
            strategy: crate::config::ShellStrategy::Clean,
            tools: vec!["fd".into()],
            dotfiles_repo: "git@github.com:me/dots".into(),
            setup: vec!["echo hi".into()],
            dotfiles: vec![".zshrc".into()],
            checkpoint: false,
            ..Default::default()
        };
        let ids: Vec<String> = plan(&req, &opts)
            .steps
            .iter()
            .map(|s| s.id.clone())
            .collect();
        // Clean drops every personal-layer step; only the workspace remains.
        assert_eq!(ids, ["workspace"]);
        assert!(
            !ids.iter()
                .any(|i| i == "tools" || i == "dotfiles" || i == "setup")
        );
    }

    #[test]
    fn plan_host_parity_substitutes_closure_before_dotfiles() {
        let opts = PlanOpts {
            strategy: crate::config::ShellStrategy::HostParity,
            dotfiles: vec![".zshrc".into()],
            home_store_roots: vec!["/nix/store/abc-starship".into()],
            checkpoint: true,
            ..Default::default()
        };
        let p = plan(&EnvRequirements::default(), &opts);
        let ids: Vec<&str> = p.steps.iter().map(|s| s.id.as_str()).collect();
        let closure = ids
            .iter()
            .position(|i| *i == "home_closure")
            .expect("closure step");
        let dots = ids
            .iter()
            .position(|i| *i == "dotfiles")
            .expect("dotfiles step");
        let chk = ids
            .iter()
            .position(|i| *i == "checkpoint")
            .expect("checkpoint");
        assert!(
            closure < dots && dots < chk,
            "closure before dotfiles before checkpoint: {ids:?}"
        );
        // host-parity needs nix even on a Bare-tier repo — installed before the closure.
        let nix = ids
            .iter()
            .position(|i| *i == "nix")
            .expect("nix installed for host-parity");
        assert!(nix < closure, "nix before closure: {ids:?}");
    }

    #[test]
    fn host_parity_closure_step_carries_cache_substituter_and_realises_roots() {
        let opts = PlanOpts {
            strategy: crate::config::ShellStrategy::HostParity,
            dotfiles: vec![".zshrc".into()],
            home_store_roots: vec!["/nix/store/abc-starship".into()],
            binary_cache: Some(BinaryCache {
                url: "https://cache.example.org".into(),
                key: "cache.example.org-1:KEY".into(),
                push: true,
            }),
            checkpoint: false,
            ..Default::default()
        };
        let p = plan(&EnvRequirements::default(), &opts);
        let step = p
            .steps
            .iter()
            .find(|s| s.id == "home_closure")
            .expect("closure step");
        let StepKind::Exec(script) = &step.kind else {
            panic!("home_closure must be an Exec step");
        };
        assert!(
            script.contains("nix-store --realise"),
            "realises the closure: {script}"
        );
        assert!(script.contains("/nix/store/abc-starship"), "names the root");
        assert!(
            script.contains("extra-substituters") && script.contains("cache.example.org"),
            "adds the configured cache as a substituter: {script}"
        );
    }

    #[test]
    fn host_parity_switch_script_runs_home_manager() {
        let s = home_switch_script("github:me/dots#me");
        assert!(s.contains("home-manager") && s.contains("switch"), "{s}");
        assert!(s.contains("github:me/dots#me"));
    }

    #[test]
    fn plan_local_parity_step_follows_clone_when_requested() {
        let opts = PlanOpts {
            origin: Some("https://example.com/r.git".into()),
            branch: Some("feat".into()),
            local_parity: Some("/home/u/wt".into()),
            checkpoint: false,
            ..Default::default()
        };
        let p = plan(&EnvRequirements::default(), &opts);
        let ids: Vec<&str> = p.steps.iter().map(|s| s.id.as_str()).collect();
        let clone = ids.iter().position(|i| *i == "clone").expect("clone");
        let parity = ids
            .iter()
            .position(|i| *i == "local_parity")
            .expect("local_parity step");
        assert_eq!(
            ids[clone + 1],
            "git_exclude",
            "the marker exclude follows the clone: {ids:?}"
        );
        assert_eq!(parity, clone + 2, "parity right after the exclude: {ids:?}");
        let StepKind::LocalParity { worktree, workdir } = &p
            .steps
            .iter()
            .find(|s| s.id == "local_parity")
            .unwrap()
            .kind
        else {
            panic!("local_parity must be a LocalParity step");
        };
        assert_eq!(worktree, "/home/u/wt");
        assert_eq!(workdir, "/workspace");
    }

    #[test]
    fn plan_bundle_seed_replaces_clone_after_git_auth() {
        let opts = PlanOpts {
            origin: Some("https://example.com/r.git".into()),
            branch: Some("feat".into()),
            local_parity: Some("/home/u/wt".into()),
            bundle_seed: Some(BundleSeed {
                worktree: "/home/u/wt".into(),
                branch: "feat".into(),
            }),
            checkpoint: false,
            ..Default::default()
        };
        let p = plan(&EnvRequirements::default(), &opts);
        let ids: Vec<&str> = p.steps.iter().map(|s| s.id.as_str()).collect();
        assert!(!ids.contains(&"clone"), "no in-sandbox clone: {ids:?}");
        let auth = ids.iter().position(|i| *i == "git_auth").expect("git_auth");
        let seed = ids
            .iter()
            .position(|i| *i == "bundle_seed")
            .expect("bundle_seed step");
        assert_eq!(seed, auth + 1, "seed right after git_auth: {ids:?}");
        let parity = ids.iter().position(|i| *i == "local_parity").unwrap();
        assert_eq!(
            ids[seed + 1],
            "git_exclude",
            "exclude follows the seed: {ids:?}"
        );
        assert_eq!(parity, seed + 2, "parity follows the exclude: {ids:?}");
        let step = &p.steps[seed];
        assert_eq!(step.label, "Seed repository (git bundle)");
        let StepKind::BundleSeed {
            worktree,
            workdir,
            branch,
        } = &step.kind
        else {
            panic!("bundle_seed must be a BundleSeed step");
        };
        assert_eq!(worktree, "/home/u/wt");
        assert_eq!(workdir, "/workspace");
        assert_eq!(branch, "feat");
    }

    #[test]
    fn plan_bundle_seed_without_origin_still_seeds() {
        let opts = PlanOpts {
            bundle_seed: Some(BundleSeed {
                worktree: "/w".into(),
                branch: "b".into(),
            }),
            snapshot_restore: Some("snap".into()),
            checkpoint: false,
            ..Default::default()
        };
        let p = plan(&EnvRequirements::default(), &opts);
        let ids: Vec<&str> = p.steps.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&"git_auth"), "{ids:?}");
        assert!(ids.contains(&"bundle_seed"), "{ids:?}");
        assert!(ids.contains(&"snapshot_restore"), "{ids:?}");
        assert!(!ids.contains(&"clone"), "{ids:?}");
    }

    #[test]
    fn plan_without_bundle_seed_keeps_clone() {
        let opts = PlanOpts {
            origin: Some("https://example.com/r.git".into()),
            checkpoint: false,
            ..Default::default()
        };
        let p = plan(&EnvRequirements::default(), &opts);
        let ids: Vec<&str> = p.steps.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&"clone"), "{ids:?}");
        assert!(!ids.contains(&"bundle_seed"), "{ids:?}");
    }

    #[test]
    fn plan_snapshot_restore_follows_local_parity() {
        let opts = PlanOpts {
            origin: Some("https://example.com/r.git".into()),
            branch: Some("feat".into()),
            local_parity: Some("/home/u/wt".into()),
            snapshot_restore: Some("00000000000000000009-abcd1234".into()),
            checkpoint: false,
            ..Default::default()
        };
        let p = plan(&EnvRequirements::default(), &opts);
        let ids: Vec<&str> = p.steps.iter().map(|s| s.id.as_str()).collect();
        let parity = ids.iter().position(|i| *i == "local_parity").unwrap();
        let restore = ids
            .iter()
            .position(|i| *i == "snapshot_restore")
            .expect("snapshot_restore step");
        assert_eq!(restore, parity + 1, "restore right after parity: {ids:?}");
        let StepKind::SnapshotRestore {
            worktree,
            workdir,
            snapshot_id,
        } = &p.steps[restore].kind
        else {
            panic!("snapshot_restore must be a SnapshotRestore step");
        };
        assert_eq!(worktree, "/home/u/wt");
        assert_eq!(workdir, "/workspace");
        assert_eq!(snapshot_id, "00000000000000000009-abcd1234");
        // Without a snapshot id the step never appears.
        let none = plan(
            &EnvRequirements::default(),
            &PlanOpts {
                origin: Some("https://example.com/r.git".into()),
                checkpoint: false,
                ..Default::default()
            },
        );
        assert!(!none.steps.iter().any(|s| s.id == "snapshot_restore"));
    }

    #[test]
    fn plan_no_local_parity_without_origin_or_request() {
        // No origin ⇒ no clone ⇒ no parity overlay even if requested.
        let no_origin = PlanOpts {
            origin: None,
            local_parity: Some("/home/u/wt".into()),
            ..Default::default()
        };
        assert!(
            !plan(&EnvRequirements::default(), &no_origin)
                .steps
                .iter()
                .any(|s| s.id == "local_parity"),
            "no clone, no parity"
        );
        // Origin but parity not requested (a spare) ⇒ pristine clone only.
        let spare = PlanOpts {
            origin: Some("https://example.com/r.git".into()),
            local_parity: None,
            ..Default::default()
        };
        assert!(
            !plan(&EnvRequirements::default(), &spare)
                .steps
                .iter()
                .any(|s| s.id == "local_parity"),
            "spare stays a pristine clone"
        );
    }

    #[test]
    fn plan_host_parity_falls_back_to_home_switch_without_roots() {
        let opts = PlanOpts {
            strategy: crate::config::ShellStrategy::HostParity,
            nix_home_flake: "github:me/dots#me".into(),
            checkpoint: false,
            ..Default::default()
        };
        let p = plan(&EnvRequirements::default(), &opts);
        let ids: Vec<&str> = p.steps.iter().map(|s| s.id.as_str()).collect();
        assert!(
            ids.contains(&"home_switch"),
            "home_switch fallback: {ids:?}"
        );
        assert!(!ids.contains(&"home_closure"));
    }

    #[test]
    fn plan_host_parity_p2p_pushes_then_installs_before_dotfiles() {
        let opts = PlanOpts {
            strategy: crate::config::ShellStrategy::HostParity,
            dotfiles: vec![".zshrc".into()],
            home_store_roots: vec!["/nix/store/abc-zsh".into()],
            home_closure_p2p: true,
            home_profile_installs: vec!["/nix/store/abc-zsh".into()],
            checkpoint: true,
            ..Default::default()
        };
        let p = plan(&EnvRequirements::default(), &opts);
        let ids: Vec<&str> = p.steps.iter().map(|s| s.id.as_str()).collect();
        let nix = ids.iter().position(|i| *i == "nix").expect("nix");
        let push = ids
            .iter()
            .position(|i| *i == "home_closure_push")
            .expect("p2p push step");
        let prof = ids
            .iter()
            .position(|i| *i == "home_profile")
            .expect("profile install step");
        let dots = ids.iter().position(|i| *i == "dotfiles").expect("dotfiles");
        assert!(
            nix < push && push < prof && prof < dots,
            "nix → push → install → dotfiles: {ids:?}"
        );
        // p2p does NOT emit the cache-substitute step.
        assert!(
            !ids.contains(&"home_closure"),
            "no substitute step under p2p: {ids:?}"
        );
        // The push step carries the roots for the host to copy.
        let StepKind::HomeClosurePush(roots) = &p
            .steps
            .iter()
            .find(|s| s.id == "home_closure_push")
            .unwrap()
            .kind
        else {
            panic!("push step must be HomeClosurePush");
        };
        assert_eq!(roots, &vec!["/nix/store/abc-zsh".to_string()]);
    }

    #[test]
    fn home_profile_install_script_installs_each_path_by_store_path() {
        let s = home_profile_install_script(&[
            "/nix/store/abc-zsh".into(),
            "/nix/store/def-starship".into(),
        ]);
        // sh_quote leaves these clean paths unquoted.
        assert!(s.contains("nix profile install /nix/store/abc-zsh"), "{s}");
        assert!(
            s.contains("nix profile install /nix/store/def-starship"),
            "{s}"
        );
        // Per-path `|| true` so one bad path can't abort the rest.
        assert!(s.contains("|| true"), "{s}");
    }

    #[test]
    fn store_roots_in_extracts_dedup_roots() {
        let rc = "source /nix/store/abc123-zsh-syntax/share/x.zsh\n\
                  eval \"$(/nix/store/abc123-zsh-syntax/bin/y)\"\n\
                  PATH=/run/current-system/sw/bin:$PATH\n\
                  echo /etc/profile";
        let roots = store_roots_in(rc);
        // The two references to the same store entry dedup to one root.
        assert!(roots.contains(&"/nix/store/abc123-zsh-syntax".to_string()));
        assert!(roots.iter().any(|r| r.starts_with("/run/current-system")));
        assert_eq!(
            roots
                .iter()
                .filter(|r| r.contains("abc123-zsh-syntax"))
                .count(),
            1
        );
        assert!(!roots.iter().any(|r| r.contains("/etc/profile")));
    }

    #[test]
    fn scan_dotfile_flags_absent_store_path() {
        let p = scan_dotfile(".zshrc", "source /nix/store/h-plugin/x.zsh", &[]);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].kind, PitfallKind::AbsentStorePath);
        assert_eq!(p[0].file, ".zshrc");
        assert!(p[0].detail.starts_with("/nix/store/"));
    }

    #[test]
    fn scan_dotfile_flags_undeclared_tool_and_suppresses_declared() {
        let rc = "eval \"$(atuin init zsh)\"\ncommand -v starship && eval \"$(starship init zsh)\"";
        let flagged = scan_dotfile(".zshrc", rc, &[]);
        let tools: Vec<&str> = flagged
            .iter()
            .filter(|p| p.kind == PitfallKind::MissingTool)
            .map(|p| p.detail.as_str())
            .collect();
        assert!(
            tools.contains(&"atuin") && tools.contains(&"starship"),
            "{tools:?}"
        );
        // Declaring atuin suppresses its flag.
        let after = scan_dotfile(".zshrc", rc, &["atuin".into()]);
        assert!(
            !after
                .iter()
                .any(|p| p.kind == PitfallKind::MissingTool && p.detail == "atuin")
        );
    }

    #[test]
    fn scan_dotfile_ignores_ubiquitous_tools() {
        // coreutils / base tools in command substitutions are not "missing".
        let rc = "X=$(dirname $0); echo $(cat /etc/x | tr a b); eval \"$(zoxide init zsh)\"";
        let tools: Vec<String> = scan_dotfile(".zshrc", rc, &[])
            .into_iter()
            .filter(|p| p.kind == PitfallKind::MissingTool)
            .map(|p| p.detail)
            .collect();
        assert_eq!(
            tools,
            vec!["zoxide".to_string()],
            "only the real tool flagged: {tools:?}"
        );
    }

    #[test]
    fn scan_dotfile_portable_rc_has_no_pitfalls() {
        let rc = "export PATH=\"$HOME/.local/bin:$PATH\"\nalias ll='ls -lah'\n# comment";
        assert!(scan_dotfile(".zshrc", rc, &[]).is_empty());
    }

    #[test]
    fn tools_installer_is_nix_first_with_native_fallback_and_name_map() {
        let s = tools_install_script(&["fd".into(), "ripgrep".into()]);
        assert!(
            s.contains("nix profile install \"nixpkgs#$t\""),
            "nix-first"
        );
        assert!(
            s.contains("apt-get install -y")
                && s.contains("apk add")
                && s.contains("dnf install -y"),
            "native fallbacks"
        );
        // fd maps to fd-find for the native path.
        assert!(
            s.contains("'fd-find'") || s.contains("fd-find"),
            "fd→fd-find name map"
        );
        assert!(s.trim_end().ends_with("true"), "best-effort: step succeeds");
    }

    #[test]
    fn agents_add_install_and_config_steps() {
        let opts = PlanOpts {
            agents: vec!["claude".into(), "hermes".into()],
            checkpoint: false,
            ..Default::default()
        };
        let p = plan(&EnvRequirements::default(), &opts);
        let ids: Vec<&str> = p.steps.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&"agents_install") && ids.contains(&"agents_config"));
        let cfg = p.steps.iter().find(|s| s.id == "agents_config").unwrap();
        assert!(matches!(&cfg.kind, StepKind::AgentConfigs(a) if a.len() == 2));
    }

    #[test]
    fn agent_config_paths_known_and_fallback() {
        let (files, dirs) = agent_config_paths("claude");
        assert!(files.contains(&".claude.json".to_string()));
        assert!(dirs.contains(&".claude".to_string()));
        // pi keeps its config/extensions under ~/.pi.
        let (fp, dp) = agent_config_paths("pi");
        assert!(fp.contains(&".pi.json".to_string()));
        assert!(dp.contains(&".pi".to_string()));
        // Unknown agent → conventional dotfile/dir fallback.
        let (f2, d2) = agent_config_paths("hermes");
        assert!(f2.contains(&".hermes.json".to_string()));
        assert!(d2.contains(&".hermes".to_string()) && d2.contains(&".config/hermes".to_string()));
    }

    #[test]
    fn agents_installer_has_known_installers() {
        let s = agents_install_script(&[
            "claude".into(),
            "codex".into(),
            "pi".into(),
            "hermes".into(),
        ]);
        assert!(s.contains("@anthropic-ai/claude-code"));
        assert!(s.contains("@openai/codex"));
        assert!(s.contains("@earendil-works/pi-coding-agent"));
        // hermes (Nous) installs via its own curl installer.
        assert!(s.contains("hermes-agent.nousresearch.com/install.sh"));
        assert!(s.contains("npm_g $pkgs"));
        assert!(s.contains("wait \"$hermes_pid\""));
        // a genuinely unknown agent doesn't fabricate an installer.
        let unknown = agents_install_script(&["frobnicator".into()]);
        assert!(unknown.contains("frobnicator: install via"));
    }

    #[test]
    fn agents_installer_logs_failures_and_lists_missing() {
        let s = agents_install_script(&["codex".into()]);
        assert!(!s.contains(": >"), "special built-in redirection exits sh");
        assert!(s.contains("thegn-agent-install.log"));
        assert!(s.contains("npm not found"));
        assert!(s.contains("agent CLI not installed"));
        assert!(s.contains("thegn-agent-install: npm"));
        assert!(s.ends_with("true"));
    }

    #[test]
    fn agents_installer_never_fails_the_step() {
        let s = agents_install_script(&["claude".into(), "hermes".into()]);
        assert_eq!(s.split("; ").last(), Some("true"));
    }

    #[test]
    fn agents_installer_background_join_is_shell_safe() {
        let s = agents_install_script(&["claude".into(), "hermes".into()]);
        assert!(!s.contains("& ;"));
        assert!(!s.contains("&;"));
    }

    #[test]
    fn agents_installer_does_not_background_without_hermes() {
        let s = agents_install_script(&["codex".into()]);
        assert!(!s.contains("} &"));
        assert!(!s.contains("hermes_pid=$!"));
    }

    #[test]
    fn agents_installer_hermes_background_captures_pid() {
        let s = agents_install_script(&["hermes".into()]);
        assert!(s.contains("& hermes_pid=$!"));
    }

    #[test]
    fn devshell_warm_sets_nix_access_token_for_private_inputs() {
        // The flake-devShell warm must export a github access-token (runtime, via
        // NIX_CONFIG) so a private flake INPUT fetches — git's credential helper
        // doesn't cover nix's fetcher.
        let req = EnvRequirements {
            direnv: true,
            direnv_uses_flake: true,
            ..Default::default()
        };
        let s = devshell_warm_script("/workspace", &req);
        assert!(s.contains("NIX_CONFIG"), "exports NIX_CONFIG");
        assert!(
            s.contains("access-tokens = github.com="),
            "sets a github access-token line"
        );
        assert!(
            s.contains("GH_TOKEN") && s.contains("GITHUB_TOKEN"),
            "derives from either token var"
        );
        // Purge a leftover /homeless-shelter so an interrupted prior build doesn't
        // wedge the next nix build (sandbox-less single-user nix in a microVM).
        assert!(
            s.contains("rm -rf /homeless-shelter"),
            "clears leftover /homeless-shelter"
        );
        // Plain (no-flake) warm also carries the prelude.
        let plain = devshell_warm_script("/workspace", &EnvRequirements::default());
        assert!(plain.contains("NIX_CONFIG") && plain.contains("/homeless-shelter"));
        // direnv install (also a nix build) carries the prelude too.
        assert!(direnv_install_script().contains("/homeless-shelter"));
    }

    #[test]
    fn direnv_install_tolerates_a_readonly_login_rc() {
        // Regression: a home-manager/nix read-only `~/.zshrc` (as on the sprite
        // login user) must NOT fail the "Install direnv" step with a red X — skip
        // the unwritable rc and end on `true` so only a broken direnv fails it.
        let s = direnv_install_script();
        assert!(
            s.contains("[ -w \"$rc\" ] || continue"),
            "skips an unwritable rc: {s}"
        );
        assert!(
            s.contains(">> \"$rc\" 2>/dev/null || true"),
            "swallows a failed hook append: {s}"
        );
        assert!(s.trim_end().ends_with("true"), "ends on true: {s}");
    }

    #[test]
    fn nix_installer_official_default_and_determinate_fallback() {
        use crate::config::NixInstaller;
        let off = nix_install_script(NixInstaller::Official, None, None, None, None);
        assert!(off.contains("nixos.org/nix/install"), "official installer");
        assert!(off.contains("--no-daemon"), "single-user");
        assert!(!off.contains("determinate.systems"));
        // Determinate carries the official installer as a fallback (|| (...)).
        let det = nix_install_script(NixInstaller::Determinate, None, None, None, None);
        assert!(
            det.contains("install.determinate.systems"),
            "uses Determinate"
        );
        assert!(det.contains("--init none"), "no-systemd flag");
        assert!(
            det.contains("nixos.org/nix/install"),
            "falls back to the official installer"
        );
    }

    #[test]
    fn nix_install_ensures_prereqs_and_retries() {
        use crate::config::NixInstaller;
        // Hardening against the sprite `exit 127` (official installer can't unpack
        // its .xz tarball on a minimal microVM): the script installs the missing
        // decompression/download tools and retries a flaky install.
        let s = nix_install_script(NixInstaller::Determinate, None, None, None, None);
        assert!(s.contains("ensure_tools"), "installs prerequisites");
        assert!(
            s.contains("xz"),
            "xz is a prerequisite (unpacks the tarball)"
        );
        // Covers the common package managers.
        for pm in ["apt-get", "apk", "dnf", "yum"] {
            assert!(s.contains(pm), "handles {pm}");
        }
        assert!(s.contains("retrying in 5s"), "bounded install retry");
        assert!(s.contains("do_install"), "retry re-checks nix presence");
    }

    #[test]
    fn nix_parallel_downloads_written_to_conf() {
        use crate::config::NixInstaller;
        let none = nix_install_script(NixInstaller::Official, None, None, None, None);
        assert!(!none.contains("http-connections"), "off by default");
        let s = nix_install_script(NixInstaller::Official, Some(100), None, None, None);
        assert!(s.contains("http-connections = 100"));
        assert!(s.contains("max-substitution-jobs = 100"));
        // still enables flakes.
        assert!(s.contains("experimental-features = nix-command flakes"));
    }

    #[test]
    fn binary_cache_substituter_and_push_step() {
        use crate::config::NixInstaller;
        let cache = BinaryCache {
            url: "https://cache.example.org".into(),
            key: "cache.example.org-1:abc".into(),
            push: true,
        };
        let s = nix_install_script(NixInstaller::Official, None, Some(&cache), None, None);
        assert!(s.contains("extra-substituters = https://cache.example.org"));
        assert!(s.contains("extra-trusted-public-keys = cache.example.org-1:abc"));
        // host_cache_url adds the loopback substituter while keeping signature
        // verification enabled, and coexists with a real cache.
        let hc = nix_install_script(
            NixInstaller::Official,
            None,
            Some(&cache),
            Some("http://127.0.0.1:8484"),
            None,
        );
        assert!(hc.contains("extra-substituters = http://127.0.0.1:8484"));
        assert!(!hc.contains("require-sigs = false"));
        assert!(hc.contains("require-sigs = true"));
        assert!(hc.contains("extra-substituters = https://cache.example.org"));
        // Host cache ⇒ graceful-degradation trio so a not-yet-up tunnel or a desynced
        // NAR degrades (fast fall-through / from-source rebuild) instead of hanging or
        // hard-failing the build.
        assert!(hc.contains("connect-timeout = 5"));
        assert!(hc.contains("stalled-download-timeout = 30"));
        assert!(hc.contains("fallback = true"));
        // No host cache ⇒ no require-sigs line (signatures stay enforced) and none of
        // the degradation knobs (the public-cache path is untouched).
        assert!(!s.contains("require-sigs"));
        assert!(!s.contains("fallback = true"));
        assert!(!s.contains("connect-timeout"));

        // plan() adds a cache_push step (after devshell) only when push is set.
        let req = EnvRequirements {
            nix_flake_devshell: true,
            ..Default::default()
        };
        let opts = PlanOpts {
            origin: None,
            checkpoint: false,
            binary_cache: Some(cache),
            ..Default::default()
        };
        let p = plan(&req, &opts);
        let ids: Vec<&str> = p.steps.iter().map(|s| s.id.as_str()).collect();
        let dev = ids.iter().position(|i| *i == "devshell").unwrap();
        let push = ids.iter().position(|i| *i == "cache_push").unwrap();
        assert!(push > dev, "cache_push runs after devshell");
        // No push step when push=false.
        let opts2 = PlanOpts {
            origin: None,
            checkpoint: false,
            binary_cache: Some(BinaryCache {
                url: "https://c".into(),
                key: String::new(),
                push: false,
            }),
            ..Default::default()
        };
        let p2 = plan(&req, &opts2);
        assert!(!p2.steps.iter().any(|s| s.id == "cache_push"));
    }

    #[test]
    fn host_cache_plan_never_disables_nix_signature_checks() {
        let req = EnvRequirements {
            nix_flake_devshell: true,
            ..Default::default()
        };
        let opts = PlanOpts {
            host_cache_url: Some("http://127.0.0.1:8484".into()),
            binary_cache: Some(BinaryCache {
                url: "https://cache.example.org".into(),
                key: String::new(),
                push: false,
            }),
            ..Default::default()
        };
        let p = plan(&req, &opts);

        let scripts: Vec<&str> = p
            .steps
            .iter()
            .filter_map(|step| match &step.kind {
                StepKind::Exec(script) => Some(script.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            !scripts.is_empty(),
            "the Nix plan includes executable steps"
        );
        assert!(scripts.iter().all(|script| {
            !script.contains("require-sigs = false") && !script.contains("--no-check-sigs")
        }));
    }

    #[test]
    fn host_cache_trusts_the_installation_key() {
        use crate::config::NixInstaller;
        let script = nix_install_script(
            NixInstaller::Official,
            None,
            None,
            Some("http://127.0.0.1:8484"),
            Some("thegn-0123456789abcdef-1:cHVibGlj"),
        );
        assert!(script.contains("extra-trusted-public-keys = thegn-0123456789abcdef-1:cHVibGlj"));
        assert!(script.contains("require-sigs = true"));

        let no_url = nix_install_script(
            NixInstaller::Official,
            None,
            None,
            None,
            Some("thegn-0123456789abcdef-1:cHVibGlj"),
        );
        assert!(!no_url.contains("extra-trusted-public-keys"));

        let escaped = nix_install_script(
            NixInstaller::Official,
            None,
            None,
            Some("http://127.0.0.1:8484"),
            Some("thegn-key%25'quoted"),
        );
        assert!(escaped.contains("thegn-key%%25'\\''quoted"));
    }

    #[test]
    fn cache_url_special_chars_are_escaped_for_printf() {
        use crate::config::NixInstaller;
        // A presigned S3 URL (with `%`) and a `'` in the key must NOT corrupt the
        // `printf '{conf}' > nix.conf` step: `%` → `%%` (else printf eats it as a
        // conversion directive) and `'` → `'\''` (else it closes the shell quote).
        let cache = BinaryCache {
            url: "s3://b?X-Amz-Signature=ab%2Fcd".into(),
            key: "it's-a-key".into(),
            push: false,
        };
        let s = nix_install_script(NixInstaller::Official, None, Some(&cache), None, None);
        // `%` doubled so printf emits a literal `%`.
        assert!(
            s.contains("X-Amz-Signature=ab%%2Fcd"),
            "url % must be doubled: {s}"
        );
        assert!(
            !s.contains("ab%2Fcd"),
            "un-doubled % leaks into the format string"
        );
        // `'` escaped so the surrounding single-quote isn't terminated.
        assert!(
            s.contains(r"it'\''s-a-key"),
            "key quote must be escaped: {s}"
        );
        // host_cache_url takes the same escaping.
        let hc = nix_install_script(
            NixInstaller::Official,
            None,
            None,
            Some("http://h/%41"),
            None,
        );
        assert!(hc.contains("http://h/%%41"));
    }

    #[test]
    fn git_auth_installs_git_first_when_the_image_lacks_it() {
        let s = git_auth_script(false);
        let ensure = s
            .find("command -v git")
            .expect("probes for git before configuring it");
        let first_config = s.find("git config").expect("configures git");
        assert!(
            ensure < first_config,
            "git is ensured before any git config"
        );
        assert!(s.contains("apt-get install -y --no-install-recommends git"));
        assert!(s.contains("apk add --no-cache git"));
        assert!(s.contains("dnf install -y git"));
        assert!(
            s.contains("no package manager could install it' >&2; exit 1"),
            "a still-missing git fails the step with a named cause"
        );
    }

    #[test]
    fn git_auth_withholds_token_helper_by_default() {
        let s = git_auth_script(false);
        assert!(
            !s.contains("GH_TOKEN") && !s.contains("GITHUB_TOKEN"),
            "{s}"
        );
        assert!(!s.contains("credential.helper"), "{s}");
        assert!(!s.contains("insteadOf"), "{s}");
        assert!(s.contains("safe.directory") && s.ends_with("true"), "{s}");
        // Plan level: the default PlanOpts keep credentials with thegn.
        let req = EnvRequirements::default();
        let step_script = |opts: &PlanOpts| {
            plan(&req, opts)
                .steps
                .into_iter()
                .find(|step| step.id == "git_auth")
                .and_then(|step| match step.kind {
                    StepKind::Exec(script) => Some(script),
                    _ => None,
                })
                .expect("git_auth step")
        };
        let base = PlanOpts {
            origin: Some("https://github.com/o/r".into()),
            ..PlanOpts::default()
        };
        assert!(!step_script(&base).contains("credential.helper"));
        let granted = PlanOpts {
            git_token: true,
            ..base
        };
        assert!(step_script(&granted).contains("credential.helper"));
    }

    #[test]
    fn git_auth_is_runtime_conditional_on_token() {
        let s = git_auth_script(true);
        assert!(
            s.contains("GH_TOKEN") && s.contains("GITHUB_TOKEN"),
            "reads both token vars"
        );
        assert!(
            s.contains("credential.helper"),
            "configures a credential helper"
        );
        assert!(
            s.contains("x-access-token"),
            "GitHub HTTPS token-as-password scheme"
        );
        assert!(s.contains("safe.directory"), "marks workdir safe");
        // The host-key policy comes from the chokepoint (SandboxBootstrap), so
        // assert against ITS output rather than a literal here (keeps host-key
        // literals out of every file but `hostkey.rs` — see the ratchet).
        let bootstrap_opts = crate::hostkey::host_key_opts_str(
            crate::hostkey::HostKeyClass::SandboxBootstrap,
            &crate::hostkey::HostKeyContext::default(),
        );
        assert!(
            s.contains("core.sshCommand") && s.contains(&bootstrap_opts),
            "trusts a fresh SSH host key so an ssh-origin clone doesn't fail host-key verification"
        );
        // Carries the host git identity (via THEGN_GIT_NAME/EMAIL in the exec env)
        // so sandbox commits aren't authored as the provider's default identity.
        assert!(
            s.contains("THEGN_GIT_NAME")
                && s.contains("user.name")
                && s.contains("THEGN_GIT_EMAIL")
                && s.contains("user.email"),
            "sets git user.name/email from the injected host identity: {s}"
        );
        // With a token, rewrite ssh GitHub origins to https so the token helper
        // authenticates the clone (the sandbox has no GitHub-authorized ssh key).
        // insteadOf is a multivar: BOTH the scp-like (`git@github.com:`) and
        // `ssh://` forms must be `--add`ed — a plain double `set` clobbers the
        // first value, dropping the scp-like form the common origin actually uses.
        assert!(
            s.contains("--add url.'https://github.com/'.insteadOf 'git@github.com:'"),
            "scp-like ssh origin rewrite is --added (not clobbered): {s}"
        );
        assert!(
            s.contains("--add url.'https://github.com/'.insteadOf 'ssh://git@github.com/'"),
            "ssh:// origin rewrite is --added too: {s}"
        );
        assert!(
            s.contains("--unset-all url.'https://github.com/'.insteadOf"),
            "clears prior insteadOf first so re-provision stays idempotent: {s}"
        );
    }

    #[test]
    fn dotfiles_repo_clones_and_bootstraps() {
        let s = dotfiles_repo_script("git@github.com:me/dots");
        assert!(s.contains("git clone"));
        assert!(s.contains("install.sh") && s.contains("bootstrap.sh") && s.contains("setup.sh"));
    }

    #[test]
    fn marker_path_under_workdir() {
        assert_eq!(
            EnvPlan::marker_path("/workspace/"),
            "/workspace/.thegn-provisioned"
        );
    }

    #[test]
    fn marker_path_uses_the_marker_const() {
        assert_eq!(EnvPlan::marker_path("/w"), format!("/w/{PROVISION_MARKER}"));
        assert_eq!(PROVISION_MARKER, ".thegn-provisioned");
    }

    #[test]
    fn split_deferred_preserves_foreground_and_deferred_order() {
        let plan = plan(
            &EnvRequirements::default(),
            &PlanOpts {
                agents: vec!["codex".into()],
                atuin: true,
                checkpoint: true,
                dotfiles: vec![".bashrc".into()],
                strategy: crate::config::ShellStrategy::Portable,
                ..Default::default()
            },
        );
        let (foreground, deferred) = plan.split_deferred(true);
        assert_eq!(
            deferred
                .iter()
                .map(|step| step.id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "agents_install",
                "agents_config",
                "atuin_sync",
                "checkpoint"
            ]
        );
        assert_eq!(
            foreground
                .iter()
                .map(|step| step.id.as_str())
                .collect::<Vec<_>>(),
            vec!["workspace", "dotfiles"]
        );
    }

    #[test]
    fn split_deferred_false_keeps_everything_in_the_foreground() {
        let plan = plan(
            &EnvRequirements::default(),
            &PlanOpts {
                agents: vec!["codex".into()],
                atuin: true,
                checkpoint: true,
                ..Default::default()
            },
        );
        let (foreground, deferred) = plan.split_deferred(false);
        assert_eq!(foreground, plan.steps);
        assert!(deferred.is_empty());
    }

    #[test]
    fn split_deferred_without_agents_only_defers_checkpoint() {
        let plan = plan(
            &EnvRequirements::default(),
            &PlanOpts {
                checkpoint: true,
                ..Default::default()
            },
        );
        let (_, deferred) = plan.split_deferred(true);
        assert_eq!(
            deferred
                .iter()
                .map(|step| step.id.as_str())
                .collect::<Vec<_>>(),
            vec!["checkpoint"]
        );
    }

    #[test]
    fn marker_state_accepts_only_deferred_as_pending() {
        for bytes in [b"ok\n".as_slice(), b"".as_slice(), b"garbage".as_slice()] {
            assert_eq!(marker_state(bytes), MarkerState::Done);
        }
        for bytes in [
            b"deferred".as_slice(),
            b"deferred\n".as_slice(),
            b" deferred\r\n".as_slice(),
        ] {
            assert_eq!(marker_state(bytes), MarkerState::Deferred);
        }
        assert_eq!(MARKER_DEFERRED, b"deferred\n");
        assert_eq!(marker_state(MARKER_DEFERRED), MarkerState::Deferred);
        assert_eq!(MARKER_DONE, b"ok\n");
    }

    fn exec_script<'a>(p: &'a EnvPlan, id: &str) -> &'a str {
        match &p.steps.iter().find(|s| s.id == id).expect(id).kind {
            StepKind::Exec(s) => s,
            other => panic!("{id} is not Exec: {other:?}"),
        }
    }

    #[test]
    fn git_exclude_follows_clone_and_bundle_seed_and_is_idempotent() {
        // THE-2043: the provision marker sits in the checkout root, so the step
        // right after the repo step lists it in `.git/info/exclude`.
        let cloned = plan(
            &EnvRequirements::default(),
            &PlanOpts {
                origin: Some("https://example.com/r.git".into()),
                ..Default::default()
            },
        );
        let seeded = plan(
            &EnvRequirements::default(),
            &PlanOpts {
                bundle_seed: Some(BundleSeed {
                    worktree: "/home/u/wt".into(),
                    branch: "feat".into(),
                }),
                ..Default::default()
            },
        );
        for (p, repo) in [(&cloned, "clone"), (&seeded, "bundle_seed")] {
            let ids: Vec<&str> = p.steps.iter().map(|s| s.id.as_str()).collect();
            let at = ids.iter().position(|i| *i == repo).expect(repo);
            assert_eq!(ids[at + 1], "git_exclude", "{ids:?}");
            let step = &p.steps[at + 1];
            assert_eq!(step.label, "Hide thegn marker from git");
            let s = exec_script(p, "git_exclude");
            assert!(s.starts_with("cd /workspace && [ -d .git ]"), "{s}");
            assert!(
                s.contains("grep -qxF '/.thegn-provisioned' .git/info/exclude"),
                "idempotent exact-line check: {s}"
            );
            assert!(
                s.contains("printf '%s\\n' '/.thegn-provisioned' >> .git/info/exclude"),
                "{s}"
            );
            assert!(s.ends_with("|| true"), "best-effort: {s}");
        }
        // No repo step, no exclude.
        let bare = plan(&EnvRequirements::default(), &PlanOpts::default());
        assert!(!bare.steps.iter().any(|s| s.id == "git_exclude"));
        // The image bake never carries per-checkout steps.
        let baked = bake_scripts(crate::config::NixInstaller::Official, None);
        assert!(baked.iter().all(|(id, _)| *id != "git_exclude"));
    }

    #[test]
    fn nix_verify_present_iff_nix_required() {
        // THE-2045: an env that needs Nix halts on "Verify Nix" instead of
        // reporting up without it; one that merely benefits gets no fatal check.
        let flake = EnvRequirements {
            nix_flake_devshell: true,
            ..Default::default()
        };
        let classic = EnvRequirements {
            nix_classic: true,
            ..Default::default()
        };
        let langs = EnvRequirements {
            languages: vec![Language::Python],
            ..Default::default()
        };
        let host_cache = PlanOpts {
            host_cache_url: Some("http://127.0.0.1:8484".into()),
            ..Default::default()
        };
        let push = PlanOpts {
            push_devshell: true,
            ..Default::default()
        };
        let parity = PlanOpts {
            strategy: crate::config::ShellStrategy::HostParity,
            home_store_roots: vec!["/nix/store/abc-zsh".into()],
            host_cache_url: Some("http://127.0.0.1:8484".into()),
            ..Default::default()
        };
        let devenv = EnvRequirements {
            devenv: true,
            ..Default::default()
        };
        let use_flake = EnvRequirements {
            nix_classic: true,
            direnv: true,
            direnv_uses_flake: true,
            ..Default::default()
        };
        let default = PlanOpts::default();
        let cases: [(&EnvRequirements, &PlanOpts); 9] = [
            (&flake, &default),
            (&devenv, &default),
            (&use_flake, &default),
            (&classic, &default),
            (&classic, &host_cache),
            (&classic, &push),
            (&langs, &default),
            (&langs, &host_cache),
            (&EnvRequirements::default(), &parity),
        ];
        for (req, opts) in cases {
            let p = plan(req, opts);
            let ids: Vec<&str> = p.steps.iter().map(|s| s.id.as_str()).collect();
            let has_nix = ids.contains(&"nix");
            assert!(has_nix, "every case installs nix: {ids:?}");
            let required = nix_required(req, opts);
            assert_eq!(
                ids.contains(&"nix_verify"),
                required,
                "nix_verify iff nix_required ({required}): {ids:?}"
            );
            if required {
                let at = ids.iter().position(|i| *i == "nix").unwrap();
                assert_eq!(ids[at + 1], "nix_verify", "right after nix: {ids:?}");
                assert_eq!(p.steps[at + 1].label, "Verify Nix");
                let s = exec_script(&p, "nix_verify");
                assert!(s.contains("command -v nix"), "{s}");
                assert!(s.contains(". \"$HOME/.nix-profile/etc/profile.d/nix.sh\""));
                assert!(
                    s.contains(
                        "nix is not installed: the Install Nix step failed (see its output above)"
                    ),
                    "{s}"
                );
                assert!(s.trim_end().ends_with("exit 1; }"), "fails closed: {s}");
            }
        }
        assert!(nix_required(&flake, &default));
        assert!(nix_required(&devenv, &default));
        assert!(nix_required(&classic, &host_cache));
        assert!(nix_required(&classic, &push));
        assert!(!nix_required(&classic, &default));
        assert!(!nix_required(&langs, &default));
        // A blank host-cache URL is not a host cache.
        let blank = PlanOpts {
            host_cache_url: Some("  ".into()),
            ..Default::default()
        };
        assert!(!nix_required(&langs, &blank));
        // Nix disallowed: no install, so no verify even when required.
        let no_nix = plan(
            &flake,
            &PlanOpts {
                allow_nix: false,
                ..Default::default()
            },
        );
        assert!(!no_nix.steps.iter().any(|s| s.id == "nix_verify"));
    }

    #[test]
    fn ensure_tools_reports_prerequisites_still_missing() {
        // THE-2045: after the package-manager attempt, re-check and name what
        // is still missing (exit 1), while the caller keeps `|| true`.
        let s = nix_install_script(
            crate::config::NixInstaller::Official,
            None,
            None,
            None,
            None,
        );
        assert!(
            s.contains(
                "echo \"nix: prerequisites still missing:$miss (no usable package manager or no root/sudo)\"; return 1;"
            ),
            "{s}"
        );
        assert!(s.contains("ensure_tools || true;"), "{s}");
        let post = s.find("prerequisites still missing").unwrap();
        let yum = s.find("yum install -y xz tar curl").unwrap();
        assert!(yum < post, "re-check runs after the install attempt");
    }

    #[test]
    fn nix_install_preps_nix_root_when_root_without_sudo() {
        // THE-2053: as root with no sudo (fly), thegn creates `/nix` itself so
        // the installer skips its `sudo mkdir`; USER is exported for the installer.
        use crate::config::NixInstaller;
        for installer in [NixInstaller::Official, NixInstaller::Determinate] {
            let s = nix_install_script(installer, None, None, None, None);
            assert!(s.contains("prep_nix_root() {"), "{s}");
            assert!(
                s.contains(
                    "prep_nix_root() { [ -e /nix ] && return 0; [ \"$(id -u)\" = 0 ] || return 0; mkdir -m 0755 /nix && chown 0:0 /nix; }"
                ),
                "acts only when uid 0 and /nix missing: {s}"
            );
            assert!(s.contains("mkdir -m 0755 /nix"), "{s}");
            assert!(
                s.contains("export USER=${USER:-$(id -un 2>/dev/null || echo root)};"),
                "{s}"
            );
            let ensure = s.find("ensure_tools || true;").unwrap();
            let prep = s.find("prep_nix_root || true;").unwrap();
            let loop_start = s.find("while :; do do_install").unwrap();
            assert!(ensure < prep, "prep runs after ensure_tools");
            assert!(prep < loop_start, "prep runs before the install loop");
            // USER is exported before anything runs the installer.
            assert!(s.find("export USER=").unwrap() < loop_start);
        }
        // The existing installer selection still holds.
        let off = nix_install_script(NixInstaller::Official, None, None, None, None);
        assert!(off.contains("nixos.org/nix/install"));
        assert!(!off.contains("determinate.systems"));
        let det = nix_install_script(NixInstaller::Determinate, None, None, None, None);
        assert!(det.contains("install.determinate.systems"));
        assert!(det.contains("nixos.org/nix/install"));
    }

    #[test]
    fn claude_auth_critical_files_are_credentials_and_settings_only() {
        // THE-2051: the MCP auth cache is not needed to authenticate.
        let files = agent_auth_critical_files("claude");
        assert!(files.iter().any(|f| f == ".claude/.credentials.json"));
        assert!(files.iter().any(|f| f == ".claude.json"));
        assert!(
            !files.iter().any(|f| f.contains("mcp-needs-auth-cache")),
            "{files:?}"
        );
        assert_eq!(files, agent_auth_critical_files("claude-code"));
    }

    #[test]
    fn codex_auth_critical_files_carry_the_login_and_the_legacy_paths() {
        // b162: codex keeps its login in `auth.json` (+ `config.toml`); the
        // old json paths stay for older installs.
        assert_eq!(
            agent_auth_critical_files("codex"),
            [
                ".codex/auth.json",
                ".codex/config.toml",
                ".codex/config.json",
                ".config/codex/config.json",
            ]
        );
    }

    #[test]
    fn hermes_auth_critical_files_carry_credentials_and_config() {
        assert_eq!(
            agent_auth_critical_files("hermes"),
            [".hermes/auth.json", ".hermes/config.yaml", ".hermes/.env"]
        );
    }

    #[test]
    fn nix_prelude_guards_the_purge_and_heals_empty_drvs() {
        // THE-2046: never purge /homeless-shelter or delete a `.drv` while a nix
        // process is alive; heal only zero-byte `.drv`s and their referrers.
        let p = nix_runtime_prelude();
        assert!(
            p.contains("nix_idle() { if command -v pgrep >/dev/null 2>&1; then ! pgrep -x \"nix|"),
            "{p}"
        );
        assert!(
            p.contains("for c in /proc/[0-9]*/comm; do"),
            "pgrep-less fallback: {p}"
        );
        assert!(
            !p.contains("pgrep -f"),
            "a -f guard matches the script itself: {p}"
        );
        assert!(p.contains("if nix_idle; then"), "{p}");
        let guard = p.find("if nix_idle; then").unwrap();
        let purge = p.find("rm -rf /homeless-shelter").unwrap();
        let heal_call = p.rfind("nix_store_heal;").unwrap();
        assert!(guard < purge && purge < heal_call, "{p}");
        assert!(
            p.contains("find /nix/store -maxdepth 1 -name \"*.drv\" -size 0"),
            "only empty .drv files: {p}"
        );
        assert!(
            p.contains("nix-store --query --referrers-closure \"$d\""),
            "{p}"
        );
        assert!(p.contains("nix-store --delete --ignore-liveness"), "{p}");
        assert!(p.contains("healed $n empty .drv file(s)"), "{p}");
        assert!(p.trim_end().ends_with(';'), "composes as a prefix: {p}");
        // Carried by every nix-building step.
        let warm = devshell_warm_script("/workspace", &EnvRequirements::default());
        assert!(warm.contains("nix_store_heal"), "{warm}");
    }

    #[test]
    fn devshell_warm_heals_and_retries_once_on_file_is_empty() {
        for req in [
            EnvRequirements::default(),
            EnvRequirements {
                direnv: true,
                direnv_uses_flake: true,
                ..Default::default()
            },
            EnvRequirements {
                nix_classic: true,
                ..Default::default()
            },
            EnvRequirements {
                devenv: true,
                ..Default::default()
            },
        ] {
            let s = devshell_warm_script("/workspace", &req);
            assert!(
                s.contains(
                    "out=$(thegn_warm 2>&1) || { printf '%s\\n' \"$out\" | grep -q 'file is empty' && nix_idle && nix_store_heal && thegn_warm >/dev/null 2>&1; }"
                ),
                "{s}"
            );
            assert_eq!(
                s.matches("thegn_warm >/dev/null").count(),
                1,
                "one retry: {s}"
            );
            assert!(
                s.trim_end().ends_with("true"),
                "warm stays best-effort: {s}"
            );
        }
    }

    #[test]
    fn direnv_install_writes_the_heal_lib_hook() {
        // THE-2046: direnv sources lib/*.sh before each .envrc, so an in-pane
        // `use flake` heals like a provisioning step.
        let s = direnv_install_script();
        assert!(
            s.contains("lib=\"${XDG_CONFIG_HOME:-$HOME/.config}/direnv/lib\"; mkdir -p \"$lib\""),
            "{s}"
        );
        assert!(s.contains("> \"$lib/thegn-nix-heal.sh\""), "{s}");
        assert!(s.contains(DIRENV_HEAL_LIB), "{s}");
        // Written inside a single-quoted printf argument: no quote may escape it.
        assert!(!DIRENV_HEAL_LIB.contains('\''), "{DIRENV_HEAL_LIB}");
        assert!(DIRENV_HEAL_LIB.contains("if nix_idle; then"));
        assert!(DIRENV_HEAL_LIB.contains("rm -rf /homeless-shelter"));
        assert!(DIRENV_HEAL_LIB.contains("nix_store_heal;"));
        assert!(s.trim_end().ends_with("true"), "{s}");
    }

    #[test]
    fn bake_scripts_are_the_repo_independent_prefix() {
        let steps = bake_scripts(crate::config::NixInstaller::Determinate, Some(32));
        let ids: Vec<&str> = steps.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec!["nix", "direnv"]);
        let nix = &steps[0].1;
        assert!(nix.contains("determinate"), "honours the installer choice");
        assert!(nix.contains("http-connections = 32"), "honours parallel");
        assert!(steps[1].1.contains("direnv hook"));
        // Nothing repo-specific may leak into a shared base image.
        for (_, s) in &steps {
            assert!(!s.contains("git clone"), "no clone in a base image");
        }
    }
}
