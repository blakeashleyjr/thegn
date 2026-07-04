//! The sprite SSH-over-WSS transport (`[env.<name>.provider] connect = "ssh"`):
//! a real local `ssh` client whose transport is the `sprite-proxy` ProxyCommand,
//! attaching to a user-owned in-sandbox sshd on a loopback high port. Extracted
//! from `agent.rs` (pinned by the file-size ratchet); re-exported from
//! `crate::agent` so call sites are unchanged.

use std::path::{Path, PathBuf};

use superzej_core::config::Config;
use superzej_core::db::Db;
use superzej_core::remote::GitLoc;
use superzej_core::repo;
use superzej_core::store::WorkspaceStore;

use crate::agent::shell_inner;

/// In-sandbox sshd listen port for the SSH-over-WSS transport. A high port — the
/// sprite user isn't root, so it can't bind 22.
pub const SPRITE_SSHD_PORT: u16 = 2222;

/// The superzej-managed ssh keypair for the sprite SSH-over-WSS transport, under
/// `$XDG_STATE/superzej/ssh/`. Generated (ed25519, no passphrase) on first use.
/// Returns `(private key path, public key line)`.
// off-loop: ssh-keygen runs once, on the provisioning path (spawn_blocking /
// pool thread / CLI); loop-side callers (sprite_ssh_connect) find the key
// already cached and skip the subprocess.
#[expect(clippy::disallowed_methods)]
pub fn sprite_ssh_keypair() -> anyhow::Result<(PathBuf, String)> {
    let dir = superzej_core::util::superzej_dir().join("ssh");
    std::fs::create_dir_all(&dir)?;
    let key = dir.join("sprite_ed25519");
    let pubp = dir.join("sprite_ed25519.pub");
    if !pubp.exists() {
        let out = std::process::Command::new("ssh-keygen")
            .args([
                "-t",
                "ed25519",
                "-N",
                "",
                "-C",
                "superzej-sprite",
                "-q",
                "-f",
            ])
            .arg(&key)
            .output()
            .map_err(|e| anyhow::anyhow!("ssh-keygen: {e}"))?;
        if !out.status.success() {
            anyhow::bail!(
                "ssh-keygen failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
    let pubkey = std::fs::read_to_string(&pubp)?.trim().to_string();
    Ok((key, pubkey))
}

/// Idempotent in-sandbox setup for the SSH-over-WSS transport (run during
/// provisioning when `connect = "ssh"`): install openssh, generate a user-owned
/// host key, authorize `pubkey`, and write a minimal sshd_config listening on
/// `127.0.0.1:SPRITE_SSHD_PORT`. Pure (shell string).
pub fn sprite_sshd_setup_script(pubkey: &str) -> String {
    let pk = superzej_core::util::sh_quote(pubkey);
    format!(
        "command -v sshd >/dev/null 2>&1 || nix profile install nixpkgs#openssh 2>/dev/null || \
           (export DEBIAN_FRONTEND=noninteractive; sudo apt-get update -y && sudo apt-get install -y openssh-server) 2>/dev/null || true; \
         mkdir -p \"$HOME/.ssh\"; chmod 700 \"$HOME/.ssh\"; \
         touch \"$HOME/.ssh/authorized_keys\"; chmod 600 \"$HOME/.ssh/authorized_keys\"; \
         grep -qF {pk} \"$HOME/.ssh/authorized_keys\" 2>/dev/null || printf '%s\\n' {pk} >> \"$HOME/.ssh/authorized_keys\"; \
         [ -f \"$HOME/.ssh/sprite_host_ed25519\" ] || ssh-keygen -t ed25519 -N '' -q -f \"$HOME/.ssh/sprite_host_ed25519\"; \
         printf 'Port {port}\\nListenAddress 127.0.0.1\\nHostKey %s/.ssh/sprite_host_ed25519\\nAuthorizedKeysFile %s/.ssh/authorized_keys\\nPasswordAuthentication no\\nPidFile %s/.ssh/sprite_sshd.pid\\nPrintMotd no\\n' \"$HOME\" \"$HOME\" \"$HOME\" > \"$HOME/.ssh/sprite_sshd_config\"; \
         true",
        port = SPRITE_SSHD_PORT,
    )
}

/// Idempotent: ensure the in-sandbox sshd is listening (start it if not). Run at
/// connect time by the `sprite-proxy` ProxyCommand. Pure (shell string).
pub fn sprite_sshd_start_script() -> String {
    "SSHD=$(command -v sshd || echo \"$HOME/.nix-profile/bin/sshd\"); \
     pgrep -f sprite_sshd_config >/dev/null 2>&1 || \
       (\"$SSHD\" -f \"$HOME/.ssh/sprite_sshd_config\" 2>/dev/null || true); true"
        .to_string()
}

/// When `worktree`'s resolved provider env has `connect = "ssh"`, the inputs to
/// spawn the interactive pane as a local `ssh` client tunneled over the provider
/// proxy: `(private key path, ssh user, in-sandbox workdir)`. `None` otherwise.
pub fn sprite_ssh_connect(cfg: &Config, worktree: &str) -> Option<(PathBuf, String, String)> {
    use superzej_core::config::ProviderConnect;
    let loc = GitLoc::for_worktree(Path::new(worktree));
    let repo_root: PathBuf = Db::open()
        .ok()
        .and_then(|db| db.repo_root_for(worktree).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| repo::main_worktree(Path::new(worktree)))
        .unwrap_or_else(|| PathBuf::from(worktree));
    let selected_env = Db::open()
        .ok()
        .and_then(|db| db.effective_env(worktree, &repo_root.to_string_lossy()));
    let environment = cfg.resolve_env(
        &repo_root,
        &loc,
        Path::new(worktree),
        selected_env.as_deref(),
    );
    let superzej_core::placement::Placement::Provider(_) = &environment.placement else {
        return None;
    };
    let pc = &cfg.env.get(&environment.name)?.provider;
    tracing::debug!(
        target: "szhost::sandbox",
        env = %environment.name,
        connect = ?pc.connect,
        "sprite_ssh_connect: resolved provider env"
    );
    if pc.connect != ProviderConnect::Ssh {
        return None;
    }
    let (key, _pubkey) = match sprite_ssh_keypair() {
        Ok(k) => k,
        Err(e) => {
            superzej_core::msg::warn(&format!(
                "connect=ssh: managed key generation failed ({e}); falling back to the WSS exec pane"
            ));
            return None;
        }
    };
    // The sprite user owns the in-sandbox sshd + authorized_keys (non-root sshd
    // can only authenticate as itself), so ssh logs in as that user.
    Some((key, "sprite".to_string(), pc.sync_workdir()))
}

/// Build the local `ssh` argv for the SSH-over-WSS pane: a real ssh client whose
/// transport is the `sprite-proxy` ProxyCommand. `szhost_exe` is this binary (for
/// the ProxyCommand); `key`/`user`/`workdir` come from [`sprite_ssh_connect`].
pub fn sprite_ssh_argv(
    szhost_exe: &str,
    worktree: &str,
    key: &Path,
    user: &str,
    workdir: &str,
) -> Vec<String> {
    let proxy = format!(
        "{} sprite-proxy {}",
        superzej_core::util::sh_quote(szhost_exe),
        superzej_core::util::sh_quote(worktree),
    );
    // Run the user's login shell, not the sprite's default `$SHELL` (which is
    // bash → no zsh / no host-parity prompt). The same runtime probe chain the
    // native pane uses (`command -v zsh && exec zsh -l; …`) so the uploaded
    // `.zshrc` (and starship/etc.) loads exactly like local.
    let shell = shell_inner(true);
    let remote = if workdir.is_empty() {
        shell
    } else {
        format!(
            "cd {} 2>/dev/null; {shell}",
            superzej_core::util::sh_quote(workdir)
        )
    };
    vec![
        "ssh".into(),
        "-tt".into(),
        "-o".into(),
        format!("ProxyCommand={proxy}"),
        "-o".into(),
        "StrictHostKeyChecking=no".into(),
        "-o".into(),
        "UserKnownHostsFile=/dev/null".into(),
        "-o".into(),
        "LogLevel=ERROR".into(),
        "-i".into(),
        key.to_string_lossy().into_owned(),
        "-p".into(),
        SPRITE_SSHD_PORT.to_string(),
        format!("{user}@sprite"),
        "--".into(),
        remote,
    ]
}
