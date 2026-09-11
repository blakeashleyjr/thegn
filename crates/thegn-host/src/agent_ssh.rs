//! The sprite SSH-over-WSS transport (`[env.<name>.provider] connect = "ssh"`):
//! a real local `ssh` client whose transport is the `sprite-proxy` ProxyCommand,
//! attaching to a user-owned in-sandbox sshd on a loopback high port. Extracted
//! from `agent.rs` (kept flat); re-exported from
//! `crate::agent` so call sites are unchanged.

use std::path::{Path, PathBuf};

use thegn_core::config::{Config, ManagedKeyScope};
use thegn_core::db::Db;
use thegn_core::remote::GitLoc;
use thegn_core::repo;
use thegn_core::store::WorkspaceStore;

use crate::agent::shell_inner;

/// In-sandbox sshd listen port for the SSH-over-WSS transport. A high port — the
/// sprite user isn't root, so it can't bind 22.
pub const SPRITE_SSHD_PORT: u16 = 2222;

/// The thegn-managed ssh keypair for the sprite SSH-over-WSS transport, under
/// `$XDG_STATE/thegn/ssh/`. Generated (ed25519, no passphrase) on first use.
/// Returns `(private key path, public key line)`.
// off-loop: ssh-keygen runs once, on the provisioning path (spawn_blocking /
// pool thread / CLI); loop-side callers (sprite_ssh_connect) find the key
// already cached and skip the subprocess.
pub fn sprite_ssh_keypair() -> anyhow::Result<(PathBuf, String)> {
    managed_ssh_keypair(ManagedKeyScope::Shared, "sprites", "shared")
}

/// The managed keypair for one configured custody scope. Per-account key names
/// are deterministic from the provider plus its non-secret credential-ref name;
/// shared mode preserves the historic `sprite_ed25519` path.
pub fn managed_ssh_keypair(
    scope: ManagedKeyScope,
    provider: &str,
    account: &str,
) -> anyhow::Result<(PathBuf, String)> {
    let dir = thegn_core::util::thegn_dir().join("ssh");
    std::fs::create_dir_all(&dir)?;
    thegn_core::fsperm::restrict_dir_to_owner(&dir)?;
    let basename = scope.managed_key_basename(provider, account);
    let key = dir.join(basename);
    generate_managed_keypair_at(&key, &format!("thegn-{provider}-{account}"))?;
    thegn_core::fsperm::restrict_to_owner(&key)?;
    let pubp = PathBuf::from(format!("{}.pub", key.display()));
    let pubkey = std::fs::read_to_string(&pubp)?.trim().to_string();
    Ok((key, pubkey))
}

/// Load an already-authorized managed keypair without ever generating a new
/// key at the recorded path. Custody records describe remote authorization, so
/// a missing local half must fail closed rather than silently creating a key
/// the remote has never seen.
pub fn existing_managed_ssh_keypair(key: &Path) -> anyhow::Result<(PathBuf, String)> {
    let public = PathBuf::from(format!("{}.pub", key.display()));
    if !key.is_file() || !public.is_file() {
        anyhow::bail!(
            "recorded managed SSH keypair is unavailable at {}",
            key.display()
        );
    }
    thegn_core::fsperm::restrict_to_owner(key)?;
    let public_key = std::fs::read_to_string(&public)?.trim().to_string();
    if public_key.is_empty() {
        anyhow::bail!(
            "recorded managed SSH public key is empty at {}",
            public.display()
        );
    }
    Ok((key.to_path_buf(), public_key))
}

/// Generate a no-passphrase ed25519 pair at an exact path. Existing complete
/// pairs are reused; a half-pair is rejected instead of silently overwriting
/// custody material. Used by initial provisioning and rotation staging.
#[expect(clippy::disallowed_methods)]
pub fn generate_managed_keypair_at(key: &Path, comment: &str) -> anyhow::Result<()> {
    let public = PathBuf::from(format!("{}.pub", key.display()));
    match (key.exists(), public.exists()) {
        (true, true) => return Ok(()),
        (true, false) | (false, true) => anyhow::bail!(
            "managed SSH keypair is incomplete at {} (refusing to overwrite)",
            key.display()
        ),
        (false, false) => {}
    }
    if let Some(parent) = key.parent() {
        std::fs::create_dir_all(parent)?;
        thegn_core::fsperm::restrict_dir_to_owner(parent)?;
    }
    let out = std::process::Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-C", comment, "-q", "-f"])
        .arg(key)
        .output()
        .map_err(|error| anyhow::anyhow!("ssh-keygen: {error}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "ssh-keygen failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

/// Idempotent in-sandbox setup for the SSH-over-WSS transport (run during
/// provisioning when `connect = "ssh"`): install openssh, generate a user-owned
/// host key, authorize `pubkey`, and write a minimal sshd_config listening on
/// `127.0.0.1:SPRITE_SSHD_PORT`. Pure (shell string).
pub fn sprite_sshd_setup_script(pubkey: &str) -> String {
    let pk = thegn_core::util::sh_quote(pubkey);
    format!(
        "command -v sshd >/dev/null 2>&1 || nix profile install nixpkgs#openssh 2>/dev/null || \
           (export DEBIAN_FRONTEND=noninteractive; sudo apt-get update -y && sudo apt-get install -y openssh-server) 2>/dev/null || true; \
         command -v sshd >/dev/null 2>&1 || exit 73; \
         mkdir -p \"$HOME/.ssh\" || exit 73; chmod 700 \"$HOME/.ssh\" || exit 73; \
         touch \"$HOME/.ssh/authorized_keys\" || exit 73; chmod 600 \"$HOME/.ssh/authorized_keys\" || exit 73; \
         grep -qF {pk} \"$HOME/.ssh/authorized_keys\" 2>/dev/null || printf '%s\\n' {pk} >> \"$HOME/.ssh/authorized_keys\" || exit 73; \
         [ -f \"$HOME/.ssh/sprite_host_ed25519\" ] || ssh-keygen -t ed25519 -N '' -q -f \"$HOME/.ssh/sprite_host_ed25519\" || exit 73; \
         printf 'Port {port}\\nListenAddress 127.0.0.1\\nHostKey %s/.ssh/sprite_host_ed25519\\nAuthorizedKeysFile %s/.ssh/authorized_keys\\nPasswordAuthentication no\\nPidFile %s/.ssh/sprite_sshd.pid\\nPrintMotd no\\n' \"$HOME\" \"$HOME\" \"$HOME\" > \"$HOME/.ssh/sprite_sshd_config\" || exit 73",
        port = SPRITE_SSHD_PORT,
    )
}

/// Idempotent in-sandbox setup for the mosh transport (`[env.<name>.provider]
/// transport = "mosh"`): ensure `mosh-server` exists so the interactive pane rides
/// mosh instead of silently falling back to plain ssh (the bridge probes for it
/// and downgrades when absent). NixOS/Determinate images install it into the nix
/// profile — on the login-shell PATH the mosh `--ssh` bootstrap runs the server
/// through; apt is the non-Nix fallback. Best-effort — a mosh-less image just
/// keeps the ssh pane. Pure (shell string).
pub fn mosh_setup_script() -> String {
    String::from(
        "command -v mosh-server >/dev/null 2>&1 || \
           nix profile install nixpkgs#mosh 2>/dev/null || \
           (export DEBIAN_FRONTEND=noninteractive; \
            sudo apt-get update -y && sudo apt-get install -y mosh) 2>/dev/null || true; \
         true",
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
    use thegn_core::config::ProviderConnect;
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
    let thegn_core::placement::Placement::Provider(_) = &environment.placement else {
        return None;
    };
    let pc = &cfg.env.get(&environment.name)?.provider;
    tracing::debug!(
        target: "thegn::sandbox",
        env = %environment.name,
        connect = ?pc.connect,
        "sprite_ssh_connect: resolved provider env"
    );
    if pc.connect != ProviderConnect::Ssh {
        return None;
    }
    let sandbox_id =
        crate::provider_factory::provider_sandbox_name(cfg, worktree, &environment.name)
            .filter(|id| !id.is_empty());
    let recorded = match sandbox_id
        .as_deref()
        .map(|id| crate::provider_factory::managed_custody_record(pc, id))
        .transpose()
    {
        Ok(record) => record.flatten(),
        Err(error) => {
            thegn_core::msg::warn(&format!("connect=ssh: {error}"));
            return None;
        }
    };
    let account = crate::provider_factory::managed_key_account(pc);
    let keypair = if let Some(record) = recorded {
        existing_managed_ssh_keypair(&record.key_path)
    } else if sandbox_id
        .as_deref()
        .is_some_and(crate::provider_workdir::is_provisioned_locally)
    {
        // A pre-custody-ledger Sprite was provisioned with the historical
        // shared key. Preserve that identity until an explicit reprovision;
        // there is no record tying that old authorization to this worktree.
        sprite_ssh_keypair()
    } else {
        managed_ssh_keypair(
            cfg.credentials.ssh.managed_key_scope,
            &pc.provider,
            &account,
        )
    };
    let (key, _pubkey) = match keypair {
        Ok(k) => k,
        Err(e) => {
            thegn_core::msg::warn(&format!(
                "connect=ssh: managed key generation failed ({e}); falling back to the WSS exec pane"
            ));
            return None;
        }
    };
    // The sprite user owns the in-sandbox sshd + authorized_keys (non-root sshd
    // can only authenticate as itself), so ssh logs in as that user. The workdir
    // resolves against the sandbox's cached `$HOME` (workspace lives under the
    // login user's home), falling back to the bare default when uncached.
    let workdir = sandbox_id
        .map(|id| crate::provider_workdir::resolve(pc, &id))
        .unwrap_or_else(|| pc.sync_workdir());
    Some((key, "sprite".to_string(), workdir))
}

/// Build the local `ssh` argv for the SSH-over-WSS pane: a real ssh client whose
/// transport is the `sprite-proxy` ProxyCommand. `thegn_exe` is this binary (for
/// the ProxyCommand); `key`/`user`/`workdir` come from [`sprite_ssh_connect`].
pub fn sprite_ssh_argv(
    thegn_exe: &str,
    worktree: &str,
    key: &Path,
    user: &str,
    workdir: &str,
) -> Vec<String> {
    sprite_ssh_argv_inner(thegn_exe, worktree, key, user, workdir, None)
}

/// Rotation proof variant: the proxy subprocess must resolve back to the exact
/// custody tuple whose replacement key is being tested. A moved worktree or a
/// same-named Sprite in another account therefore fails closed before opening
/// the WSS transport.
pub(crate) struct SpriteSshCustody<'a> {
    pub provider: &'a str,
    pub account: &'a str,
    pub instance: &'a str,
}

pub(crate) fn sprite_ssh_argv_for_custody(
    thegn_exe: &str,
    worktree: &str,
    key: &Path,
    user: &str,
    workdir: &str,
    custody: SpriteSshCustody<'_>,
) -> Vec<String> {
    sprite_ssh_argv_inner(
        thegn_exe,
        worktree,
        key,
        user,
        workdir,
        Some((custody.provider, custody.account, custody.instance)),
    )
}

fn sprite_ssh_argv_inner(
    thegn_exe: &str,
    worktree: &str,
    key: &Path,
    user: &str,
    workdir: &str,
    custody: Option<(&str, &str, &str)>,
) -> Vec<String> {
    let mut proxy = format!(
        "{} sprite-proxy {}",
        thegn_core::util::sh_quote(thegn_exe),
        thegn_core::util::sh_quote(worktree),
    );
    if let Some((provider, account, instance)) = custody {
        proxy.push_str(&format!(
            " --expected-provider {} --expected-account {} --expected-instance {}",
            thegn_core::util::sh_quote(provider),
            thegn_core::util::sh_quote(account),
            thegn_core::util::sh_quote(instance),
        ));
    }
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
            thegn_core::util::sh_quote(workdir)
        )
    };
    let mut v: Vec<String> = vec![
        "ssh".into(),
        "-tt".into(),
        // Hermetic managed identity. In particular, disabling multiplexing is
        // load-bearing for rotation: a staged-key proof must not reuse an
        // already-authenticated connection made with the retiring key.
        "-F".into(),
        "/dev/null".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "IdentitiesOnly=yes".into(),
        "-o".into(),
        "ControlMaster=no".into(),
        "-o".into(),
        "ControlPath=none".into(),
        "-o".into(),
        "ConnectTimeout=10".into(),
        "-o".into(),
        format!("ProxyCommand={proxy}"),
    ];
    // Loopback over the authenticated sprite WSS transport: the host-key policy
    // (inner check disabled — the outer WSS auth established identity, and a
    // TOFU pin against the churning proxy would false-mismatch) comes from the
    // one chokepoint. See `thegn_core::hostkey` and the host-key ratchet.
    v.extend(thegn_core::hostkey::host_key_args(
        thegn_core::hostkey::HostKeyClass::LoopbackTunneled,
        &thegn_core::hostkey::HostKeyContext::default(),
    ));
    v.extend([
        "-o".into(),
        "LogLevel=ERROR".into(),
        "-i".into(),
        key.to_string_lossy().into_owned(),
        "-p".into(),
        SPRITE_SSHD_PORT.to_string(),
        format!("{user}@sprite"),
        "--".into(),
        remote,
    ]);
    v
}
