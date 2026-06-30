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

use std::path::Path;

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
    /// Language ecosystems detected from manifests (for the non-Nix tiers).
    pub languages: Vec<Language>,
}

/// A language ecosystem detected from a manifest file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Node,
    Python,
    Rust,
    Go,
    Ruby,
}

impl EnvRequirements {
    /// The highest-fidelity tier this repo supports.
    pub fn tier(&self) -> Tier {
        if self.nix_flake_devshell || self.devenv {
            Tier::Nix
        } else if self.tool_versions {
            Tier::ToolVersions
        } else if !self.languages.is_empty() {
            Tier::Languages
        } else {
            Tier::Bare
        }
    }
}

/// Provisioning fidelity tiers, best → fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Nix flake/devenv present — install Nix and reproduce the exact devShell.
    Nix,
    /// `.tool-versions`/mise — install mise and `mise install`.
    ToolVersions,
    /// Only language manifests — best-effort native runtimes.
    Languages,
    /// Nothing declared — just the repo + a shell.
    Bare,
}

/// Detect a worktree's environment requirements from the files it ships. Pure
/// over the filesystem: reads a handful of well-known paths, never executes.
pub fn detect(worktree: &Path) -> EnvRequirements {
    let read = |name: &str| std::fs::read_to_string(worktree.join(name)).ok();
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

    let tool_versions =
        exists(".tool-versions") || exists("mise.toml") || exists(".mise.toml") || exists(".nvmrc");

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

    EnvRequirements {
        nix_flake_devshell,
        devenv: exists("devenv.nix"),
        direnv,
        direnv_uses_flake,
        tool_versions,
        languages,
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
    /// Opt-in: seed the host's already-built devShell closure into the sandbox
    /// store so the in-sandbox devShell is a local store hit, not a from-source
    /// rebuild. SCOPED — the host uploads only the paths public caches lack (the
    /// repo's from-source builds + rust-overlay output) and the sandbox fills the
    /// rest from cache.nixos.org. Emits a `devshell_push` step after `nix`,
    /// independent of `skip_devshell_warm`. Only meaningful with a nix devShell.
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
    /// Sandbox-side URL of the host's embedded nix binary cache (the loopback the
    /// reverse tunnel binds, e.g. `http://127.0.0.1:8484`), or `None`. When set,
    /// it's baked into the sandbox nix.conf as an `extra-substituters` entry with
    /// `require-sigs = false`, so `nix develop`/`direnv` substitute prebuilt store
    /// paths from the host instead of building. See `[env.<name>.provider] host_cache`.
    pub host_cache_url: Option<String>,
    /// Provision superzej's MANAGED pi inside the sandbox (`<home>/.superzej/pi`):
    /// carry the host's managed agent dir (the seeded `superzej-acp` package +
    /// settings) and npm-install the pinned binary there, so the "Agent" picker
    /// entry's `$HOME/.superzej/pi` snippet resolves in-sprite exactly as on the
    /// host. `false` ⇒ skip (no managed-pi agent configured). Emits a
    /// `managed_pi` step. Best-effort.
    pub managed_pi: bool,
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
            host_cache_url: None,
            managed_pi: false,
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
    /// Host-executed: transfer the repo's devShell closure — already built on the
    /// HOST — into the sandbox store (host `nix copy --to file://` → fs upload →
    /// sandbox `nix copy --from file://`), so the in-sandbox devShell is a local
    /// store hit instead of a rebuild. Opt-in (`[env.<name>.provider] push_devshell
    /// = true`). Best-effort; runs after Nix is installed/claimed, before the
    /// devShell warm. The host repo root is resolved in the applier.
    DevShellClosurePush,
    /// Host-executed: bring the sandbox clone to full parity with the LOCAL
    /// worktree — replay unpushed commits (a thin `git bundle`), uncommitted
    /// tracked changes (`git diff HEAD`), and untracked non-ignored files. The
    /// applier reads the host worktree (`worktree`), uploads the captured
    /// artifacts, and execs the replay in `workdir`. Best-effort: a failure
    /// leaves the pristine origin checkout in place.
    LocalParity { worktree: String, workdir: String },
    /// Host-executed: provision superzej's managed pi inside the sandbox — carry
    /// the host's `~/.superzej/pi/agent` (the seeded superzej-acp package + config)
    /// into `<home>/.superzej/pi/agent`, then npm-install the pinned pi binary
    /// there. Best-effort. The pin + host dir are resolved in the applier.
    ManagedPi,
}

/// The compiled, backend-agnostic provisioning plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvPlan {
    pub steps: Vec<ProvisionStep>,
    /// The detected tier this plan targets (for logging / the loading screen).
    pub tier: Tier,
}

impl EnvPlan {
    /// Sentinel file the applier drops after a full successful run so a later
    /// open skips re-provisioning. Lives under the workdir's parent so a repo
    /// re-clone doesn't wipe it.
    pub fn marker_path(workdir: &str) -> String {
        format!("{}/.superzej-provisioned", workdir.trim_end_matches('/'))
    }
}

/// Compile [`EnvRequirements`] + [`PlanOpts`] into an ordered [`EnvPlan`]. Pure:
/// produces shell strings, runs nothing.
pub fn plan(req: &EnvRequirements, opts: &PlanOpts) -> EnvPlan {
    let wd = sh_quote(&opts.workdir);
    let tier = req.tier();
    let mut steps = Vec::new();

    // 1. Workspace dir.
    steps.push(ProvisionStep {
        id: "workspace".into(),
        label: "Prepare workspace".into(),
        kind: StepKind::Exec(format!("mkdir -p {wd}")),
    });

    // 2. Git auth + clone. The auth step is a runtime-conditional credential
    // helper: if a GitHub token is present in the exec env (carried from the host
    // via `passthrough_env_remote` — GH_TOKEN/GITHUB_TOKEN), configure git to use
    // it for HTTPS clone/push; a no-op without a token. Persisted in ~/.gitconfig
    // so it survives the checkpoint and works for later pushes from the shell.
    if opts.origin.is_some() {
        steps.push(ProvisionStep {
            id: "git_auth".into(),
            label: "Configure git credentials".into(),
            kind: StepKind::Exec(git_auth_script()),
        });
    }
    // Clone the repo into the workdir (idempotent — skips if already a repo).
    if let Some(origin) = opts.origin.as_deref() {
        let clone = crate::remote::provision_repo_script(origin, opts.branch.as_deref());
        steps.push(ProvisionStep {
            id: "clone".into(),
            label: "Clone repository".into(),
            kind: StepKind::Exec(format!("cd {wd} && {clone}")),
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
                    )),
                });
                if req.direnv {
                    steps.push(ProvisionStep {
                        id: "direnv".into(),
                        label: "Install direnv".into(),
                        kind: StepKind::Exec(direnv_install_script()),
                    });
                }
                // Seed the host's already-built closure into the sandbox store
                // (scoped: the push uploads only the paths public caches lack — the
                // repo's from-source builds + rust-overlay output — and the sandbox
                // fills the rest from cache.nixos.org). This is the FAST path: a
                // local store hit instead of a from-source compile. Independent of
                // `skip_devshell_warm` — seeding isn't the slow build, it replaces it.
                if opts.push_devshell {
                    steps.push(ProvisionStep {
                        id: "devshell_push".into(),
                        label: "Seed dev shell from host".into(),
                        kind: StepKind::DevShellClosurePush,
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
                } else if req.direnv {
                    // Skipping the build, but still TRUST the repo's `.envrc` (cheap,
                    // instant) so the lazy in-pane build fires the moment you `cd` in
                    // — no manual `direnv allow` needed.
                    steps.push(ProvisionStep {
                        id: "direnv_allow".into(),
                        label: "Allow direnv".into(),
                        kind: StepKind::Exec(direnv_allow_script(&opts.workdir)),
                    });
                }
            }
        }
        Tier::ToolVersions => {
            steps.push(ProvisionStep {
                id: "mise".into(),
                label: "Install toolchains (mise)".into(),
                kind: StepKind::Exec(mise_install_script(&opts.workdir)),
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
    let nix_installed = matches!(tier, Tier::Nix) && opts.allow_nix;

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

    // Managed pi: carry the host's ~/.superzej/pi/agent + install the pinned binary
    // in the sandbox, so the "Agent" picker entry's `$HOME/.superzej/pi` snippet
    // resolves in-sprite. After agent configs (it's the same family of work).
    if opts.managed_pi {
        steps.push(ProvisionStep {
            id: "managed_pi".into(),
            label: "Install managed pi".into(),
            kind: StepKind::ManagedPi,
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
                )),
            });
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

/// Single-user Nix install (no systemd/daemon — sprites are minimal microVMs),
/// idempotent: a no-op if `nix` is already on `PATH`. Enables flakes.
fn nix_install_script(
    installer: crate::config::NixInstaller,
    parallel: Option<u32>,
    cache: Option<&BinaryCache>,
    host_cache_url: Option<&str>,
) -> String {
    use crate::config::NixInstaller;
    // Persisted nix.conf lines (static, safe to bake — NOT secrets; the token auth
    // stays runtime-only in nix_runtime_prelude). These apply to the devShell build
    // (the longest step) AND interactive in-pane `nix develop`/`direnv`.
    let mut conf = String::from("experimental-features = nix-command flakes\\n");
    if let Some(n) = parallel {
        // Parallelize the download-bound devShell substitution — the biggest win.
        conf.push_str(&format!(
            "http-connections = {n}\\nmax-substitution-jobs = {n}\\n"
        ));
    }
    if let Some(c) = cache.filter(|c| !c.url.trim().is_empty()) {
        conf.push_str(&format!("extra-substituters = {}\\n", c.url.trim()));
        if !c.key.trim().is_empty() {
            conf.push_str(&format!("extra-trusted-public-keys = {}\\n", c.key.trim()));
        }
    }
    // The host's embedded nix cache (served over the reverse tunnel). It's UNSIGNED
    // — paths are served straight from the host store with no signing key — so the
    // sandbox must accept unsigned substitutes: `require-sigs = false`. Safe here
    // (the cache is reachable only over the per-sandbox loopback tunnel, the store
    // is single-user, and NarHash still content-checks each download). `nix`
    // accumulates `extra-substituters` lines, so this coexists with `cache` above.
    if let Some(url) = host_cache_url.map(str::trim).filter(|u| !u.is_empty()) {
        conf.push_str(&format!("extra-substituters = {url}\\n"));
        conf.push_str("require-sigs = false\\n");
    }
    // Readiness wait (sudo available + DNS) shared by both installers; the sprite
    // user has passwordless sudo and the installer creates /nix via sudo.
    let wait = "n=0; while [ $n -lt 45 ]; do \
           { sudo -n true 2>/dev/null || [ \"$(id -u)\" = 0 ]; } && \
             getent hosts nixos.org >/dev/null 2>&1 && break; \
           n=$((n+1)); sleep 2; \
         done";
    // Official: single-user (`--no-daemon`), POSIX sh (download then run — no
    // process substitution under dash). The proven default + fallback.
    let official = "curl -fsSL https://nixos.org/nix/install -o /tmp/nix-install.sh && \
           sh /tmp/nix-install.sh --no-daemon --yes";
    let install = match installer {
        NixInstaller::Official => official.to_string(),
        // Determinate (faster Rust installer). `--init none` = no systemd (a bare
        // microVM); falls back to the official single-user installer on ANY failure
        // so a determinate-incompatibility never wedges provisioning.
        NixInstaller::Determinate => format!(
            "(curl -fsSL https://install.determinate.systems/nix -o /tmp/nix-ds.sh && \
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
    format!(
        "claim_store() {{ if [ \"$(id -u)\" != 0 ] && [ -d /nix/var/nix/db ] && [ \"$(stat -c %u /nix/var/nix/db 2>/dev/null)\" != \"$(id -u)\" ] && [ ! -S /nix/var/nix/daemon-socket/socket ] && command -v sudo >/dev/null 2>&1 && sudo -n true 2>/dev/null; then sudo chown -R \"$(id -u):$(id -g)\" /nix 2>/dev/null || true; fi; }}; \
         export HOME=${{HOME:-/root}}; \
         claim_store; \
         if command -v nix >/dev/null 2>&1 && {{ [ \"$(stat -c %u /nix/var/nix/db 2>/dev/null)\" = \"$(id -u)\" ] || [ -S /nix/var/nix/daemon-socket/socket ] || [ \"$(id -u)\" = 0 ]; }}; then exit 0; fi; \
         {wait}; \
         {install}; \
         [ -r \"$HOME/.nix-profile/etc/profile.d/nix.sh\" ] && . \"$HOME/.nix-profile/etc/profile.d/nix.sh\" 2>/dev/null || true; \
         [ -r /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh ] && . /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh 2>/dev/null || true; \
         export PATH=\"$HOME/.nix-profile/bin:/nix/var/nix/profiles/default/bin:$PATH\"; \
         claim_store; \
         mkdir -p \"$HOME/.config/nix\"; \
         printf '{conf}' > \"$HOME/.config/nix/nix.conf\"; \
         command -v nix >/dev/null 2>&1"
    )
}

/// An sh prelude for any step that runs `nix` builds. Two jobs:
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
///
/// Trailing `;` so it composes as a prefix; a no-op for the token half when no
/// token is present.
fn nix_runtime_prelude() -> &'static str {
    "tok=\"${GH_TOKEN:-${GITHUB_TOKEN:-}}\"; \
     if [ -n \"$tok\" ]; then export NIX_CONFIG=\"access-tokens = github.com=$tok\"; fi; \
     sudo -n rm -rf /homeless-shelter 2>/dev/null || true; \
     rm -rf /homeless-shelter 2>/dev/null || true; "
}

/// Install direnv + hook it into the login shells, so entering the workdir
/// activates the flake devShell exactly like local. Idempotent.
fn direnv_install_script() -> String {
    // apt-FIRST: a fresh sprite's freshly-claimed Nix store makes `nix profile
    // install` slow (~10s+); the native package is ~2s and direnv is just a PATH
    // shim (no Nix integration needed here). Nix is the fallback for base images
    // without apt.
    format!(
        "{}command -v direnv >/dev/null 2>&1 \
           || (export DEBIAN_FRONTEND=noninteractive; apt-get update -y >/dev/null 2>&1 && apt-get install -y direnv >/dev/null 2>&1) \
           || nix profile install nixpkgs#direnv 2>/dev/null; \
         for sh in bash zsh; do \
           rc=\"$HOME/.${{sh}}rc\"; \
           [ -f \"$rc\" ] || touch \"$rc\"; \
           grep -q 'direnv hook' \"$rc\" 2>/dev/null || \
             printf '\\neval \"$(direnv hook %s)\"\\n' \"$sh\" >> \"$rc\"; \
         done",
        nix_runtime_prelude(),
    )
}

/// Warm the dev environment so the first interactive shell is instant. With
/// direnv+flake we `direnv allow` + evaluate once; otherwise build the flake
/// devShell / devenv directly.
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
    let nixsh = nixsh.as_str();
    if req.direnv && req.direnv_uses_flake {
        format!(
            "{nixsh}; cd {wd} 2>/dev/null && direnv allow . 2>/dev/null; direnv exec . true 2>/dev/null || nix develop --command true 2>/dev/null || true; true"
        )
    } else if req.devenv {
        format!(
            "{nixsh}; cd {wd} 2>/dev/null && (command -v devenv >/dev/null 2>&1 || nix profile install nixpkgs#devenv 2>/dev/null) || true; devenv shell true 2>/dev/null || true; true"
        )
    } else {
        format!(
            "{nixsh}; cd {wd} 2>/dev/null && nix develop --command true 2>/dev/null || true; true"
        )
    }
}

/// Trust the repo's `.envrc` without building anything (the companion to
/// `skip_devshell_warm`). `direnv allow` is instant — it only marks the `.envrc`
/// approved — so the loading screen doesn't block, yet the first in-pane `cd` into
/// the worktree replays direnv and builds the devShell lazily, with no manual
/// `direnv allow`. Best-effort: a missing direnv / no `.envrc` is a no-op.
fn direnv_allow_script(workdir: &str) -> String {
    let wd = sh_quote(workdir);
    format!(
        "{tok}export PATH=\"$HOME/.nix-profile/bin:/nix/var/nix/profiles/default/bin:$HOME/.local/state/nix/profile/bin:$HOME/.local/bin:$PATH\"; \
         command -v direnv >/dev/null 2>&1 && cd {wd} 2>/dev/null && [ -f .envrc ] && direnv allow . 2>/dev/null; true",
        tok = nix_runtime_prelude(),
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

/// Install mise + the repo's pinned toolchains.
fn mise_install_script(workdir: &str) -> String {
    let wd = sh_quote(workdir);
    format!(
        "command -v mise >/dev/null 2>&1 || curl -fsSL https://mise.run | sh; \
         export PATH=\"$HOME/.local/bin:$PATH\"; \
         cd {wd} && mise trust 2>/dev/null; mise install 2>/dev/null || true"
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
           nat_install \"$(nat \"$t\")\" 2>/dev/null || echo \"superzej: skipped tool $t (no installer)\" >&2; \
         done; true"
    )
}

/// Coding agents superzej knows how to reproduce in a sandbox. Used to
/// AUTO-DETECT which agents the host has (so a sandbox reaches "exact local
/// parity" without per-sandbox config) — each is probed on the host PATH /
/// config locations. Known installers exist for a subset (see
/// [`agents_install_script`]); the rest still get their config uploaded
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

/// Install known coding-agent CLIs (best-effort, idempotent). Known agents get a
/// real installer; unknown ones are left to the user's `setup`/`tools` (their
/// config is still uploaded). Per-agent failures are non-fatal.
fn agents_install_script(agents: &[String]) -> String {
    let mut lines = vec![
        "have() { command -v \"$1\" >/dev/null 2>&1; }".to_string(),
        "npm_g() { have npm && npm install -g \"$1\" 2>/dev/null; }".to_string(),
    ];
    for a in agents {
        let line = match a.trim() {
            "claude" | "claude-code" => {
                "have claude || npm_g @anthropic-ai/claude-code || true".to_string()
            }
            "codex" => "have codex || npm_g @openai/codex || true".to_string(),
            // superzej's own agent; pi >=0.80 bundles its extension deps.
            "pi" => "have pi || npm_g @earendil-works/pi-coding-agent || true".to_string(),
            other => format!("# {other}: install via [sandbox.home] setup/tools"),
        };
        lines.push(line);
    }
    lines.push("true".into());
    lines.join("; ")
}

/// Configure git to authenticate HTTPS GitHub clone/push using a token from the
/// environment, IF one is present (GH_TOKEN, else GITHUB_TOKEN). Runtime-
/// conditional + idempotent; the helper reads the token at git-run time (so it's
/// never baked into config or the checkpoint). No-op without a token (e.g. ssh
/// remotes / public repos). Also marks the workdir a safe directory.
fn git_auth_script() -> String {
    String::from(
        "tok=\"${GH_TOKEN:-${GITHUB_TOKEN:-}}\"; \
         git config --global --add safe.directory '*' 2>/dev/null || true; \
         if [ -n \"$tok\" ]; then \
           git config --global credential.helper \
             '!f() { test \"$1\" = get && printf \"username=x-access-token\\npassword=%s\\n\" \"${GH_TOKEN:-$GITHUB_TOKEN}\"; }; f'; \
         fi; true",
    )
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
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!("sz-envplan-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
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
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn detect_devenv_is_nix_tier() {
        let d = tmp("devenv");
        write(&d, "devenv.nix", "{ }\n");
        let r = detect(&d);
        assert!(r.devenv);
        assert_eq!(r.tier(), Tier::Nix);
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
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn bare_repo_is_bare_tier() {
        let d = tmp("bare");
        write(&d, "README.md", "hi");
        assert_eq!(detect(&d).tier(), Tier::Bare);
        let _ = std::fs::remove_dir_all(&d);
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
                "nix",
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
                "nix",
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
    fn plan_devshell_push_after_nix_before_devshell_when_opted_in() {
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
        let nix = ids.iter().position(|i| *i == "nix").expect("nix");
        let push = ids
            .iter()
            .position(|i| *i == "devshell_push")
            .expect("devshell_push present when opted in");
        let dev = ids.iter().position(|i| *i == "devshell").expect("devshell");
        assert!(
            nix < push && push < dev,
            "devshell_push after nix, before devshell: {ids:?}"
        );
        assert!(matches!(
            p.steps
                .iter()
                .find(|s| s.id == "devshell_push")
                .unwrap()
                .kind,
            StepKind::DevShellClosurePush
        ));
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
    fn plan_skip_devshell_warm_omits_build_but_keeps_scoped_push() {
        let req = EnvRequirements {
            nix_flake_devshell: true,
            direnv: true,
            direnv_uses_flake: true,
            ..Default::default()
        };
        // Skip drops the from-source BUILD (+ its cache push), but the scoped push
        // (the fast seed) and the cheap `direnv allow` stay.
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
        // The scoped push (fast seed) is independent of the warm decision.
        let push = p
            .steps
            .iter()
            .position(|s| s.id == "devshell_push")
            .expect("scoped devshell_push present even when skipping the build");
        let nix = ids.iter().position(|i| *i == "nix").unwrap();
        assert!(nix < push, "push after nix: {ids:?}");
        // ...and the cheap `direnv allow` stays, so the in-pane lazy build fires.
        let allow = p
            .steps
            .iter()
            .find(|s| s.id == "direnv_allow")
            .expect("direnv_allow present when skipping the build with direnv");
        assert!(matches!(
            &allow.kind,
            StepKind::Exec(s) if s.contains("direnv allow")
        ));
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
        assert_eq!(parity, clone + 1, "parity immediately after clone: {ids:?}");
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
    fn plan_managed_pi_step_emitted_only_when_requested() {
        let on = PlanOpts {
            managed_pi: true,
            ..Default::default()
        };
        assert!(
            plan(&EnvRequirements::default(), &on)
                .steps
                .iter()
                .any(|s| s.id == "managed_pi" && s.kind == StepKind::ManagedPi),
            "managed_pi step present when requested"
        );
        assert!(
            !plan(&EnvRequirements::default(), &PlanOpts::default())
                .steps
                .iter()
                .any(|s| s.id == "managed_pi"),
            "absent by default"
        );
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
        // unknown agent doesn't fabricate an installer.
        assert!(s.contains("hermes: install via"));
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
    fn nix_installer_official_default_and_determinate_fallback() {
        use crate::config::NixInstaller;
        let off = nix_install_script(NixInstaller::Official, None, None, None);
        assert!(off.contains("nixos.org/nix/install"), "official installer");
        assert!(off.contains("--no-daemon"), "single-user");
        assert!(!off.contains("determinate.systems"));
        // Determinate carries the official installer as a fallback (|| (...)).
        let det = nix_install_script(NixInstaller::Determinate, None, None, None);
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
    fn nix_parallel_downloads_written_to_conf() {
        use crate::config::NixInstaller;
        let none = nix_install_script(NixInstaller::Official, None, None, None);
        assert!(!none.contains("http-connections"), "off by default");
        let s = nix_install_script(NixInstaller::Official, Some(100), None, None);
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
        let s = nix_install_script(NixInstaller::Official, None, Some(&cache), None);
        assert!(s.contains("extra-substituters = https://cache.example.org"));
        assert!(s.contains("extra-trusted-public-keys = cache.example.org-1:abc"));
        // host_cache_url adds the loopback substituter + require-sigs=false (unsigned
        // host store served over the reverse tunnel), and coexists with a real cache.
        let hc = nix_install_script(
            NixInstaller::Official,
            None,
            Some(&cache),
            Some("http://127.0.0.1:8484"),
        );
        assert!(hc.contains("extra-substituters = http://127.0.0.1:8484"));
        assert!(hc.contains("require-sigs = false"));
        assert!(hc.contains("extra-substituters = https://cache.example.org"));
        // No host cache ⇒ no require-sigs line (signatures stay enforced).
        assert!(!s.contains("require-sigs"));

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
    fn git_auth_is_runtime_conditional_on_token() {
        let s = git_auth_script();
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
            "/workspace/.superzej-provisioned"
        );
    }
}
