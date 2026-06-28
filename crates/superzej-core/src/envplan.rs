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
    }

    // 3. Tier-specific toolchain.
    match tier {
        Tier::Nix => {
            if opts.allow_nix {
                steps.push(ProvisionStep {
                    id: "nix".into(),
                    label: "Install Nix".into(),
                    kind: StepKind::Exec(nix_install_script()),
                });
                if req.direnv {
                    steps.push(ProvisionStep {
                        id: "direnv".into(),
                        label: "Install direnv".into(),
                        kind: StepKind::Exec(direnv_install_script()),
                    });
                }
                steps.push(ProvisionStep {
                    id: "devshell".into(),
                    label: "Build dev shell".into(),
                    kind: StepKind::Exec(devshell_warm_script(&opts.workdir, req)),
                });
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

    // 4. Personal layer (`[sandbox.home]`) — generic, repo-independent. Runs
    // after the project toolchain so the tool installer can use Nix when present.
    if !opts.tools.is_empty() {
        steps.push(ProvisionStep {
            id: "tools".into(),
            label: "Install personal tools".into(),
            kind: StepKind::Exec(tools_install_script(&opts.tools)),
        });
    }
    if !opts.dotfiles_repo.trim().is_empty() {
        steps.push(ProvisionStep {
            id: "dotfiles_repo".into(),
            label: "Set up dotfiles".into(),
            kind: StepKind::Exec(dotfiles_repo_script(opts.dotfiles_repo.trim())),
        });
    }
    if !opts.setup.is_empty() {
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

    // 5. Dotfiles (opportunistic).
    if !opts.dotfiles.is_empty() {
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
fn nix_install_script() -> String {
    // `--no-daemon` = single-user (no systemd); works in a container without an
    // init. Source the profile so the same shell sees `nix`. Enable flakes.
    // POSIX sh (runs under /bin/sh = dash in the sprite): NO process substitution
    // — download to a file then run. Single-user (`--no-daemon`), flakes enabled.
    // POSIX sh (runs under /bin/sh = dash in the sprite): NO process substitution
    // — download to a file then run. Single-user (`--no-daemon`); the installer
    // uses sudo to create /nix when not root. Enable flakes. FAIL-FAST: the final
    // `command -v nix` sets the exit code, so a failed install surfaces (not
    // masked by the trailing writes).
    String::from(
        "if command -v nix >/dev/null 2>&1; then exit 0; fi; \
         export HOME=${HOME:-/root}; \
         n=0; while [ $n -lt 45 ]; do \
           { sudo -n true 2>/dev/null || [ \"$(id -u)\" = 0 ]; } && \
             getent hosts nixos.org >/dev/null 2>&1 && break; \
           n=$((n+1)); sleep 2; \
         done; \
         curl -fsSL https://nixos.org/nix/install -o /tmp/nix-install.sh && \
           sh /tmp/nix-install.sh --no-daemon --yes; \
         export PATH=\"$HOME/.nix-profile/bin:$PATH\"; \
         mkdir -p \"$HOME/.config/nix\"; \
         printf 'experimental-features = nix-command flakes\\n' > \"$HOME/.config/nix/nix.conf\"; \
         command -v nix >/dev/null 2>&1",
    )
}

/// Install direnv + hook it into the login shells, so entering the workdir
/// activates the flake devShell exactly like local. Idempotent.
fn direnv_install_script() -> String {
    String::from(
        "command -v direnv >/dev/null 2>&1 || (nix profile install nixpkgs#direnv 2>/dev/null \
           || (export DEBIAN_FRONTEND=noninteractive; apt-get update -y && apt-get install -y direnv)); \
         for rc in \"$HOME/.bashrc\" \"$HOME/.zshrc\"; do \
           [ -f \"$rc\" ] || touch \"$rc\"; \
           grep -q 'direnv hook' \"$rc\" 2>/dev/null || \
             printf '\\neval \"$(direnv hook %s)\"\\n' \"$(basename \"$rc\" | sed s/^\\.//;s/rc$//)\" >> \"$rc\"; \
         done",
    )
}

/// Warm the dev environment so the first interactive shell is instant. With
/// direnv+flake we `direnv allow` + evaluate once; otherwise build the flake
/// devShell / devenv directly.
fn devshell_warm_script(workdir: &str, req: &EnvRequirements) -> String {
    let wd = sh_quote(workdir);
    let nixsh = ". \"$HOME/.nix-profile/etc/profile.d/nix.sh\" 2>/dev/null || true; \
                 export PATH=\"$HOME/.nix-profile/bin:$PATH\"";
    if req.direnv && req.direnv_uses_flake {
        format!(
            "{nixsh}; cd {wd} && direnv allow . 2>/dev/null; direnv exec . true 2>/dev/null || nix develop --command true 2>/dev/null || true"
        )
    } else if req.devenv {
        format!(
            "{nixsh}; cd {wd} && (command -v devenv >/dev/null 2>&1 || nix profile install nixpkgs#devenv 2>/dev/null); devenv shell true 2>/dev/null || true"
        )
    } else {
        format!("{nixsh}; cd {wd} && nix develop --command true 2>/dev/null || true")
    }
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
