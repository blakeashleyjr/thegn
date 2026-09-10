//! Lifecycle for the route-to-host merge credential owned by a provisioned
//! provider sandbox. Plaintext exists only long enough to cross the provider's
//! secret exec environment and is never persisted on the host.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::io::Write as _;
use std::process::{Command, Stdio};
use thegn_core::config::{Config, EnvProviderConfig, MergeRemoteMode, ServeExposure};
use thegn_core::control::{RouteToHostTokenBinding, Scope, ScopeSet, TokenKind};
use thegn_core::db::Db;
use thegn_core::remote::SshTarget;
use thegn_core::store::ControlStore;
use thegn_svc::provider::Provider;

const LEGACY_CREDENTIAL_PATH: &str = ".config/thegn/route-to-host.env";
const CREDENTIAL_DIR: &str = ".config/thegn/route-to-host";
const TOKEN_LIFETIME_MS: i64 = 30 * 24 * 60 * 60 * 1000;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn provider_owner_label(provider: &EnvProviderConfig, sandbox_id: &str) -> String {
    // Provider ids are not globally unique. Include the non-secret account
    // reference identity so equal sandbox ids in two providers/accounts cannot
    // rotate or revoke each other's return credential.
    let identity = (
        provider.provider.trim(),
        crate::provider_factory::managed_key_account(provider),
        sandbox_id,
    );
    serde_json::to_string(&identity).expect("route credential owner tuple serializes")
}

fn ssh_owner_label(target: &SshTarget, worktree: &str) -> String {
    // The host string includes the SSH account (`user@host`). Hash the rest of
    // the connection identity before putting it in the pairing label: identity
    // paths and advanced args are host-local routing metadata and do not belong
    // in audit/config surfaces, even though the credential itself is never in
    // this tuple. The worktree is included here as well as in the authorization
    // binding so two worktrees sharing one SSH account cannot rotate each other.
    let connection = serde_json::to_vec(target).expect("ssh route owner serializes");
    let digest = format!("{:x}", Sha256::digest(connection));
    serde_json::to_string(&(
        "ssh",
        target.host.trim(),
        target.port,
        &digest[..16],
        worktree,
    ))
    .expect("ssh route owner tuple serializes")
}

pub(crate) fn credential_path(worktree: &str) -> String {
    // An unmanaged SSH account can host many thegn worktrees in one $HOME.
    // One global file would make the last launch steal every other worktree's
    // credential, so use a stable opaque per-worktree name.
    let digest = format!("{:x}", Sha256::digest(worktree.as_bytes()));
    format!("{CREDENTIAL_DIR}/{digest}.env")
}

fn credential_lock_keys(owner: Option<&str>, worktree: Option<&str>) -> Vec<String> {
    let mut keys = Vec::with_capacity(2);
    if let Some(owner) = owner {
        keys.push(format!("owner:{owner}"));
    }
    if let Some(worktree) = worktree {
        keys.push(format!("worktree:{worktree}"));
    }
    keys.sort_unstable();
    keys.dedup();
    keys
}

fn credential_locks(owner: Option<&str>, worktree: Option<&str>) -> Result<Vec<std::fs::File>> {
    let dir = thegn_core::util::thegn_dir().join("run/route-credential-locks");
    std::fs::create_dir_all(&dir).context("create SSH route-credential lock directory")?;
    credential_lock_keys(owner, worktree)
        .into_iter()
        .map(|stable_key| {
            let digest = format!("{:x}", Sha256::digest(stable_key.as_bytes()));
            let file = std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(dir.join(format!("{digest}.lock")))
                .context("open route-credential lifecycle lock")?;
            match file.try_lock() {
                Ok(()) => Ok(file),
                Err(std::fs::TryLockError::WouldBlock) => anyhow::bail!(
                    "another process is updating this route credential owner/worktree; retry"
                ),
                Err(std::fs::TryLockError::Error(error)) => {
                    Err(error).context("lock route-credential lifecycle")
                }
            }
        })
        .collect()
}

fn binding_matches_owner(row_label: &str, owner: &str) -> bool {
    matches!(
        RouteToHostTokenBinding::parse_label(row_label),
        Some(Ok(binding)) if binding.owner == owner
    )
}

fn revoke_owner_label(
    store: &dyn ControlStore,
    owner: &str,
    legacy_label: Option<&str>,
    except_pairing_id: Option<&str>,
    at_ms: i64,
) -> Result<usize> {
    let ids: Vec<String> = store
        .pairings()?
        .into_iter()
        .filter(|row| {
            row.kind == "token"
                && row.revoked_at.is_none()
                && except_pairing_id != Some(row.pairing_id.as_str())
                && (legacy_label.is_some_and(|label| row.label == label)
                    || binding_matches_owner(&row.label, owner))
        })
        .map(|row| row.pairing_id)
        .collect();
    for id in &ids {
        store.revoke_pairing(id, at_ms)?;
    }
    Ok(ids.len())
}

/// Revoke every current route grant for exactly one host-canonical worktree,
/// independent of the mutable provider account or SSH connection settings in
/// its owner label. The worktree binding is the immutable authorization
/// boundary enforced by the server, so this retires an old owner after config
/// changes without touching any neighboring worktree.
fn revoke_worktree_grants(
    store: &dyn ControlStore,
    worktree: &str,
    except_pairing_id: Option<&str>,
    at_ms: i64,
) -> Result<usize> {
    let ids: Vec<String> = store
        .pairings()?
        .into_iter()
        .filter(|row| {
            row.kind == "token"
                && row.revoked_at.is_none()
                && except_pairing_id != Some(row.pairing_id.as_str())
                && matches!(
                    RouteToHostTokenBinding::parse_label(&row.label),
                    Some(Ok(binding)) if binding.worktree == worktree
                )
        })
        .map(|row| row.pairing_id)
        .collect();
    for id in &ids {
        store.revoke_pairing(id, at_ms)?;
    }
    Ok(ids.len())
}

fn revoke_owner(
    store: &dyn ControlStore,
    provider: &EnvProviderConfig,
    sandbox_id: &str,
    at_ms: i64,
) -> Result<usize> {
    let owner = provider_owner_label(provider, sandbox_id);
    // The exact label comparison retires credentials minted before worktree
    // binding existed; parsed labels cover current credentials.
    let legacy = format!("route-to-host:{owner}");
    revoke_owner_label(store, &owner, Some(&legacy), None, at_ms)
}

fn secure_control_origin(cfg: &Config, store: &dyn ControlStore, at_ms: i64) -> Result<String> {
    let transport = cfg
        .serve
        .resolve_transport(None, false)
        .map_err(anyhow::Error::msg)
        .context("route-to-host control endpoint is invalid")?;
    anyhow::ensure!(
        matches!(
            transport.exposure,
            ServeExposure::TlsTerminated | ServeExposure::Tunnel
        ),
        "route-to-host provisioning requires [serve] topology = \"tls-terminated\" or \"tunnel\"; plaintext direct endpoints are not eligible"
    );
    let row = store
        .daemons()?
        .into_iter()
        .filter(|row| row.control_origin.is_some())
        .filter(|row| {
            at_ms.saturating_sub(row.heartbeat_at)
                <= thegn_svc::control::client::DAEMON_HEARTBEAT_TTL_MS
        })
        .max_by_key(|row| row.heartbeat_at)
        .context(
            "route-to-host provisioning needs a live `thegn serve` endpoint; start the secure serving topology and retry",
        )?;
    let origin = row
        .control_origin
        .as_deref()
        .context("live serving daemon has no advertised control origin")?;
    anyhow::ensure!(
        origin.starts_with(&format!("{}://", transport.http_scheme())),
        "live serving daemon's advertised origin does not match the configured confidentiality topology; restart `thegn serve` and reprovision"
    );
    Ok(origin.to_string())
}

struct RouteCredential {
    origin: String,
    token: String,
    pairing_id: String,
}

fn rotate_credential(
    cfg: &Config,
    store: &dyn ControlStore,
    provider: &EnvProviderConfig,
    sandbox_id: &str,
    worktree: &str,
    at_ms: i64,
) -> Result<RouteCredential> {
    let origin = secure_control_origin(cfg, store, at_ms)?;
    // The immutable worktree binding, rather than today's mutable provider
    // account reference, retires every prior usable owner. Legacy unbound
    // labels are unusable at the authorization boundary but are cleaned up by
    // the current-owner pass as well.
    revoke_worktree_grants(store, worktree, None, at_ms)?;
    revoke_owner(store, provider, sandbox_id, at_ms)?;
    let minted = thegn_svc::control::auth::mint(
        TokenKind::Control,
        ScopeSet::of(&[Scope::MergeAdd]),
        &RouteToHostTokenBinding {
            owner: provider_owner_label(provider, sandbox_id),
            worktree: worktree.to_string(),
        }
        .label(),
        None,
        Some(at_ms.saturating_add(TOKEN_LIFETIME_MS)),
        at_ms,
    );
    store.put_pairing(&minted.row)?;
    Ok(RouteCredential {
        origin,
        token: minted.token,
        pairing_id: minted.row.pairing_id,
    })
}

fn install_credential(
    provider: &Provider,
    sandbox_id: &str,
    worktree: &str,
    credential: &RouteCredential,
) -> Result<()> {
    // Values travel in the provider exec's secret environment (Sprites JSON;
    // SSH-backed providers stream an exports preamble on stdin). The argv and
    // script contain variable names only. `umask 077` makes the runtime file
    // private from creation; it is written after base checkpointing.
    let path = credential_path(worktree);
    let script = format!(
        "umask 077; d=\"$HOME/{CREDENTIAL_DIR}\"; p=\"$HOME/{path}\"; \
         mkdir -p \"$d\" && chmod 700 \"$d\" || exit 73; t=\"$p.tmp.$$\"; \
         trap 'rm -f \"$t\"' EXIT HUP INT TERM; \
         printf '%s\\n%s\\n' \
         \"$THEGN_ROUTE_ORIGIN\" \"$THEGN_ROUTE_TOKEN\" > \"$t\" && \
         chmod 600 \"$t\" && mv -f \"$t\" \"$p\"; rc=$?; \
         trap - EXIT HUP INT TERM; [ $rc -eq 0 ] || {{ rm -f \"$t\"; exit $rc; }}"
    );
    let argv = vec!["/bin/sh".into(), "-lc".into(), script];
    let env = vec![
        ("THEGN_ROUTE_ORIGIN".into(), credential.origin.clone()),
        ("THEGN_ROUTE_TOKEN".into(), credential.token.clone()),
    ];
    let (code, _output) = crate::agent::block_on_provider(|| async {
        provider.run_exec(sandbox_id, &argv, None, &env).await
    })?;
    anyhow::ensure!(
        code == 0,
        "provider rejected route-to-host credential installation (exit {code})"
    );
    Ok(())
}

fn remove_credential_file(provider: &Provider, sandbox_id: &str, worktree: &str) -> Result<()> {
    let script = format!(
        "rm -f \"$HOME/{}\" \"$HOME/{LEGACY_CREDENTIAL_PATH}\"",
        credential_path(worktree)
    );
    let argv = vec!["/bin/sh".into(), "-lc".into(), script];
    let (code, _output) = crate::agent::block_on_provider(|| async {
        provider.run_exec(sandbox_id, &argv, None, &[]).await
    })?;
    anyhow::ensure!(
        code == 0,
        "provider rejected route-to-host credential removal (exit {code})"
    );
    Ok(())
}

fn read_provider_credential(
    provider: &Provider,
    sandbox_id: &str,
    worktree: &str,
) -> Result<Option<(String, String)>> {
    let path = credential_path(worktree);
    let script = format!(
        "p=\"$HOME/{path}\"; [ -r \"$p\" ] || exit 44; \
         printf '%s\\n' __THEGN_ROUTE_V1__; cat -- \"$p\""
    );
    let argv = vec!["/bin/sh".into(), "-lc".into(), script];
    let (code, output) = crate::agent::block_on_provider(|| async {
        provider.run_exec(sandbox_id, &argv, None, &[]).await
    })?;
    if code == 44 {
        return Ok(None);
    }
    anyhow::ensure!(
        code == 0,
        "provider rejected route-to-host credential read (exit {code})"
    );
    parse_credential_response(&output)
}

fn parse_credential_response(text: &str) -> Result<Option<(String, String)>> {
    let Some(payload) = text
        .split_once("__THEGN_ROUTE_V1__\n")
        .map(|(_, tail)| tail)
    else {
        anyhow::bail!("route-to-host credential response was missing its protocol marker");
    };
    let mut lines = payload.lines();
    let origin = lines.next().unwrap_or_default().to_string();
    let token = lines.next().unwrap_or_default().to_string();
    anyhow::ensure!(
        !origin.is_empty() && !token.is_empty(),
        "route-to-host credential file was incomplete"
    );
    Ok(Some((origin, token)))
}

/// Reconcile the route-to-host credential after provider provisioning. A valid
/// installed token is reused across reattach; mismatch/expiry rotates it. A
/// spare/base image receives none, and push mode actively retires stale grants.
pub(crate) fn reconcile_provider_credential(
    cfg: &Config,
    provider: &Provider,
    provider_config: &EnvProviderConfig,
    sandbox_id: &str,
    repo_root: &std::path::Path,
    worktree: &str,
    assigned_worktree: bool,
) -> Result<()> {
    let db = Db::open().context("open host state for route-to-host credential")?;
    let owner = provider_owner_label(provider_config, sandbox_id);
    // Take both identities in deterministic order. Worktree serialization
    // survives mutable owner settings; the owner lock also overlaps account
    // inventory/reaper teardown paths that cannot safely infer a worktree.
    let _credential_locks = credential_locks(Some(&owner), assigned_worktree.then_some(worktree))?;
    let mq = cfg.repo_merge_queue(repo_root);
    if !assigned_worktree || !mq.enabled || mq.remote_mode != MergeRemoteMode::RouteToHost {
        if assigned_worktree {
            revoke_worktree_grants(&db, worktree, None, now_ms())?;
        }
        revoke_owner(&db, provider_config, sandbox_id, now_ms())?;
        remove_credential_file(provider, sandbox_id, worktree)?;
        return Ok(());
    }
    let at_ms = now_ms();
    let origin = secure_control_origin(cfg, &db, at_ms)?;
    let installed = read_provider_credential(provider, sandbox_id, worktree)?;
    if let Some(credential) = reusable_credential(&db, &owner, worktree, &origin, installed, at_ms)
    {
        let legacy = format!("route-to-host:{owner}");
        revoke_worktree_grants(&db, worktree, Some(&credential.pairing_id), at_ms)?;
        revoke_owner_label(
            &db,
            &owner,
            Some(&legacy),
            Some(&credential.pairing_id),
            at_ms,
        )?;
        return Ok(());
    }
    let credential = rotate_credential(cfg, &db, provider_config, sandbox_id, worktree, now_ms())?;
    if let Err(error) = install_credential(provider, sandbox_id, worktree, &credential) {
        // The plaintext is dropped with `credential`; revoke the persisted hash
        // before returning so a partially-written file never remains useful.
        if let Err(revoke_error) = db.revoke_pairing(&credential.pairing_id, now_ms()) {
            return Err(anyhow::anyhow!(
                "install route-to-host credential failed: {error:#}; revoking the partially-installed credential also failed: {revoke_error:#}"
            ));
        }
        return Err(error.context("install route-to-host credential"));
    }
    Ok(())
}

/// Revoke every return token owned by a sandbox before it is recycled or
/// destroyed. Idempotent and safe to call on a sandbox that never had one.
/// Callers with durable worktree custody must pass it so mutable account
/// settings cannot strand an older grant. `None` deliberately stays within
/// the currently selected provider account: an inventory/reaper operating
/// after an account-ref change is controlling a different account, and must
/// not infer ownership from a coincidentally equal sandbox id.
pub(crate) fn revoke_for_sandbox(
    provider: &EnvProviderConfig,
    sandbox_id: &str,
    worktree: Option<&str>,
) -> Result<usize> {
    let db = Db::open().context("open host state to revoke route-to-host credential")?;
    let owner = provider_owner_label(provider, sandbox_id);
    let _credential_locks = credential_locks(Some(&owner), worktree)?;
    let at_ms = now_ms();
    let mut revoked = 0;
    if let Some(worktree) = worktree {
        revoked += revoke_worktree_grants(&db, worktree, None, at_ms)?;
    }
    revoked += revoke_owner(&db, provider, sandbox_id, at_ms)?;
    Ok(revoked)
}

fn ssh_command(target: &SshTarget, remote_script: &str) -> Command {
    let mut argv = target.ssh_base(true);
    argv.push(target.host.clone());
    argv.push(remote_script.to_string());
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]);
    command
}

#[expect(
    clippy::disallowed_methods,
    reason = "SSH credential reconciliation runs only on provisioning/CLI blocking workers"
)]
fn read_ssh_credential(target: &SshTarget, worktree: &str) -> Result<Option<(String, String)>> {
    let path = credential_path(worktree);
    let script = format!(
        "p=\"$HOME/{path}\"; [ -r \"$p\" ] || exit 44; \
         printf '%s\\n' __THEGN_ROUTE_V1__; cat -- \"$p\""
    );
    let output = ssh_command(target, &script)
        .output()
        .context("read existing route-to-host credential over SSH")?;
    if output.status.code() == Some(44) {
        return Ok(None);
    }
    anyhow::ensure!(
        output.status.success(),
        "SSH rejected route-to-host credential read (exit {})",
        output.status.code().unwrap_or(-1)
    );
    let text = String::from_utf8(output.stdout).context("SSH credential response was not UTF-8")?;
    parse_credential_response(&text)
}

fn install_ssh_credential(
    target: &SshTarget,
    worktree: &str,
    credential: &RouteCredential,
) -> Result<()> {
    let script = ssh_install_script(worktree);
    stream_credential(ssh_command(target, &script), credential)
}

fn ssh_install_script(worktree: &str) -> String {
    let path = credential_path(worktree);
    // The only dynamic argv value is an opaque SHA-256 filename. Credential
    // bytes cross the SSH channel on stdin and are written atomically under a
    // 0077 umask; they never enter argv, tracing, or captured output.
    format!(
        "umask 077; d=\"$HOME/{CREDENTIAL_DIR}\"; p=\"$HOME/{path}\"; \
         mkdir -p \"$d\" && chmod 700 \"$d\" || exit 73; \
         t=\"$p.tmp.$$\"; trap 'rm -f \"$t\"' EXIT HUP INT TERM; \
         IFS= read -r origin && IFS= read -r token && [ -n \"$origin\" ] && [ -n \"$token\" ] || exit 65; \
         printf '%s\\n%s\\n' \"$origin\" \"$token\" > \"$t\" && chmod 600 \"$t\" && mv -f \"$t\" \"$p\"; \
         rc=$?; trap - EXIT HUP INT TERM; [ $rc -eq 0 ] || {{ rm -f \"$t\"; exit $rc; }}"
    )
}

#[expect(
    clippy::disallowed_methods,
    reason = "SSH credential installation runs only on provisioning/CLI blocking workers"
)]
fn stream_credential(mut command: Command, credential: &RouteCredential) -> Result<()> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("start SSH route-to-host credential installation")?;
    let write_result = (|| -> std::io::Result<()> {
        let stdin = child.stdin.as_mut().expect("piped SSH stdin");
        stdin.write_all(credential.origin.as_bytes())?;
        stdin.write_all(b"\n")?;
        stdin.write_all(credential.token.as_bytes())?;
        stdin.write_all(b"\n")
    })();
    drop(child.stdin.take());
    let status = child
        .wait()
        .context("wait for SSH route-to-host credential installation")?;
    write_result.context("stream route-to-host credential over SSH")?;
    anyhow::ensure!(
        status.success(),
        "SSH rejected route-to-host credential installation (exit {})",
        status.code().unwrap_or(-1)
    );
    Ok(())
}

#[expect(
    clippy::disallowed_methods,
    reason = "SSH credential removal runs only on provisioning/teardown/CLI blocking workers"
)]
fn remove_ssh_credential(target: &SshTarget, worktree: &str) -> Result<()> {
    let path = credential_path(worktree);
    let output = ssh_command(target, &format!("rm -f \"$HOME/{path}\""))
        .output()
        .context("remove route-to-host credential over SSH")?;
    anyhow::ensure!(
        output.status.success(),
        "SSH rejected route-to-host credential removal (exit {})",
        output.status.code().unwrap_or(-1)
    );
    Ok(())
}

fn reusable_credential(
    store: &dyn ControlStore,
    owner: &str,
    worktree: &str,
    expected_origin: &str,
    installed: Option<(String, String)>,
    at_ms: i64,
) -> Option<RouteCredential> {
    let (origin, token) = installed?;
    if origin != expected_origin {
        return None;
    }
    let auth = thegn_svc::control::auth::verify(store, &token, at_ms)?;
    if auth.scopes != ScopeSet::of(&[Scope::MergeAdd]) {
        return None;
    }
    let binding = RouteToHostTokenBinding::parse_label(&auth.label)?.ok()?;
    if binding.owner != owner || binding.worktree != worktree {
        return None;
    }
    Some(RouteCredential {
        origin,
        token,
        pairing_id: auth.pairing_id,
    })
}

/// Ensure a user-managed SSH worktree has one reusable, worktree-bound return
/// credential before its pane starts. A valid installed credential is reused so
/// opening a second pane does not revoke the token already exported by the
/// first. Any mismatch rotates fail-closed: the old owner grants are revoked
/// before the replacement is installed, and a failed install revokes the new
/// token as well.
pub(crate) fn reconcile_ssh_credential(
    cfg: &Config,
    target: &SshTarget,
    repo_root: &std::path::Path,
    worktree: &str,
) -> Result<()> {
    let db = Db::open().context("open host state for SSH route-to-host credential")?;
    let owner = ssh_owner_label(target, worktree);
    let _credential_locks = credential_locks(Some(&owner), Some(worktree))?;
    let mq = cfg.repo_merge_queue(repo_root);
    let at_ms = now_ms();
    if !mq.enabled || mq.remote_mode != MergeRemoteMode::RouteToHost {
        revoke_worktree_grants(&db, worktree, None, at_ms)?;
        revoke_owner_label(&db, &owner, None, None, at_ms)?;
        // Retry file removal even when a prior attempt already committed the
        // revocation but lost its SSH connection before cleanup.
        remove_ssh_credential(target, worktree)?;
        return Ok(());
    }

    let origin = secure_control_origin(cfg, &db, at_ms)?;
    let installed = read_ssh_credential(target, worktree)?;
    if let Some(credential) = reusable_credential(&db, &owner, worktree, &origin, installed, at_ms)
    {
        // Clean up any orphan duplicate grants without invalidating the token
        // already held by live panes.
        revoke_worktree_grants(&db, worktree, Some(&credential.pairing_id), at_ms)?;
        revoke_owner_label(&db, &owner, None, Some(&credential.pairing_id), at_ms)?;
        return Ok(());
    }

    revoke_worktree_grants(&db, worktree, None, at_ms)?;
    revoke_owner_label(&db, &owner, None, None, at_ms)?;
    let minted = thegn_svc::control::auth::mint(
        TokenKind::Control,
        ScopeSet::of(&[Scope::MergeAdd]),
        &RouteToHostTokenBinding {
            owner,
            worktree: worktree.to_string(),
        }
        .label(),
        None,
        Some(at_ms.saturating_add(TOKEN_LIFETIME_MS)),
        at_ms,
    );
    db.put_pairing(&minted.row)?;
    let credential = RouteCredential {
        origin,
        token: minted.token,
        pairing_id: minted.row.pairing_id,
    };
    if let Err(error) = install_ssh_credential(target, worktree, &credential) {
        db.revoke_pairing(&credential.pairing_id, now_ms())?;
        return Err(error.context("install SSH route-to-host credential"));
    }
    Ok(())
}

/// Revoke and remove the return credential while the worktree's SSH ownership
/// metadata still exists. Revocation is authoritative; removal prevents an
/// unusable bearer string lingering in the remote account after deletion.
pub(crate) fn revoke_for_ssh(target: &SshTarget, worktree: &str) -> Result<usize> {
    let db = Db::open().context("open host state to revoke SSH route-to-host credential")?;
    let owner = ssh_owner_label(target, worktree);
    let _credential_locks = credential_locks(Some(&owner), Some(worktree))?;
    let at_ms = now_ms();
    let mut revoked = revoke_worktree_grants(&db, worktree, None, at_ms)?;
    revoked += revoke_owner_label(&db, &owner, None, None, at_ms)?;
    // Removal is independently idempotent. Always retry it: a prior teardown
    // may have committed revocation and then lost the SSH connection, leaving
    // zero live grants but a bearer string on disk.
    remove_ssh_credential(target, worktree)?;
    Ok(revoked)
}

/// Shell prefix for every off-host pane. The marker makes a missing credential
/// fail closed in `merge add`; the private file supplies two raw data lines.
/// `read -r` deliberately does not evaluate them as shell source.
pub(crate) fn source_prefix(worktree: &str) -> String {
    let path = credential_path(worktree);
    format!(
        "export THEGN_REMOTE_ENV=1; if [ -r \"$HOME/{path}\" ] && \
         {{ IFS= read -r THEGN_CONTROL_URL && IFS= read -r THEGN_CONTROL_TOKEN; }} \
         < \"$HOME/{path}\"; then export THEGN_CONTROL_URL THEGN_CONTROL_TOKEN; \
         else unset THEGN_CONTROL_URL THEGN_CONTROL_TOKEN; fi\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::config::ServeTopology;
    use thegn_core::store::{ControlStore, DaemonRow};

    fn serving_db(path: &std::path::Path, at_ms: i64) -> Db {
        let db = Db::open_at(path).unwrap();
        db.put_daemon(&DaemonRow {
            daemon_id: "serve".into(),
            pid: 1,
            scope: "/state".into(),
            endpoint: "/run/thegn.sock".into(),
            tcp_addr: Some("127.0.0.1:8484".into()),
            control_origin: Some("http://host.example:8443".into()),
            hostname: "host".into(),
            version: "test".into(),
            started_at: at_ms,
            heartbeat_at: at_ms,
        })
        .unwrap();
        db
    }

    fn provider(kind: &str, account_ref: &str) -> EnvProviderConfig {
        EnvProviderConfig {
            provider: kind.into(),
            api_key_env: account_ref.into(),
            ..EnvProviderConfig::default()
        }
    }

    fn ssh_target(host: &str) -> SshTarget {
        SshTarget::plain(host.to_string(), 22, false)
    }

    fn put_route_token(
        db: &Db,
        owner: String,
        worktree: &str,
        at_ms: i64,
    ) -> thegn_svc::control::auth::Minted {
        let minted = thegn_svc::control::auth::mint(
            TokenKind::Control,
            ScopeSet::of(&[Scope::MergeAdd]),
            &RouteToHostTokenBinding {
                owner,
                worktree: worktree.into(),
            }
            .label(),
            None,
            Some(at_ms + TOKEN_LIFETIME_MS),
            at_ms,
        );
        db.put_pairing(&minted.row).unwrap();
        minted
    }

    #[test]
    fn lifecycle_lock_keys_are_namespaced_and_deterministically_ordered() {
        let expected = vec!["owner:z".to_string(), "worktree:a".to_string()];
        assert_eq!(credential_lock_keys(Some("z"), Some("a")), expected);
        assert_eq!(
            credential_lock_keys(Some("same"), Some("same")),
            vec!["owner:same".to_string(), "worktree:same".to_string()],
            "namespacing prevents an owner label and worktree path from aliasing"
        );
    }

    #[test]
    fn rotated_token_is_merge_add_only_hashed_expiring_and_revocable() {
        let temp = tempfile::tempdir().unwrap();
        let at_ms = 10_000;
        let db = serving_db(&temp.path().join("db.sqlite"), at_ms);
        let mut cfg = Config::default();
        cfg.serve.topology = ServeTopology::Tunnel;
        // The live daemon applied CLI advertise overrides that differ from the
        // static config visible to this provisioning process. Its registry
        // origin is authoritative.
        cfg.serve.advertise_host = "stale-config.example".into();
        cfg.serve.advertise_port = 9443;
        let provider = provider("sprites", "SPRITES_ACCOUNT_A");

        let first =
            rotate_credential(&cfg, &db, &provider, "sandbox-a", "/remote/a", at_ms).unwrap();
        assert_eq!(first.origin, "http://host.example:8443");
        let row = db
            .pairings()
            .unwrap()
            .into_iter()
            .find(|row| row.pairing_id == first.pairing_id)
            .unwrap();
        assert_eq!(row.scope, "merge_add");
        assert!(!row.token_hash.contains(&first.token));
        assert_eq!(row.expires_at, Some(at_ms + TOKEN_LIFETIME_MS));
        let ctx = thegn_svc::control::auth::verify(&db, &first.token, at_ms + 1).unwrap();
        assert!(ctx.require(Scope::MergeAdd).is_ok());
        assert!(ctx.require(Scope::Read).is_err());
        assert!(ctx.require(Scope::Git).is_err());

        let second =
            rotate_credential(&cfg, &db, &provider, "sandbox-a", "/remote/a", at_ms + 2).unwrap();
        assert!(thegn_svc::control::auth::verify(&db, &first.token, at_ms + 3).is_none());
        assert!(thegn_svc::control::auth::verify(&db, &second.token, at_ms + 3).is_some());
        assert_eq!(
            revoke_owner(&db, &provider, "sandbox-a", at_ms + 4).unwrap(),
            1
        );
        assert!(thegn_svc::control::auth::verify(&db, &second.token, at_ms + 5).is_none());
    }

    #[test]
    fn plaintext_direct_endpoint_is_never_injected() {
        let temp = tempfile::tempdir().unwrap();
        let db = serving_db(&temp.path().join("db.sqlite"), 10_000);
        let error = rotate_credential(
            &Config::default(),
            &db,
            &provider("sprites", "SPRITES_ACCOUNT_A"),
            "sandbox-a",
            "/remote/a",
            10_000,
        )
        .err()
        .expect("plaintext direct must fail")
        .to_string();
        assert!(error.contains("tls-terminated") && error.contains("tunnel"));
        assert!(db.pairings().unwrap().is_empty());
    }

    #[test]
    fn equal_sandbox_ids_in_different_provider_accounts_are_isolated() {
        let temp = tempfile::tempdir().unwrap();
        let at_ms = 10_000;
        let db = serving_db(&temp.path().join("db.sqlite"), at_ms);
        let mut cfg = Config::default();
        cfg.serve.topology = ServeTopology::Tunnel;
        cfg.serve.advertise_host = "host.example".into();
        cfg.serve.advertise_port = 8443;
        let account_a = provider("sprites", "SPRITES_ACCOUNT_A");
        let account_b = provider("sprites", "SPRITES_ACCOUNT_B");

        let a = rotate_credential(&cfg, &db, &account_a, "same-id", "/remote/a", at_ms).unwrap();
        let b =
            rotate_credential(&cfg, &db, &account_b, "same-id", "/remote/b", at_ms + 1).unwrap();
        assert_ne!(
            provider_owner_label(&account_a, "same-id"),
            provider_owner_label(&account_b, "same-id")
        );
        assert_eq!(
            revoke_owner(&db, &account_a, "same-id", at_ms + 2).unwrap(),
            1
        );
        assert!(thegn_svc::control::auth::verify(&db, &a.token, at_ms + 3).is_none());
        assert!(thegn_svc::control::auth::verify(&db, &b.token, at_ms + 3).is_some());
    }

    #[test]
    fn provider_account_change_retires_prior_worktree_grant_only() {
        let temp = tempfile::tempdir().unwrap();
        let at_ms = 10_000;
        let db = serving_db(&temp.path().join("db.sqlite"), at_ms);
        let mut cfg = Config::default();
        cfg.serve.topology = ServeTopology::Tunnel;
        cfg.serve.advertise_host = "host.example".into();
        cfg.serve.advertise_port = 8443;
        let old_account = provider("sprites", "SPRITES_ACCOUNT_A");
        let new_account = provider("sprites", "SPRITES_ACCOUNT_B");

        let old =
            rotate_credential(&cfg, &db, &old_account, "same-id", "/remote/a", at_ms).unwrap();
        let neighbor =
            rotate_credential(&cfg, &db, &old_account, "other-id", "/remote/b", at_ms + 1).unwrap();
        let replacement =
            rotate_credential(&cfg, &db, &new_account, "same-id", "/remote/a", at_ms + 2).unwrap();

        assert!(thegn_svc::control::auth::verify(&db, &old.token, at_ms + 3).is_none());
        assert!(thegn_svc::control::auth::verify(&db, &replacement.token, at_ms + 3).is_some());
        assert!(
            thegn_svc::control::auth::verify(&db, &neighbor.token, at_ms + 3).is_some(),
            "account migration must not revoke another worktree"
        );
    }

    #[test]
    fn account_inventory_cleanup_does_not_cross_account_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let db = Db::open_at(&temp.path().join("db.sqlite")).unwrap();
        let old_account = provider("sprites", "SPRITES_ACCOUNT_A");
        let new_account = provider("sprites", "SPRITES_ACCOUNT_B");
        let old = put_route_token(
            &db,
            provider_owner_label(&old_account, "same-id"),
            "/remote/a",
            10_000,
        );

        // This is the safe policy used by `revoke_for_sandbox(..., None)`:
        // after an account-ref change, current inventory controls a different
        // provider account even when it happens to expose the same sandbox id.
        assert_eq!(
            revoke_owner(&db, &new_account, "same-id", 10_001).unwrap(),
            0
        );
        assert!(thegn_svc::control::auth::verify(&db, &old.token, 10_002).is_some());
        assert_eq!(
            revoke_worktree_grants(&db, "/remote/a", None, 10_003).unwrap(),
            1,
            "durable worktree teardown remains authoritative across account changes"
        );
    }

    #[test]
    fn source_prefix_contains_only_path_and_fail_closed_marker() {
        let prefix = source_prefix("/host/worktree-a");
        assert!(prefix.contains("THEGN_REMOTE_ENV=1"));
        assert!(prefix.contains(CREDENTIAL_DIR));
        assert!(!prefix.contains("/host/worktree-a"));
        assert!(!prefix.contains("THEGN_CONTROL_TOKEN="));
        assert!(!prefix.contains(". \"$HOME"));
    }

    #[test]
    fn ssh_owners_and_files_are_isolated_per_account_host_and_worktree() {
        let a = ssh_target("alice@build.example");
        let b = ssh_target("bob@build.example");
        assert_ne!(
            ssh_owner_label(&a, "/host/worktree-a"),
            ssh_owner_label(&a, "/host/worktree-b")
        );
        assert_ne!(
            ssh_owner_label(&a, "/host/worktree-a"),
            ssh_owner_label(&b, "/host/worktree-a")
        );
        assert_ne!(
            credential_path("/host/worktree-a"),
            credential_path("/host/worktree-b")
        );
        assert!(!credential_path("/host/worktree-a").contains("/host/worktree-a"));
    }

    #[test]
    fn ssh_target_change_retires_prior_worktree_grant_only() {
        let temp = tempfile::tempdir().unwrap();
        let db = Db::open_at(&temp.path().join("db.sqlite")).unwrap();
        let at_ms = 10_000;
        let old_target = ssh_target("alice@old-build.example");
        let new_target = ssh_target("alice@new-build.example");
        let old = put_route_token(
            &db,
            ssh_owner_label(&old_target, "/host/worktree-a"),
            "/host/worktree-a",
            at_ms,
        );
        let neighbor = put_route_token(
            &db,
            ssh_owner_label(&old_target, "/host/worktree-b"),
            "/host/worktree-b",
            at_ms + 1,
        );

        assert_ne!(
            ssh_owner_label(&old_target, "/host/worktree-a"),
            ssh_owner_label(&new_target, "/host/worktree-a")
        );
        assert_eq!(
            revoke_worktree_grants(&db, "/host/worktree-a", None, at_ms + 2).unwrap(),
            1
        );
        assert!(thegn_svc::control::auth::verify(&db, &old.token, at_ms + 3).is_none());
        assert!(
            thegn_svc::control::auth::verify(&db, &neighbor.token, at_ms + 3).is_some(),
            "SSH target migration must not revoke another worktree"
        );
    }

    #[test]
    fn valid_ssh_credential_is_reused_without_revoking_live_panes() {
        let temp = tempfile::tempdir().unwrap();
        let db = Db::open_at(&temp.path().join("db.sqlite")).unwrap();
        let target = ssh_target("alice@build.example");
        let owner = ssh_owner_label(&target, "/host/worktree-a");
        let minted = thegn_svc::control::auth::mint(
            TokenKind::Control,
            ScopeSet::of(&[Scope::MergeAdd]),
            &RouteToHostTokenBinding {
                owner: owner.clone(),
                worktree: "/host/worktree-a".into(),
            }
            .label(),
            None,
            Some(20_000),
            10_000,
        );
        db.put_pairing(&minted.row).unwrap();

        let reused = reusable_credential(
            &db,
            &owner,
            "/host/worktree-a",
            "https://control.example",
            Some(("https://control.example".into(), minted.token.clone())),
            10_001,
        )
        .expect("matching installed credential is reusable");
        assert_eq!(reused.pairing_id, minted.row.pairing_id);
        assert!(thegn_svc::control::auth::verify(&db, &minted.token, 10_002).is_some());
        assert!(
            reusable_credential(
                &db,
                &owner,
                "/host/worktree-b",
                "https://control.example",
                Some(("https://control.example".into(), minted.token)),
                10_001,
            )
            .is_none(),
            "a worktree-bound token is never reusable by its neighbor"
        );
    }

    #[test]
    fn valid_provider_reattach_credential_is_reused_and_duplicates_are_retired() {
        let temp = tempfile::tempdir().unwrap();
        let db = Db::open_at(&temp.path().join("db.sqlite")).unwrap();
        let provider = provider("sprites", "SPRITES_ACCOUNT_A");
        let owner = provider_owner_label(&provider, "sandbox-a");
        let mint = |at_ms| {
            thegn_svc::control::auth::mint(
                TokenKind::Control,
                ScopeSet::of(&[Scope::MergeAdd]),
                &RouteToHostTokenBinding {
                    owner: owner.clone(),
                    worktree: "/host/worktree-a".into(),
                }
                .label(),
                None,
                Some(20_000),
                at_ms,
            )
        };
        let installed = mint(10_000);
        let duplicate = mint(10_001);
        db.put_pairing(&installed.row).unwrap();
        db.put_pairing(&duplicate.row).unwrap();

        let reused = reusable_credential(
            &db,
            &owner,
            "/host/worktree-a",
            "https://control.example",
            Some(("https://control.example".into(), installed.token.clone())),
            10_002,
        )
        .expect("reattach reuses the credential already exported by live panes");
        assert_eq!(reused.pairing_id, installed.row.pairing_id);
        assert_eq!(
            revoke_owner_label(&db, &owner, None, Some(&reused.pairing_id), 10_003).unwrap(),
            1
        );
        assert!(thegn_svc::control::auth::verify(&db, &installed.token, 10_004).is_some());
        assert!(thegn_svc::control::auth::verify(&db, &duplicate.token, 10_004).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn ssh_install_streams_secret_off_argv_into_owner_only_atomic_file() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let worktree = "/host/worktree-a";
        let credential = RouteCredential {
            origin: "https://control.example".into(),
            token: "tg_1_secret-must-not-be-in-argv".into(),
            pairing_id: "public-id".into(),
        };
        let script = ssh_install_script(worktree);
        assert!(!script.contains(&credential.origin));
        assert!(!script.contains(&credential.token));
        assert!(!script.contains(worktree));

        let mut command = Command::new("/bin/sh");
        command.args(["-c", &script]).env("HOME", temp.path());
        assert!(!format!("{command:?}").contains(&credential.token));
        stream_credential(command, &credential).unwrap();

        let installed = temp.path().join(credential_path(worktree));
        assert_eq!(
            std::fs::read_to_string(&installed).unwrap(),
            format!("{}\n{}\n", credential.origin, credential.token)
        );
        assert_eq!(
            std::fs::metadata(&installed).unwrap().permissions().mode() & 0o077,
            0,
            "credential file must be owner-only"
        );
        assert!(
            std::fs::read_dir(installed.parent().unwrap())
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".tmp.")),
            "atomic-install temp file was cleaned"
        );
    }
}
