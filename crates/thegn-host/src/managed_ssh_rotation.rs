//! Transactional managed-SSH-key rotation.
//!
//! Every instance first receives and verifies the replacement. Only after the
//! entire scope verifies do we promote the local pair and remove the old key.
//! A de-authorization failure re-adds the old key everywhere before restoring
//! the old local pair, preserving the invariant that partial failure leaves both
//! keys usable instead of stranding any managed machine.
//!
//! A crash after the last custody row is cleaned but before the retired or
//! auxiliary local pair is deleted leaves that pair on disk with no custody
//! row. It is never authorized anywhere, and the next rotation of that key
//! removes it: under the scope lease, once recovery has settled, every
//! `<stem>.next-<id>` / `<stem>.retiring-<id>` file (and its `.pub`) that no
//! custody row of any key names is deleted. The sweep is skipped when another
//! managed key shares the same sibling namespace.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};
use thegn_core::config::Config;
use thegn_core::managed_ssh::{AuthorizedKeyRecord, CustodyChanged, RotationRecovery};

static ROTATION_NONCE: AtomicU64 = AtomicU64::new(0);

pub struct RotationSummary {
    pub scopes: usize,
    pub instances: usize,
    pub dry_run: bool,
}

struct ScopeLease {
    _file: std::fs::File,
}

#[derive(Debug)]
struct RotationLeaseHeld {
    key_name: String,
}

impl std::fmt::Display for RotationLeaseHeld {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "another `thegn secret ssh rotate` is already rotating {}; nothing was changed",
            self.key_name
        )
    }
}

impl std::error::Error for RotationLeaseHeld {}

fn acquire_scope_lease_in(lock_dir: &Path, key_path: &Path) -> Result<ScopeLease> {
    let anchor = lock_dir
        .parent()
        .ok_or_else(|| anyhow!("managed SSH rotation lease directory has no parent"))?;
    let lock_name = lock_dir
        .file_name()
        .ok_or_else(|| anyhow!("managed SSH rotation lease directory has no name"))?;
    if !anchor.exists() {
        thegn_core::fsperm::ensure_private_dir(anchor).with_context(|| {
            format!(
                "secure managed SSH rotation lease anchor {}",
                anchor.display()
            )
        })?;
    }
    let mut digest = Sha256::new();
    digest.update(b"thegn-ssh-rotation-lease-v1\0");
    digest.update(key_path.as_os_str().as_encoded_bytes());
    let path = lock_dir.join(format!("{:x}.lock", digest.finalize()));
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("managed SSH rotation lease path has no valid file name"))?;
    let private_dir = thegn_core::fsperm::ensure_private_dir_beneath(anchor, Path::new(lock_name))
        .with_context(|| {
            format!(
                "secure managed SSH rotation lease directory {}",
                lock_dir.display()
            )
        })?;
    let file = private_dir
        .open_or_create_regular_nofollow(file_name)
        .with_context(|| format!("open managed SSH rotation lease {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(ScopeLease { _file: file }),
        Err(std::fs::TryLockError::WouldBlock) => Err(RotationLeaseHeld {
            key_name: key_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("managed key")
                .to_string(),
        }
        .into()),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

fn acquire_scope_lease(key_path: &Path) -> Result<ScopeLease> {
    let lock_dir = thegn_core::managed_ssh::dir().with_file_name("rotation-leases");
    acquire_scope_lease_in(&lock_dir, key_path)
}

pub fn rotate(cfg: &Config, account: Option<&str>, dry_run: bool) -> Result<RotationSummary> {
    let all =
        thegn_core::managed_ssh::list().context("read complete managed SSH custody inventory")?;
    let mut selected = all.clone();
    if let Some(account) = account {
        selected.retain(|record| record.account == account);
    }
    if selected.is_empty() {
        let available = all
            .into_iter()
            .map(|record| record.account)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "no managed SSH authorizations match account {}; recorded accounts: {}",
            account.unwrap_or("*"),
            if available.is_empty() {
                "none"
            } else {
                &available
            }
        );
    }

    let instances = selected.len();
    let mut scopes: BTreeMap<PathBuf, Vec<AuthorizedKeyRecord>> = BTreeMap::new();
    for record in selected {
        scopes
            .entry(record.key_path.clone())
            .or_default()
            .push(record);
    }
    if dry_run {
        for (path, records) in &scopes {
            thegn_core::outln!(
                "would rotate {} ({} live authorization(s))",
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("managed key"),
                records.len()
            );
            let orphans = match plan_orphan_sweep(path, &all) {
                Ok(Some(sweep)) => sweep.orphans.len(),
                Ok(None) => 0,
                Err(error) => {
                    tracing::debug!(
                        key = %path.display(),
                        %error,
                        "could not scan for orphaned rotation key files",
                    );
                    0
                }
            };
            if orphans > 0 {
                thegn_core::outln!("would remove {orphans} orphaned rotation key file(s)");
            }
        }
        return Ok(RotationSummary {
            scopes: scopes.len(),
            instances,
            dry_run: true,
        });
    }

    for (key_path, records) in &scopes {
        let _lease = acquire_scope_lease(key_path)?;
        let mut current = thegn_core::managed_ssh::list()
            .context("re-read managed SSH custody inventory under rotation lease")?
            .into_iter()
            .filter(|record| {
                record.key_path == *key_path
                    && account.is_none_or(|account| record.account == account)
            })
            .collect::<Vec<_>>();
        let mut snapshot = records.clone();
        sort_scope_rows(&mut current);
        sort_scope_rows(&mut snapshot);
        anyhow::ensure!(
            current == snapshot,
            "managed SSH custody for {} changed while this rotation was starting; re-run",
            key_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("managed key")
        );
        rotate_scope(&ProviderRemote { cfg, key_path }, key_path, records)?;
    }
    Ok(RotationSummary {
        scopes: scopes.len(),
        instances,
        dry_run: false,
    })
}

/// Remote and key-generation side effects of one scope's rotation. Production
/// resolves a provider per custody row; tests drive the whole transaction,
/// including interruption and resume, against local stand-in machines.
trait RotationRemote {
    type Transport;

    /// Resolve credentials and build a transport authenticating with `key`.
    /// `label` names the transport in the unavailable-provider error.
    fn connect(
        &self,
        record: &AuthorizedKeyRecord,
        key: &Path,
        public: &str,
        label: &str,
    ) -> Result<Self::Transport>;

    /// Run `script` on the row's instance, failing on a non-zero exit.
    fn run(
        &self,
        transport: &Self::Transport,
        record: &AuthorizedKeyRecord,
        script: &str,
        operation: &str,
    ) -> Result<()>;

    /// Prove an actual remote command authenticates with the replacement key.
    fn verify(&self, record: &AuthorizedKeyRecord, key: &Path, public: &str) -> Result<()>;

    fn generate_keypair(&self, key: &Path) -> Result<()>;

    /// Called after every durable rotation or recovery step. Production does
    /// nothing; fault-injection tests stop the process here.
    fn phase(&self, _name: &'static str) {}
}

struct ProviderRemote<'a> {
    cfg: &'a Config,
    /// Scope key that selects the configured provider account for each row.
    key_path: &'a Path,
}

impl RotationRemote for ProviderRemote<'_> {
    type Transport = thegn_svc::provider::Provider;

    fn connect(
        &self,
        record: &AuthorizedKeyRecord,
        key: &Path,
        public: &str,
        label: &str,
    ) -> Result<Self::Transport> {
        let pc = provider_config(self.cfg, record, self.key_path)?;
        crate::provider_factory::provider_for_named_with_key(
            pc,
            &record.instance,
            Some((key.to_path_buf(), public.trim().to_string())),
        )
        .ok_or_else(|| anyhow!("{} {label} is unavailable", record.provider))
    }

    fn run(
        &self,
        transport: &Self::Transport,
        record: &AuthorizedKeyRecord,
        script: &str,
        operation: &str,
    ) -> Result<()> {
        run_checked(transport, &record.instance, script, operation)
    }

    fn verify(&self, record: &AuthorizedKeyRecord, key: &Path, public: &str) -> Result<()> {
        if record.provider == "sprites" {
            provider_config(self.cfg, record, self.key_path)?;
            run_sprite_checked(record, key, "true", "verify replacement key")
        } else {
            let replacement = self.connect(record, key, public, "replacement-key provider")?;
            run_checked(
                &replacement,
                &record.instance,
                "true",
                "verify replacement key",
            )
        }
    }

    fn generate_keypair(&self, key: &Path) -> Result<()> {
        crate::agent::generate_managed_keypair_at(key, "thegn-managed-rotation")
    }
}

fn sort_scope_rows(records: &mut [AuthorizedKeyRecord]) {
    records.sort_by(|left, right| {
        (&left.provider, &left.account, &left.instance).cmp(&(
            &right.provider,
            &right.account,
            &right.instance,
        ))
    });
}

fn rotate_scope<R: RotationRemote>(
    remote: &R,
    key_path: &Path,
    records: &[AuthorizedKeyRecord],
) -> Result<()> {
    let mut tracked = records.to_vec();
    let resumed_recovery = tracked
        .iter()
        .any(|record| record.rotation_recovery.is_some());
    recover_pending(remote, key_path, &mut tracked).with_context(|| {
        format!(
            "recover an interrupted managed SSH rotation for {}",
            key_path.display()
        )
    })?;
    // Recovery has settled and no staging pair exists yet, so every sibling
    // file no custody row names is a leftover of an earlier crash.
    sweep_orphans_under_lease(key_path);
    if resumed_recovery {
        tracing::info!(
            target: "thegn::secret::audit",
            consumer = "secret.ssh.rotate",
            key = %key_path.display(),
            outcome = "recovered",
            "interrupted managed SSH key rotation recovered",
        );
        return Ok(());
    }
    if let Some(record) = tracked
        .iter()
        .find(|record| record.provider == "sprites" && record.proxy_worktree.is_none())
    {
        bail!(
            "cannot safely rotate {}: Sprites SSH-over-WSS record {} predates the originating-worktree custody field needed to verify a replacement-key SSH connection; the current key and every remote authorization are unchanged",
            key_path.display(),
            record.instance,
        );
    }
    let old_public_path = public_path(key_path);
    let old_public = std::fs::read_to_string(&old_public_path)
        .with_context(|| format!("read managed public key {}", old_public_path.display()))?;
    let old_key_id = key_id(&old_public)?;
    let staging = rotation_sibling(key_path, "next");
    let backup = rotation_sibling(key_path, "retiring");
    remote.generate_keypair(&staging)?;
    let staging_public_path = public_path(&staging);
    let new_public = std::fs::read_to_string(&staging_public_path).with_context(|| {
        format!(
            "read replacement public key {}",
            staging_public_path.display()
        )
    })?;
    let new_key_id = key_id(&new_public)?;
    let recovery = RotationRecovery {
        key_path: staging.clone(),
        key_fingerprint: thegn_core::managed_ssh::key_fingerprint(&new_public),
        backup_key_path: Some(backup.clone()),
        authorized: false,
        verified: false,
        retiring_started: false,
        retiring_completed: false,
    };
    if let Err(error) = persist_recovery_for_all(&mut tracked, &recovery) {
        if error.downcast_ref::<CustodyChanged>().is_some() {
            return Err(error.context("record replacement-key recovery plan"));
        }
        let cleanup = clear_unstarted_recovery(&mut tracked, &staging);
        return Err(combine_recovery_error(
            error.context("record replacement-key recovery plan"),
            cleanup,
            &staging,
        ));
    }
    remote.phase("plan-recorded");

    // Phase 1: old-key transport authorizes the replacement, then a separately
    // constructed provider proves an actual command with the replacement key.
    for index in 0..tracked.len() {
        let phase = (|| -> Result<()> {
            let record = &tracked[index];
            let old = remote.connect(record, key_path, &old_public, "provider")?;
            remote.run(
                &old,
                record,
                &authorize_script(&new_public, &new_key_id)?,
                "authorize replacement key",
            )?;
            Ok(())
        })();
        if let Err(error) = phase {
            let cleanup = recover_pending(remote, key_path, &mut tracked);
            return Err(combine_recovery_error(error, cleanup, &staging));
        }
        remote.phase("replacement-authorized");
        let mut recovery = tracked[index]
            .rotation_recovery
            .clone()
            .expect("recovery plan was persisted before remote mutation");
        recovery.authorized = true;
        if let Err(error) = update_recovery(&mut tracked[index], recovery) {
            if error.downcast_ref::<CustodyChanged>().is_some() {
                return Err(error.context("record replacement authorization"));
            }
            let cleanup = recover_pending(remote, key_path, &mut tracked);
            return Err(combine_recovery_error(
                error.context("record replacement authorization"),
                cleanup,
                &staging,
            ));
        }
        remote.phase("authorization-recorded");

        if let Err(error) = remote.verify(&tracked[index], &staging, &new_public) {
            let cleanup = recover_pending(remote, key_path, &mut tracked);
            return Err(combine_recovery_error(error, cleanup, &staging));
        }
        remote.phase("replacement-verified");
        let mut recovery = tracked[index]
            .rotation_recovery
            .clone()
            .expect("authorized recovery plan remains present");
        recovery.verified = true;
        if let Err(error) = update_recovery(&mut tracked[index], recovery) {
            if error.downcast_ref::<CustodyChanged>().is_some() {
                return Err(error.context("record replacement verification"));
            }
            let cleanup = recover_pending(remote, key_path, &mut tracked);
            return Err(combine_recovery_error(
                error.context("record replacement verification"),
                cleanup,
                &staging,
            ));
        }
        remote.phase("verification-recorded");
    }

    // Resolve credentials and construct every canonical-path replacement
    // provider before touching local custody. Nothing after this point can fail
    // merely because a token/keyring backend disappeared between phases.
    let canonical_providers = tracked
        .iter()
        .map(|record| {
            remote.connect(
                record,
                key_path,
                &new_public,
                "canonical replacement-key provider",
            )
        })
        .collect::<Result<Vec<_>>>();
    let canonical_providers = match canonical_providers {
        Ok(providers) => providers,
        Err(error) => {
            let cleanup = recover_pending(remote, key_path, &mut tracked);
            return Err(combine_recovery_error(error, cleanup, &staging));
        }
    };

    // Conservatively mark retirement before promotion. If the process exits at
    // any later boundary, recovery first re-authorizes the primary key across
    // the whole scope before removing the auxiliary key.
    for index in 0..tracked.len() {
        let mut recovery = tracked[index]
            .rotation_recovery
            .clone()
            .expect("verified recovery plan remains present");
        recovery.retiring_started = true;
        if let Err(error) = update_recovery(&mut tracked[index], recovery) {
            if error.downcast_ref::<CustodyChanged>().is_some() {
                return Err(error.context("record old-key retirement boundary"));
            }
            let cleanup = recover_pending(remote, key_path, &mut tracked);
            return Err(combine_recovery_error(
                error.context("record old-key retirement boundary"),
                cleanup,
                &staging,
            ));
        }
        remote.phase("retirement-recorded");
    }

    // Promote locally while retaining a rollback copy of the old pair. Remote
    // de-authorization below uses providers reconstructed against the canonical
    // path, so every future caller also sees the replacement immediately.
    if let Err(error) = promote_pair(key_path, &staging, &backup) {
        let cleanup = recover_pending(remote, key_path, &mut tracked);
        return Err(combine_recovery_error(
            error.context("promote replacement managed SSH key"),
            cleanup,
            &staging,
        ));
    }
    remote.phase("pair-promoted");

    let mut deauthorized = 0usize;
    for (record, replacement) in tracked.iter().zip(&canonical_providers) {
        if let Err(error) = remote.run(
            replacement,
            record,
            &deauthorize_script(&old_key_id)?,
            "de-authorize old key",
        ) {
            // Best effort, across the whole scope rather than only completed
            // records: append is idempotent and this maximizes the both-keys-live
            // safety invariant before restoring the old local pair.
            let restore_errors = attempt_all(tracked.len(), |index| {
                remote.run(
                    &canonical_providers[index],
                    &tracked[index],
                    &authorize_script(&old_public, &old_key_id)?,
                    "restore old key after partial failure",
                )
            })
            .into_iter()
            .map(|(index, error)| format!("{}: {error:#}", tracked[index].instance))
            .collect::<Vec<_>>();
            if restore_errors.is_empty() {
                if let Err(restore_error) = restore_pair(key_path, &staging, &backup) {
                    return Err(anyhow!(
                        "managed SSH rotation stopped while removing the old key: {error:#}; old-key remote authorization was restored, but local custody restore failed: {restore_error:#}. Recovery remains recorded at {}",
                        staging.display()
                    ));
                }
                let cleanup = recover_pending(remote, key_path, &mut tracked);
                return Err(combine_recovery_error(
                    error.context(
                        "managed SSH rotation stopped; the old key was restored and the replacement was rolled back",
                    ),
                    cleanup,
                    &staging,
                ));
            }

            // At least one machine may now accept only the replacement. Keep
            // the verified replacement canonical; the common recovery plan
            // still records both its pre-promotion path and the old backup, so
            // a later retry can repair old access and finish the rollback.
            bail!(
                "managed SSH rotation stopped while removing the old key: {error:#}. The replacement remains canonical and usable; old-key rollback was incomplete ({}) and the retiring pair remains tracked for recovery at {}",
                restore_errors.join("; "),
                backup.display()
            );
        }
        deauthorized += 1;
        remote.phase("old-key-removed");
    }

    // Mark the all-remotes-retired boundary durably before clearing individual
    // recovery rows. If a later custody write fails partway, any surviving row
    // proves it is safe to complete forward to the canonical replacement.
    let mut completion_errors = Vec::new();
    for record in &mut tracked {
        let Some(mut recovery) = record.rotation_recovery.clone() else {
            continue;
        };
        recovery.retiring_completed = true;
        if let Err(error) = update_recovery(record, recovery) {
            let custody_changed = error.downcast_ref::<CustodyChanged>().is_some();
            completion_errors.push(error);
            if custody_changed {
                break;
            }
        } else {
            remote.phase("retirement-completion-recorded");
        }
    }
    if let Some(error) = completion_errors.into_iter().next() {
        if error.downcast_ref::<CustodyChanged>().is_some() {
            return Err(error.context("record completed remote retirement boundary"));
        }
        let cleanup = recover_pending(remote, key_path, &mut tracked);
        return Err(combine_recovery_error(
            error.context("record completed remote retirement boundary"),
            cleanup,
            &staging,
        ));
    }

    for record in &tracked {
        thegn_core::managed_ssh::record_rotation_completed(record, key_path, &new_public)?;
        remote.phase("custody-completed");
    }
    // Custody now reflects the working canonical key even if deleting retired
    // local material fails. Surface that cleanup failure without lying about
    // which key future connections use.
    remove_pair(&backup).context(
        "rotation completed remotely and custody was updated, but retiring the old local key failed",
    )?;
    tracing::info!(
        target: "thegn::secret::audit",
        consumer = "secret.ssh.rotate",
        key_fingerprint = %thegn_core::managed_ssh::key_fingerprint(&new_public),
        instances = deauthorized,
        outcome = "rotated",
        "managed SSH key rotation completed",
    );
    Ok(())
}

fn rotation_sibling(key_path: &Path, label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let nonce = ROTATION_NONCE.fetch_add(1, Ordering::Relaxed);
    let identity = thegn_core::util::short_hash(
        &format!(
            "{}\0{}\0{nanos}\0{nonce}",
            key_path.display(),
            std::process::id()
        ),
        SIBLING_ID_LEN,
    );
    key_path.with_extension(format!("{label}-{identity}"))
}

/// Length of the identity [`rotation_sibling`] appends. `short_hash` draws it
/// from the lowercase base36 alphabet `[0-9a-z]`.
const SIBLING_ID_LEN: usize = 16;

/// Every label [`rotation_sibling`] is called with.
const SIBLING_LABELS: [&str; 2] = ["next", "retiring"];

/// Whether `name` is exactly a [`rotation_sibling`] name (or its `.pub`) for a
/// key whose file stem is `stem`: `<stem>.(next|retiring)-[0-9a-z]{16}(.pub)?`.
fn is_rotation_sibling_name(stem: &str, name: &str) -> bool {
    let Some(rest) = name
        .strip_prefix(stem)
        .and_then(|rest| rest.strip_prefix('.'))
    else {
        return false;
    };
    let rest = rest.strip_suffix(".pub").unwrap_or(rest);
    SIBLING_LABELS.iter().any(|label| {
        rest.strip_prefix(label)
            .and_then(|rest| rest.strip_prefix('-'))
            .is_some_and(|id| {
                id.len() == SIBLING_ID_LEN
                    && id
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || byte.is_ascii_lowercase())
            })
    })
}

/// Every local key file a custody row of ANY key still names: each canonical
/// pair and every recorded recovery's auxiliary and retiring pair.
fn referenced_key_files(custody: &[AuthorizedKeyRecord]) -> BTreeSet<PathBuf> {
    let mut referenced = BTreeSet::new();
    let mut add_pair = |private: &Path| {
        referenced.insert(private.to_path_buf());
        referenced.insert(public_path(private));
    };
    for record in custody {
        add_pair(&record.key_path);
        if let Some(recovery) = &record.rotation_recovery {
            add_pair(&recovery.key_path);
            if let Some(backup) = &recovery.backup_key_path {
                add_pair(backup);
            }
        }
    }
    referenced
}

fn same_dir(left: &Path, right: &Path) -> bool {
    left == right
        || matches!(
            (std::fs::canonicalize(left), std::fs::canonicalize(right)),
            (Ok(left), Ok(right)) if left == right
        )
}

/// `with_extension` replaces a key's extension, so keys `a` and `a.x` in one
/// directory both mint `a.next-*` siblings. Such a namespace is never swept:
/// one key's leftovers cannot be told from the other's.
fn shares_sibling_namespace(key_path: &Path, custody: &[AuthorizedKeyRecord]) -> bool {
    let (Some(dir), Some(stem)) = (key_path.parent(), key_path.file_stem()) else {
        return true;
    };
    custody
        .iter()
        .map(|record| record.key_path.as_path())
        .filter(|other| *other != key_path)
        .any(|other| {
            other.file_stem() == Some(stem) && other.parent().is_some_and(|p| same_dir(p, dir))
        })
}

/// The key's directory, held open so listing and removal stay anchored to
/// the same inode. Managed key directories are owner-only.
fn open_key_dir(key_path: &Path) -> std::io::Result<thegn_core::fsperm::PrivateDir> {
    let parent = key_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "managed key path has no parent directory",
            )
        })?;
    thegn_core::fsperm::open_private_dir(parent)
}

/// Rotation siblings of `key_path` in `dir` that nothing references: names
/// matching [`is_rotation_sibling_name`], not named by any `referenced` path
/// (compared by file name, which is stricter than by full path), and — without
/// following links — a regular file owned by the current user.
fn orphan_siblings(
    dir: &thegn_core::fsperm::PrivateDir,
    key_path: &Path,
    referenced: &BTreeSet<PathBuf>,
) -> std::io::Result<Vec<PathBuf>> {
    let Some(stem) = key_path.file_stem().and_then(OsStr::to_str) else {
        return Ok(Vec::new());
    };
    let referenced_names = referenced
        .iter()
        .filter_map(|path| path.file_name())
        .collect::<BTreeSet<&OsStr>>();
    let mut orphans = Vec::new();
    for name in dir.file_names()? {
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if !is_rotation_sibling_name(stem, name_str) || referenced_names.contains(name.as_os_str())
        {
            continue;
        }
        let Some(metadata) = dir.metadata_nofollow(name_str)? else {
            continue;
        };
        if !metadata.file_type().is_file() || !thegn_core::fsperm::owned_by_current_user(&metadata)
        {
            continue;
        }
        orphans.push(dir.path().join(&name));
    }
    orphans.sort();
    Ok(orphans)
}

struct OrphanSweep {
    dir: thegn_core::fsperm::PrivateDir,
    orphans: Vec<PathBuf>,
}

/// The orphaned rotation files beside `key_path` given the complete custody
/// inventory, or `None` when the sibling namespace is shared and must not be
/// swept.
fn plan_orphan_sweep(
    key_path: &Path,
    custody: &[AuthorizedKeyRecord],
) -> std::io::Result<Option<OrphanSweep>> {
    if shares_sibling_namespace(key_path, custody) {
        tracing::debug!(
            key = %key_path.display(),
            "skip orphaned rotation key sweep: another managed key shares its sibling namespace",
        );
        return Ok(None);
    }
    let dir = open_key_dir(key_path)?;
    let orphans = orphan_siblings(&dir, key_path, &referenced_key_files(custody))?;
    Ok(Some(OrphanSweep { dir, orphans }))
}

/// Delete the orphaned rotation files beside `key_path` and return how many
/// were removed. The caller holds the scope lease and has settled recovery.
/// Every failure is logged, never returned: an orphan is never authorized, so
/// leaving one behind must not fail the rotation of the live key.
fn sweep_orphan_siblings(key_path: &Path, custody: &[AuthorizedKeyRecord]) -> usize {
    let sweep = match plan_orphan_sweep(key_path, custody) {
        Ok(Some(sweep)) => sweep,
        Ok(None) => return 0,
        Err(error) => {
            // best-effort: a key directory that cannot be listed safely only
            // keeps never-authorized leftovers; the rotation itself proceeds.
            tracing::warn!(
                key = %key_path.display(),
                %error,
                "could not scan for orphaned rotation key files",
            );
            return 0;
        }
    };
    if sweep.orphans.is_empty() {
        return 0;
    }
    let mut removed = 0usize;
    for path in &sweep.orphans {
        let Some(name) = path.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        match sweep.dir.remove_file(name) {
            Ok(true) => removed += 1,
            Ok(false) => {}
            // best-effort: the file is never authorized; the next rotation
            // retries the removal.
            Err(error) => tracing::warn!(
                path = %path.display(),
                %error,
                "could not remove orphaned rotation key file",
            ),
        }
    }
    tracing::info!(
        target: "thegn::secret::audit",
        consumer = "secret.ssh.rotate",
        key = %key_path.display(),
        count = removed,
        outcome = "orphans-swept",
        "orphaned managed SSH rotation key files removed",
    );
    removed
}

/// Re-read the complete custody inventory (ALL keys) and sweep `key_path`'s
/// orphaned rotation siblings. Must run under the scope lease.
fn sweep_orphans_under_lease(key_path: &Path) {
    match thegn_core::managed_ssh::list() {
        Ok(custody) => {
            sweep_orphan_siblings(key_path, &custody);
        }
        // best-effort: without the complete inventory no sibling can be proven
        // unreferenced, so nothing is deleted and the rotation proceeds.
        Err(error) => tracing::warn!(
            key = %key_path.display(),
            error = %format!("{error:#}"),
            "skip orphaned rotation key sweep: custody inventory unreadable",
        ),
    }
}

fn publish_if_unchanged_in(
    dir: &Path,
    expected: &AuthorizedKeyRecord,
    next: &AuthorizedKeyRecord,
) -> Result<AuthorizedKeyRecord> {
    thegn_core::managed_ssh::publish_if_unchanged_at(dir, expected, next)
}

fn ensure_unchanged(dir: &Path, expected: &AuthorizedKeyRecord) -> Result<()> {
    let actual = thegn_core::managed_ssh::read_at(
        dir,
        &expected.provider,
        &expected.account,
        &expected.instance,
    )?;
    if actual.as_ref() != Some(expected) {
        return Err(CustodyChanged {
            provider: expected.provider.clone(),
            instance: expected.instance.clone(),
        }
        .into());
    }
    Ok(())
}

fn update_recovery(record: &mut AuthorizedKeyRecord, recovery: RotationRecovery) -> Result<()> {
    let expected = record.clone();
    let mut next = expected.clone();
    next.rotation_recovery = Some(recovery);
    *record = publish_if_unchanged_in(&thegn_core::managed_ssh::dir(), &expected, &next)?;
    Ok(())
}

fn clear_recovery(record: &mut AuthorizedKeyRecord) -> Result<()> {
    let expected = record.clone();
    let mut next = expected.clone();
    next.rotation_recovery = None;
    *record = publish_if_unchanged_in(&thegn_core::managed_ssh::dir(), &expected, &next)?;
    Ok(())
}

fn persist_recovery_for_all(
    records: &mut [AuthorizedKeyRecord],
    recovery: &RotationRecovery,
) -> Result<()> {
    for record in records {
        update_recovery(record, recovery.clone())?;
    }
    Ok(())
}

fn clear_unstarted_recovery(records: &mut [AuthorizedKeyRecord], staging: &Path) -> Result<()> {
    let errors = records
        .iter_mut()
        .filter(|record| record.rotation_recovery.is_some())
        .filter_map(|record| clear_recovery(record).err())
        .map(|error| format!("{error:#}"))
        .collect::<Vec<_>>();
    let remove_error = remove_pair(staging).err();
    if errors.is_empty() && remove_error.is_none() {
        Ok(())
    } else {
        let mut details = errors;
        if let Some(error) = remove_error {
            details.push(format!("remove staged pair: {error:#}"));
        }
        bail!(
            "unstarted recovery cleanup was incomplete: {}",
            details.join("; ")
        )
    }
}

/// Finish or roll back any auxiliary key recorded by an interrupted rotation.
/// Fingerprints determine which side of local promotion completed; no secret or
/// public-key body is stored in the ledger.
fn recover_pending<R: RotationRemote>(
    remote: &R,
    key_path: &Path,
    records: &mut [AuthorizedKeyRecord],
) -> Result<()> {
    let pending = records
        .iter()
        .enumerate()
        .filter_map(|(index, record)| record.rotation_recovery.as_ref().map(|_| index))
        .collect::<Vec<_>>();
    let Some(&first_index) = pending.first() else {
        return Ok(());
    };
    for &index in &pending {
        ensure_unchanged(&thegn_core::managed_ssh::dir(), &records[index])?;
    }
    let recovery = records[first_index]
        .rotation_recovery
        .clone()
        .expect("pending index has recovery state");
    for &index in &pending {
        let other = records[index]
            .rotation_recovery
            .as_ref()
            .expect("pending index has recovery state");
        anyhow::ensure!(
            other.key_path == recovery.key_path
                && other.key_fingerprint == recovery.key_fingerprint
                && other.backup_key_path == recovery.backup_key_path,
            "managed SSH recovery rows disagree within custody scope {}",
            key_path.display()
        );
    }

    let canonical_public = std::fs::read_to_string(public_path(key_path)).with_context(|| {
        format!(
            "read canonical public key for recovery {}",
            key_path.display()
        )
    })?;
    let canonical_fingerprint = thegn_core::managed_ssh::key_fingerprint(canonical_public.trim());
    let primary_fingerprint = &records[first_index].key_fingerprint;
    if pending.iter().any(|&index| {
        records[index]
            .rotation_recovery
            .as_ref()
            .is_some_and(|state| state.retiring_completed)
    }) {
        anyhow::ensure!(
            canonical_fingerprint == recovery.key_fingerprint,
            "completed rotation recovery expected canonical fingerprint {}, found {} at {}",
            recovery.key_fingerprint,
            canonical_fingerprint,
            key_path.display()
        );
        // Spread the forward decision to every pending row before clearing
        // any: a cleared row no longer proves retirement completed, so a crash
        // between clears would otherwise leave the survivors (with
        // retiring_completed unset) to roll the scope back while the cleared
        // rows already name the replacement.
        for &index in &pending {
            let mut state = records[index]
                .rotation_recovery
                .clone()
                .expect("pending index has recovery state");
            if !state.retiring_completed {
                state.retiring_completed = true;
                update_recovery(&mut records[index], state)
                    .context("record completed remote retirement boundary during recovery")?;
            }
        }
        for &index in &pending {
            let expected = records[index].clone();
            thegn_core::managed_ssh::record_rotation_completed(
                &expected,
                key_path,
                &canonical_public,
            )?;
            records[index].key_path = key_path.to_path_buf();
            records[index].key_fingerprint = recovery.key_fingerprint.clone();
            records[index].authorized_at = thegn_core::util::now();
            records[index].rotation_recovery = None;
            remote.phase("recovery-completed-forward");
        }
        remove_pair(&recovery.key_path)?;
        if let Some(backup) = recovery.backup_key_path.as_deref() {
            remove_pair(backup)?;
        }
        return Ok(());
    }
    let (primary_key_path, auxiliary_key_path, primary_public, auxiliary_public, restore_local) =
        if canonical_fingerprint == *primary_fingerprint {
            let auxiliary_public = read_public_matching(
                &recovery.key_path,
                &recovery.key_fingerprint,
                "auxiliary recovery key",
            )?;
            (
                key_path.to_path_buf(),
                recovery.key_path.clone(),
                canonical_public,
                auxiliary_public,
                false,
            )
        } else if canonical_fingerprint == recovery.key_fingerprint {
            let backup = recovery.backup_key_path.clone().ok_or_else(|| {
                anyhow!(
                    "auxiliary key is canonical but recovery has no retiring-key path for {}",
                    key_path.display()
                )
            })?;
            let primary_public =
                read_public_matching(&backup, primary_fingerprint, "retiring primary key")?;
            (
                backup,
                key_path.to_path_buf(),
                primary_public,
                canonical_public,
                true,
            )
        } else {
            bail!(
                "canonical key fingerprint {} matches neither custody {} nor recovery {} for {}",
                canonical_fingerprint,
                primary_fingerprint,
                recovery.key_fingerprint,
                key_path.display()
            )
        };
    let primary_key_id = key_id(&primary_public)?;
    let auxiliary_key_id = key_id(&auxiliary_public)?;

    // Once remote retirement may have begun, first restore the primary key on
    // every affected instance using the auxiliary credential. This is
    // idempotent and prevents recovery from stranding an instance that lost the
    // primary just before the previous process exited.
    if pending.iter().any(|&index| {
        records[index]
            .rotation_recovery
            .as_ref()
            .is_some_and(|state| state.retiring_started)
    }) {
        for &index in &pending {
            let record = &records[index];
            let auxiliary = remote.connect(
                record,
                &auxiliary_key_path,
                &auxiliary_public,
                "recovery provider",
            )?;
            let restored = remote.run(
                &auxiliary,
                record,
                &authorize_script(&primary_public, &primary_key_id)?,
                "restore primary key during rotation recovery",
            );
            if let Err(error) = restored {
                // An interrupted earlier recovery may already have removed the
                // auxiliary key here after restoring the primary, so the
                // auxiliary can no longer authenticate. That state is safe only
                // when the primary itself proves it is still authorized.
                let primary = remote.connect(
                    record,
                    &primary_key_path,
                    &primary_public,
                    "recovery provider",
                )?;
                if let Err(probe_error) = remote.run(
                    &primary,
                    record,
                    &authorized_probe_script(&primary_key_id),
                    "probe primary key during rotation recovery",
                ) {
                    bail!(
                        "{error:#}; the primary key is not provably authorized either: {probe_error:#}"
                    );
                }
            }
            remote.phase("recovery-primary-restored");
        }
    }

    for &index in &pending {
        let record = &records[index];
        ensure_unchanged(&thegn_core::managed_ssh::dir(), record)?;
        let primary = remote.connect(
            record,
            &primary_key_path,
            &primary_public,
            "recovery provider",
        )?;
        remote.run(
            &primary,
            record,
            &deauthorize_script(&auxiliary_key_id)?,
            "remove auxiliary key during rotation recovery",
        )?;
        remote.phase("recovery-auxiliary-removed");
    }

    if restore_local {
        let backup = recovery
            .backup_key_path
            .as_deref()
            .expect("promoted recovery has a retiring path");
        restore_pair(key_path, &recovery.key_path, backup)?;
        remote.phase("recovery-local-restored");
    }
    for &index in &pending {
        clear_recovery(&mut records[index])?;
        remote.phase("recovery-cleared");
    }
    remove_pair(&recovery.key_path)?;
    if let Some(backup) = recovery.backup_key_path.as_deref() {
        remove_pair(backup)?;
    }
    Ok(())
}

fn read_public_matching(private: &Path, expected: &str, label: &str) -> Result<String> {
    let path = public_path(private);
    let public = std::fs::read_to_string(&path)
        .with_context(|| format!("read {label} {}", path.display()))?;
    let actual = thegn_core::managed_ssh::key_fingerprint(public.trim());
    anyhow::ensure!(
        actual == expected,
        "{label} fingerprint {actual} does not match recorded {expected} at {}",
        path.display()
    );
    Ok(public)
}

fn combine_recovery_error(
    operation: anyhow::Error,
    recovery: Result<()>,
    staging: &Path,
) -> anyhow::Error {
    match recovery {
        Ok(()) => operation.context("managed SSH rotation was fully rolled back"),
        Err(recovery_error) => anyhow!(
            "managed SSH rotation failed: {operation:#}; automatic recovery was incomplete: {recovery_error:#}. Value-free recovery custody remains recorded for {}",
            staging.display()
        ),
    }
}

fn provider_config<'a>(
    cfg: &'a Config,
    record: &AuthorizedKeyRecord,
    key_path: &Path,
) -> Result<&'a thegn_core::config::EnvProviderConfig> {
    let filename = key_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let provider_configs = cfg
        .env
        .values()
        .map(|env| &env.provider)
        .filter(|pc| pc.provider.trim().eq_ignore_ascii_case(&record.provider))
        .collect::<Vec<_>>();
    let exact = provider_configs.iter().copied().find(|pc| {
        let account = crate::provider_factory::managed_key_account(pc);
        crate::provider_factory::managed_custody_account(pc) == record.account
            || account == record.account
            || cfg
                .credentials
                .ssh
                .managed_key_scope
                .managed_key_basename(&pc.provider, &account)
                == filename
    });
    exact
        // A historic shared custody row derived its account label from the
        // basename (`shared`). Selecting by provider alone is safe only when
        // there is exactly one configured account; ambiguity must fail closed.
        .or_else(|| (provider_configs.len() == 1).then_some(provider_configs[0]))
        .ok_or_else(|| {
            anyhow!(
                "no unambiguous configured {} provider account owns managed key {} for instance {}",
                record.provider,
                key_path.display(),
                record.instance
            )
        })
}

/// Prove a Sprites replacement with a real local OpenSSH handshake through the
/// worktree-bound `sprite-proxy`, not the provider's WSS exec API. The argv
/// builder forces `IdentitiesOnly=yes` and disables ControlMaster reuse, so
/// success necessarily authenticates with `key`.
#[expect(clippy::disallowed_methods)]
fn run_sprite_checked(
    record: &AuthorizedKeyRecord,
    key: &Path,
    script: &str,
    operation: &str,
) -> Result<()> {
    let worktree = record.proxy_worktree.as_deref().ok_or_else(|| {
        anyhow!(
            "Sprites record {} has no originating worktree for SSH-over-WSS proof",
            record.instance
        )
    })?;
    let exe = thegn_core::util::self_exe_path()
        .ok_or_else(|| anyhow!("cannot resolve thegn executable for sprite-proxy"))?;
    let mut argv = crate::agent::sprite_ssh_argv_for_custody(
        &exe.to_string_lossy(),
        worktree,
        key,
        "sprite",
        "",
        crate::agent::SpriteSshCustody {
            provider: &record.provider,
            account: &record.account,
            instance: &record.instance,
        },
    );
    if argv.get(1).is_some_and(|flag| flag == "-tt") {
        argv[1] = "-T".to_string();
    }
    let remote = argv
        .last_mut()
        .ok_or_else(|| anyhow!("sprite SSH proof argv is empty"))?;
    *remote = script.to_string();
    let output = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .with_context(|| format!("spawn {operation} for {}", record.instance))?;
    if output.status.success() {
        return Ok(());
    }
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    bail!(
        "{operation} failed for {} (exit {}): {}",
        record.instance,
        output.status.code().unwrap_or(255),
        combined
            .lines()
            .rev()
            .take(3)
            .collect::<Vec<_>>()
            .join(" | ")
    )
}

fn run_checked(
    provider: &thegn_svc::provider::Provider,
    instance: &str,
    script: &str,
    operation: &str,
) -> Result<()> {
    let argv = vec!["sh".to_string(), "-lc".to_string(), script.to_string()];
    let (code, output) = crate::agent::block_on_provider(|| async {
        provider.run_exec(instance, &argv, None, &[]).await
    })?;
    if code == 0 {
        Ok(())
    } else {
        bail!(
            "{operation} failed for {instance} (exit {code}): {}",
            output.lines().rev().take(3).collect::<Vec<_>>().join(" | ")
        )
    }
}

pub(crate) fn key_id(public_key: &str) -> Result<(String, String)> {
    let mut fields = public_key.split_whitespace();
    let key_type = fields
        .next()
        .ok_or_else(|| anyhow!("public key is missing its type"))?;
    let body = fields
        .next()
        .ok_or_else(|| anyhow!("public key is missing its base64 body"))?;
    if !key_type
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || "@.-".contains(ch))
    {
        bail!("public key has an invalid key type");
    }
    if !body
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || "+/=".contains(ch))
    {
        bail!("public key has an invalid base64 body");
    }
    Ok((key_type.to_string(), body.to_string()))
}

fn awk_key_match(key_id: &(String, String), program: &str) -> String {
    let key_type = thegn_core::util::sh_quote(&key_id.0);
    let body = thegn_core::util::sh_quote(&key_id.1);
    format!(
        "awk -v key_type={key_type} -v key_body={body} 'function key_matches( i) {{ if ($0 ~ /^[ \\t]*#/) return 0; for (i = 1; i < NF; i++) if ($i ~ /^[a-z0-9@.-]+$/ && $(i + 1) ~ /^[A-Za-z0-9+\\/=]+$/) return ($i == key_type && $(i + 1) == key_body); return 0 }} {program}'"
    )
}

/// Authorize a public key and verify the published `authorized_keys` post-state.
pub(crate) fn authorize_script(public_key: &str, key_id: &(String, String)) -> Result<String> {
    let matcher = awk_key_match(
        key_id,
        "{ if (key_matches()) found = 1 } END { if (found) exit 0; exit 1 }",
    );
    let key = thegn_core::util::sh_quote(public_key.trim());
    Ok(format!(
        "set -eu\n\
         mkdir -p \"$HOME/.ssh\"\n\
         chmod 700 \"$HOME/.ssh\"\n\
         f=\"$HOME/.ssh/authorized_keys\"\n\
         if [ -L \"$f\" ]; then printf '%s\\n' 'TG-AUTHKEYS-ERROR:authorized_keys is not a regular file' >&2; exit 1; fi\n\
         if [ -e \"$f\" ] && [ ! -f \"$f\" ]; then printf '%s\\n' 'TG-AUTHKEYS-ERROR:authorized_keys is not a regular file' >&2; exit 1; fi\n\
         if [ ! -e \"$f\" ] && [ ! -L \"$f\" ]; then (umask 077; : > \"$f\"); fi\n\
         if [ -L \"$f\" ]; then printf '%s\\n' 'TG-AUTHKEYS-ERROR:authorized_keys is not a regular file' >&2; exit 1; fi\n\
         if [ ! -f \"$f\" ]; then printf '%s\\n' 'TG-AUTHKEYS-ERROR:authorized_keys is not a regular file' >&2; exit 1; fi\n\
         cat \"$f\" > /dev/null\n\
         chmod 600 \"$f\"\n\
         before=$(awk 'END {{ print NR }}' \"$f\")\n\
         if {matcher} \"$f\"; then\n\
             :\n\
         else\n\
             status=$?\n\
             if [ \"$status\" -ne 1 ]; then exit \"$status\"; fi\n\
             if [ -s \"$f\" ] && [ -n \"$(tail -c 1 \"$f\")\" ]; then printf '\\n' >> \"$f\"; fi\n\
             printf '%s\\n' {key} >> \"$f\"\n\
             after=$(awk 'END {{ print NR }}' \"$f\")\n\
             if [ \"$after\" -ne $((before + 1)) ]; then printf 'TG-AUTHKEYS-ERROR:line count mismatch before=%s after=%s\\n' \"$before\" \"$after\" >&2; exit 1; fi\n\
         fi\n\
         if {matcher} \"$f\"; then :; else status=$?; if [ \"$status\" -ne 1 ]; then exit \"$status\"; fi; printf '%s\\n' 'TG-AUTHKEYS-ERROR:key missing after authorization' >&2; exit 1; fi"
    ))
}

/// Probe whether the decoded managed key is authorized: exit 0 present, 1 absent, 2 when
/// authorized_keys is a symlink, missing or not a regular file. Comment lines never count;
/// option-prefixed lines do.
pub(crate) fn authorized_probe_script(key_id: &(String, String)) -> String {
    let probe = awk_key_match(
        key_id,
        "{ if (key_matches()) found = 1 } END { if (found) exit 0; exit 1 }",
    );
    format!(
        "set -eu\n\
         f=\"$HOME/.ssh/authorized_keys\"\n\
         if [ -L \"$f\" ] || [ ! -f \"$f\" ]; then printf '%s\\n' 'TG-AUTHKEYS-ERROR:authorized_keys is not a regular file' >&2; exit 2; fi\n\
         {probe} \"$f\""
    )
}

/// Deauthorize a key and verify the published `authorized_keys` post-state.
fn deauthorize_script(key_id: &(String, String)) -> Result<String> {
    let filter = awk_key_match(key_id, "{ if (!key_matches()) print $0 }");
    let count_matches = awk_key_match(key_id, "{ if (key_matches()) n++ } END { print n + 0 }");
    let probe = awk_key_match(
        key_id,
        "{ if (key_matches()) found = 1 } END { if (found) exit 0; exit 1 }",
    );
    Ok(format!(
        "set -eu\n\
         f=\"$HOME/.ssh/authorized_keys\"\n\
         if [ -d \"$HOME/.ssh\" ] && [ ! -x \"$HOME/.ssh\" ]; then printf '%s\\n' 'TG-AUTHKEYS-ERROR:.ssh directory is not searchable' >&2; exit 1; fi\n\
         if [ ! -e \"$f\" ] && [ ! -L \"$f\" ]; then exit 0; fi\n\
         if [ -L \"$f\" ]; then printf '%s\\n' 'TG-AUTHKEYS-ERROR:authorized_keys is a symlink' >&2; exit 1; fi\n\
         if [ ! -f \"$f\" ]; then printf '%s\\n' 'TG-AUTHKEYS-ERROR:authorized_keys is not a regular file' >&2; exit 1; fi\n\
         cat \"$f\" > /dev/null\n\
         t=$(mktemp \"$f.thegn.XXXXXX\")\n\
         trap 'rm -f \"$t\"' EXIT\n\
         total=$(awk 'END {{ print NR }}' \"$f\")\n\
         removed=$({count_matches} \"$f\")\n\
         {filter} \"$f\" > \"$t\"\n\
         kept=$((total - removed))\n\
         actual=$(awk 'END {{ print NR }}' \"$t\")\n\
         if [ \"$actual\" -ne \"$kept\" ]; then printf 'TG-AUTHKEYS-ERROR:line count mismatch total=%s removed=%s kept=%s actual=%s\\n' \"$total\" \"$removed\" \"$kept\" \"$actual\" >&2; exit 1; fi\n\
         if {probe} \"$t\"; then printf '%s\\n' 'TG-AUTHKEYS-ERROR:target key remains after removal' >&2; exit 1; else status=$?; if [ \"$status\" -ne 1 ]; then exit \"$status\"; fi; fi\n\
         if [ \"$removed\" -eq 0 ]; then exit 0; fi\n\
         chmod 600 \"$t\"\n\
         mv -f \"$t\" \"$f\"\n\
         published=$(awk 'END {{ print NR }}' \"$f\")\n\
         if [ \"$published\" -ne \"$kept\" ]; then printf 'TG-AUTHKEYS-ERROR:published line count mismatch kept=%s published=%s\\n' \"$kept\" \"$published\" >&2; exit 1; fi\n\
         if {probe} \"$f\"; then printf '%s\\n' 'TG-AUTHKEYS-ERROR:target key remains after publication' >&2; exit 1; else status=$?; if [ \"$status\" -ne 1 ]; then exit \"$status\"; fi; fi\n\
         trap - EXIT"
    ))
}

fn public_path(private: &Path) -> PathBuf {
    PathBuf::from(format!("{}.pub", private.display()))
}

/// Run a rollback attempt for every instance even after one fails. Returning
/// every indexed error lets the caller decide whether reverting local custody
/// is safe and report the exact incomplete repairs.
fn attempt_all(
    count: usize,
    mut attempt: impl FnMut(usize) -> Result<()>,
) -> Vec<(usize, anyhow::Error)> {
    (0..count)
        .filter_map(|index| attempt(index).err().map(|error| (index, error)))
        .collect()
}

fn promote_pair(current: &Path, staging: &Path, backup: &Path) -> Result<()> {
    let current_public = public_path(current);
    let staging_public = public_path(staging);
    let backup_public = public_path(backup);
    std::fs::rename(current, backup)?;
    if let Err(error) = std::fs::rename(&current_public, &backup_public) {
        return Err(promotion_error(
            error,
            rollback_renames(&[(backup, current)]),
        ));
    }
    if let Err(error) = std::fs::rename(staging, current) {
        return Err(promotion_error(
            error,
            rollback_renames(&[(&backup_public, &current_public), (backup, current)]),
        ));
    }
    if let Err(error) = std::fs::rename(&staging_public, &current_public) {
        return Err(promotion_error(
            error,
            rollback_renames(&[
                (current, staging),
                (&backup_public, &current_public),
                (backup, current),
            ]),
        ));
    }
    Ok(())
}

fn rollback_renames(steps: &[(&Path, &Path)]) -> Vec<String> {
    steps
        .iter()
        .filter_map(|(from, to)| {
            std::fs::rename(from, to)
                .err()
                .map(|error| format!("{} -> {}: {error}", from.display(), to.display()))
        })
        .collect()
}

fn promotion_error(error: std::io::Error, rollback_errors: Vec<String>) -> anyhow::Error {
    if rollback_errors.is_empty() {
        error.into()
    } else {
        anyhow!(
            "managed SSH key promotion failed: {error}; rollback was incomplete: {}",
            rollback_errors.join("; ")
        )
    }
}

fn restore_pair(current: &Path, staging: &Path, backup: &Path) -> Result<()> {
    restore_pair_with(current, staging, backup, |from, to| {
        std::fs::rename(from, to)
    })
}

fn restore_pair_with(
    current: &Path,
    staging: &Path,
    backup: &Path,
    mut rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
) -> Result<()> {
    let current_public = public_path(current);
    let staging_public = public_path(staging);
    let backup_public = public_path(backup);
    rename(current, staging)?;
    if let Err(error) = rename(&current_public, &staging_public) {
        return Err(restoration_error(
            error,
            rollback_renames_with(&[(staging, current)], &mut rename),
        ));
    }
    if let Err(error) = rename(backup, current) {
        return Err(restoration_error(
            error,
            rollback_renames_with(
                &[(&staging_public, &current_public), (staging, current)],
                &mut rename,
            ),
        ));
    }
    if let Err(error) = rename(&backup_public, &current_public) {
        return Err(restoration_error(
            error,
            rollback_renames_with(
                &[
                    (current, backup),
                    (&staging_public, &current_public),
                    (staging, current),
                ],
                &mut rename,
            ),
        ));
    }
    Ok(())
}

fn rollback_renames_with(
    steps: &[(&Path, &Path)],
    rename: &mut impl FnMut(&Path, &Path) -> std::io::Result<()>,
) -> Vec<String> {
    steps
        .iter()
        .filter_map(|(from, to)| {
            rename(from, to)
                .err()
                .map(|error| format!("{} -> {}: {error}", from.display(), to.display()))
        })
        .collect()
}

fn restoration_error(error: std::io::Error, rollback_errors: Vec<String>) -> anyhow::Error {
    if rollback_errors.is_empty() {
        error.into()
    } else {
        anyhow!(
            "managed SSH key restore failed: {error}; rollback was incomplete: {}",
            rollback_errors.join("; ")
        )
    }
}

fn remove_pair(private: &Path) -> Result<()> {
    for path in [private.to_path_buf(), public_path(private)] {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "managed_ssh_rotation_fault_tests.rs"]
mod fault_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn custody_record(instance: &str, recovery: Option<RotationRecovery>) -> AuthorizedKeyRecord {
        AuthorizedKeyRecord {
            provider: "test-provider".to_string(),
            account: "test-account".to_string(),
            instance: instance.to_string(),
            key_path: PathBuf::from("/keys/managed"),
            key_fingerprint: "SHA256:test".to_string(),
            authorized_at: 1,
            generation: 0,
            proxy_worktree: None,
            rotation_recovery: recovery,
        }
    }

    fn recovery_marker(marker: &str) -> RotationRecovery {
        RotationRecovery {
            key_path: PathBuf::from(format!("/keys/{marker}")),
            key_fingerprint: format!("SHA256:{marker}"),
            backup_key_path: None,
            authorized: false,
            verified: false,
            retiring_started: false,
            retiring_completed: false,
        }
    }

    #[test]
    fn second_lease_on_a_scope_is_refused_and_released_on_drop() {
        let temp = tempfile::tempdir().unwrap();
        let lock_dir = temp.path().join("rotation-leases");
        let key_path = temp.path().join("managed-key");
        let first = acquire_scope_lease_in(&lock_dir, &key_path).unwrap();
        let second = acquire_scope_lease_in(&lock_dir, &key_path);
        let refusal = match second {
            Err(error) => error,
            Ok(_) => panic!("a second lease for the same key scope must be refused"),
        };
        assert!(refusal.downcast_ref::<RotationLeaseHeld>().is_some());
        assert!(refusal.to_string().contains("managed-key"));
        let independent = acquire_scope_lease_in(&lock_dir, &temp.path().join("other-key"));
        assert!(independent.is_ok());
        drop(first);
        assert!(acquire_scope_lease_in(&lock_dir, &key_path).is_ok());
    }

    #[test]
    fn lease_dir_symlink_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let victim = temp.path().join("victim");
        std::fs::create_dir(&victim).unwrap();
        let link = temp.path().join("link");
        if thegn_core::fsperm::create_symlink(&victim, &link, true).is_err() {
            return;
        }
        assert!(acquire_scope_lease_in(&link, Path::new("managed-key")).is_err());
        assert_eq!(std::fs::read_dir(&victim).unwrap().count(), 0);
    }

    #[test]
    fn trusted_anchor_symlink_is_allowed() {
        let temp = tempfile::tempdir().unwrap();
        let real_anchor = temp.path().join("real-anchor");
        std::fs::create_dir(&real_anchor).unwrap();
        let anchor = temp.path().join("anchor");
        if thegn_core::fsperm::create_symlink(&real_anchor, &anchor, true).is_err() {
            return;
        }
        let lock_dir = anchor.join("rotation-leases");
        assert!(acquire_scope_lease_in(&lock_dir, Path::new("managed-key")).is_ok());
    }

    #[test]
    fn pre_existing_lease_dir_is_tightened_to_owner_only() {
        let temp = tempfile::tempdir().unwrap();
        let lock_dir = temp.path().join("rotation-leases");
        std::fs::create_dir(&lock_dir).unwrap();
        if thegn_core::fsperm::set_mode_for_test(&lock_dir, 0o755).is_err() {
            return;
        }
        let _lease = acquire_scope_lease_in(&lock_dir, Path::new("managed-key")).unwrap();
        assert_eq!(
            thegn_core::fsperm::mode_bits(&lock_dir).unwrap(),
            Some(0o700)
        );
    }

    #[test]
    fn stale_recovery_write_is_refused_and_leaves_the_row() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("authorized.d");
        let a = custody_record("instance-a", Some(recovery_marker("a")));
        let b = custody_record("instance-a", Some(recovery_marker("b")));
        let c = custody_record("instance-a", Some(recovery_marker("c")));
        let a = thegn_core::managed_ssh::record_authorized_at(&dir, &a).unwrap();
        let b = thegn_core::managed_ssh::record_authorized_at(&dir, &b).unwrap();

        let result = publish_if_unchanged_in(&dir, &a, &c);
        assert!(matches!(result, Err(error) if error.downcast_ref::<CustodyChanged>().is_some()));
        assert_eq!(
            thegn_core::managed_ssh::read_at(&dir, &b.provider, &b.account, &b.instance).unwrap(),
            Some(b)
        );
    }

    #[test]
    fn unchanged_row_is_published() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("authorized.d");
        let a = custody_record("instance-a", Some(recovery_marker("a")));
        let c = custody_record("instance-a", Some(recovery_marker("c")));
        let a = thegn_core::managed_ssh::record_authorized_at(&dir, &a).unwrap();
        let c = publish_if_unchanged_in(&dir, &a, &c).unwrap();
        assert_eq!(
            thegn_core::managed_ssh::read_at(&dir, &c.provider, &c.account, &c.instance).unwrap(),
            Some(c)
        );
    }

    #[test]
    fn consecutive_recovery_writes_chain_on_the_stamped_row() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("authorized.d");
        let a = thegn_core::managed_ssh::record_authorized_at(
            &dir,
            &custody_record("instance-a", None),
        )
        .unwrap();
        let b = publish_if_unchanged_in(
            &dir,
            &a,
            &AuthorizedKeyRecord {
                rotation_recovery: Some(recovery_marker("b")),
                ..a.clone()
            },
        )
        .unwrap();
        let c = publish_if_unchanged_in(
            &dir,
            &b,
            &AuthorizedKeyRecord {
                rotation_recovery: None,
                ..b.clone()
            },
        )
        .unwrap();
        assert!(c.generation > b.generation && b.generation > a.generation);

        let stale = publish_if_unchanged_in(
            &dir,
            &b,
            &AuthorizedKeyRecord {
                rotation_recovery: Some(recovery_marker("stale")),
                ..b.clone()
            },
        );
        assert!(matches!(stale, Err(error) if error.downcast_ref::<CustodyChanged>().is_some()));
        assert_eq!(
            thegn_core::managed_ssh::read_at(&dir, &c.provider, &c.account, &c.instance).unwrap(),
            Some(c)
        );
    }

    #[test]
    fn update_and_clear_recovery_keep_the_stamped_row_for_the_next_cas() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let dir = thegn_core::managed_ssh::dir();
        let mut record = thegn_core::managed_ssh::record_authorized_at(
            &dir,
            &custody_record("instance-a", None),
        )
        .unwrap();
        let initial_generation = record.generation;

        update_recovery(&mut record, recovery_marker("pending")).unwrap();
        let updated = thegn_core::managed_ssh::read_at(
            &dir,
            &record.provider,
            &record.account,
            &record.instance,
        )
        .unwrap()
        .unwrap();
        assert_eq!(record, updated);
        assert!(record.generation > initial_generation);
        assert!(record.rotation_recovery.is_some());

        let updated_generation = record.generation;
        clear_recovery(&mut record).unwrap();
        let cleared = thegn_core::managed_ssh::read_at(
            &dir,
            &record.provider,
            &record.account,
            &record.instance,
        )
        .unwrap()
        .unwrap();
        assert_eq!(record, cleared);
        assert!(record.generation > updated_generation);
        assert!(record.rotation_recovery.is_none());
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "test runs the generated remote script with a local sh"
    )]
    fn run_local_sh(home: &Path, script: &str) -> Option<std::process::Output> {
        if !Path::new("/bin/sh").exists() {
            return None;
        }
        Some(
            std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(script)
                .env("HOME", home)
                .output()
                .unwrap(),
        )
    }

    #[test]
    fn deauthorize_keeps_unrelated_keys_options_and_comments() {
        let temp = tempfile::tempdir().unwrap();
        let ssh = temp.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        let path = ssh.join("authorized_keys");
        let contents = "# operator comment\nssh-ed25519 AAAAOTHER unrelated\nfrom=\"10.0.0.1\",command=\"echo hi\" ssh-ed25519 AAAAOPTIONS option-comment\nssh-ed25519 AAAATARGET old-comment\n";
        std::fs::write(&path, contents).unwrap();
        let public = "ssh-ed25519 AAAATARGET rotation@test";
        let id = key_id(public).unwrap();
        let script = deauthorize_script(&id).unwrap();
        let Some(output) = run_local_sh(temp.path(), &script) else {
            return;
        };
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "# operator comment\nssh-ed25519 AAAAOTHER unrelated\nfrom=\"10.0.0.1\",command=\"echo hi\" ssh-ed25519 AAAAOPTIONS option-comment\n"
        );
    }

    #[test]
    fn deauthorize_fails_closed_on_unreadable_file() {
        let temp = tempfile::tempdir().unwrap();
        let ssh = temp.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        let path = ssh.join("authorized_keys");
        let contents = "ssh-ed25519 AAAAOTHER unrelated\nssh-ed25519 AAAATARGET old-comment\n";
        std::fs::write(&path, contents).unwrap();
        let public = "ssh-ed25519 AAAATARGET rotation@test";
        let id = key_id(public).unwrap();
        let script = deauthorize_script(&id).unwrap();
        let Some(set_mode) = run_local_sh(temp.path(), "chmod 640 \"$HOME/.ssh/authorized_keys\"")
        else {
            return;
        };
        assert!(set_mode.status.success());
        let Some(chmod_output) =
            run_local_sh(temp.path(), "chmod 000 \"$HOME/.ssh/authorized_keys\"")
        else {
            return;
        };
        assert!(chmod_output.status.success());
        if std::fs::read(&path).is_ok() {
            let restore = run_local_sh(temp.path(), "chmod 640 \"$HOME/.ssh/authorized_keys\"");
            assert!(restore.is_some_and(|result| result.status.success()));
            return;
        }
        let Some(output) = run_local_sh(temp.path(), &script) else {
            return;
        };
        assert!(!output.status.success());
        let mode = run_local_sh(
            temp.path(),
            "stat -c %a \"$HOME/.ssh/authorized_keys\" 2>/dev/null || stat -f %Lp \"$HOME/.ssh/authorized_keys\"",
        )
        .expect("local sh was available");
        assert_eq!(String::from_utf8_lossy(&mode.stdout).trim(), "0");
        let restore = run_local_sh(temp.path(), "chmod 640 \"$HOME/.ssh/authorized_keys\"")
            .expect("local sh was available");
        assert!(restore.status.success());
        assert_eq!(std::fs::read_to_string(path).unwrap(), contents);
        let restored_mode = run_local_sh(
            temp.path(),
            "stat -c %a \"$HOME/.ssh/authorized_keys\" 2>/dev/null || stat -f %Lp \"$HOME/.ssh/authorized_keys\"",
        )
        .expect("local sh was available");
        assert_eq!(String::from_utf8_lossy(&restored_mode.stdout).trim(), "640");
    }

    #[test]
    fn deauthorize_without_the_key_is_a_noop_success() {
        let temp = tempfile::tempdir().unwrap();
        let ssh = temp.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        let path = ssh.join("authorized_keys");
        let contents = "# comment\nssh-ed25519 AAAAOTHER unrelated\n";
        std::fs::write(&path, contents).unwrap();
        let public = "ssh-ed25519 AAAATARGET rotation@test";
        let script = deauthorize_script(&key_id(public).unwrap()).unwrap();
        let Some(output) = run_local_sh(temp.path(), &script) else {
            return;
        };
        assert!(output.status.success());
        assert_eq!(std::fs::read_to_string(path).unwrap(), contents);
    }

    #[test]
    fn deauthorize_with_no_file_succeeds() {
        let temp = tempfile::tempdir().unwrap();
        let public = "ssh-ed25519 AAAATARGET rotation@test";
        let script = deauthorize_script(&key_id(public).unwrap()).unwrap();
        let Some(output) = run_local_sh(temp.path(), &script) else {
            return;
        };
        assert!(output.status.success());
        assert!(!temp.path().join(".ssh/authorized_keys").exists());
    }

    #[test]
    fn authorize_does_not_duplicate_a_key_under_another_comment() {
        let temp = tempfile::tempdir().unwrap();
        let ssh = temp.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        let path = ssh.join("authorized_keys");
        std::fs::write(&path, "ssh-ed25519 AAAATARGET existing-comment\n").unwrap();
        let public = "ssh-ed25519 AAAATARGET new-comment";
        let script = authorize_script(public, &key_id(public).unwrap()).unwrap();
        let Some(output) = run_local_sh(temp.path(), &script) else {
            return;
        };
        assert!(output.status.success());
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "ssh-ed25519 AAAATARGET existing-comment\n"
        );
    }

    #[test]
    fn authorize_ignores_target_pair_in_an_unrelated_comment() {
        let temp = tempfile::tempdir().unwrap();
        let ssh = temp.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        let path = ssh.join("authorized_keys");
        let contents = "ssh-ed25519 AAAAOTHER note ssh-ed25519 AAAATARGET\n";
        std::fs::write(&path, contents).unwrap();
        let public = "ssh-ed25519 AAAATARGET rotation@test";
        let script = authorize_script(public, &key_id(public).unwrap()).unwrap();
        let Some(output) = run_local_sh(temp.path(), &script) else {
            return;
        };
        assert!(output.status.success());
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            format!("{contents}{public}\n")
        );
    }

    #[test]
    fn authorize_appends_key_when_only_a_commented_copy_exists() {
        let temp = tempfile::tempdir().unwrap();
        let ssh = temp.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        let path = ssh.join("authorized_keys");
        std::fs::write(&path, "# ssh-ed25519 AAAATARGET old-comment\n").unwrap();
        let public = "ssh-ed25519 AAAATARGET new-comment";
        let script = authorize_script(public, &key_id(public).unwrap()).unwrap();
        let Some(output) = run_local_sh(temp.path(), &script) else {
            return;
        };
        assert!(output.status.success());
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "# ssh-ed25519 AAAATARGET old-comment\nssh-ed25519 AAAATARGET new-comment\n"
        );
    }

    #[test]
    fn authorize_refuses_symlink_without_creating_its_target() {
        let temp = tempfile::tempdir().unwrap();
        let ssh = temp.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        let target = temp.path().join("target");
        let Some(link_output) = run_local_sh(
            temp.path(),
            "ln -s \"$HOME/target\" \"$HOME/.ssh/authorized_keys\"",
        ) else {
            return;
        };
        assert!(link_output.status.success());
        let public = "ssh-ed25519 AAAATARGET new-comment";
        let script = authorize_script(public, &key_id(public).unwrap()).unwrap();
        let Some(output) = run_local_sh(temp.path(), &script) else {
            return;
        };
        assert!(!output.status.success());
        assert!(!target.exists());
    }

    #[test]
    fn authorized_probe_ignores_a_commented_copy() {
        let temp = tempfile::tempdir().unwrap();
        let ssh = temp.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        let path = ssh.join("authorized_keys");
        let public = "ssh-ed25519 AAAATARGET new-comment";
        std::fs::write(&path, "# ssh-ed25519 AAAATARGET old-comment\n").unwrap();
        let key = key_id(public).unwrap();
        let script = authorized_probe_script(&key);
        let Some(output) = run_local_sh(temp.path(), &script) else {
            return;
        };
        assert_eq!(output.status.code(), Some(1));

        std::fs::write(&path, "# ssh-ed25519 AAAATARGET new-comment\n").unwrap();
        let Some(output) = run_local_sh(temp.path(), &script) else {
            return;
        };
        assert_eq!(output.status.code(), Some(1));
    }

    #[test]
    fn authorized_probe_finds_the_key_under_options_and_another_comment() {
        let temp = tempfile::tempdir().unwrap();
        let ssh = temp.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        std::fs::write(
            ssh.join("authorized_keys"),
            "from=\"10.0.0.1\" ssh-ed25519 AAAATARGET other\n",
        )
        .unwrap();
        let public = "ssh-ed25519 AAAATARGET new-comment";
        let script = authorized_probe_script(&key_id(public).unwrap());
        let Some(output) = run_local_sh(temp.path(), &script) else {
            return;
        };
        assert!(
            output.status.success(),
            "option-prefixed authorized key with another comment is present"
        );
    }

    #[test]
    fn authorized_probe_refuses_a_symlinked_file() {
        let temp = tempfile::tempdir().unwrap();
        let ssh = temp.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        std::fs::write(
            temp.path().join("target"),
            "ssh-ed25519 AAAATARGET new-comment\n",
        )
        .unwrap();
        let Some(link_output) = run_local_sh(
            temp.path(),
            "ln -s \"$HOME/target\" \"$HOME/.ssh/authorized_keys\"",
        ) else {
            return;
        };
        assert!(link_output.status.success());
        let public = "ssh-ed25519 AAAATARGET new-comment";
        let script = authorized_probe_script(&key_id(public).unwrap());
        let Some(output) = run_local_sh(temp.path(), &script) else {
            return;
        };
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains("TG-AUTHKEYS-ERROR"));
    }

    #[test]
    fn deauthorize_refuses_directory_at_authorized_keys() {
        let temp = tempfile::tempdir().unwrap();
        let ssh = temp.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        let path = ssh.join("authorized_keys");
        std::fs::create_dir(&path).unwrap();
        let public = "ssh-ed25519 AAAATARGET rotation@test";
        let script = deauthorize_script(&key_id(public).unwrap()).unwrap();
        let Some(output) = run_local_sh(temp.path(), &script) else {
            return;
        };
        assert!(!output.status.success());
        assert!(path.is_dir());
    }

    #[test]
    fn deauthorize_fails_when_ssh_directory_is_not_searchable() {
        let temp = tempfile::tempdir().unwrap();
        let ssh = temp.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        let path = ssh.join("authorized_keys");
        let contents = "ssh-ed25519 AAAATARGET rotation@test\n";
        std::fs::write(&path, contents).unwrap();
        let public = "ssh-ed25519 AAAATARGET rotation@test";
        let script = deauthorize_script(&key_id(public).unwrap()).unwrap();
        let Some(chmod_output) = run_local_sh(temp.path(), "chmod 600 \"$HOME/.ssh\"") else {
            return;
        };
        assert!(chmod_output.status.success());
        let Some(check_output) = run_local_sh(temp.path(), "[ ! -x \"$HOME/.ssh\" ]") else {
            return;
        };
        if !check_output.status.success() {
            let restore = run_local_sh(temp.path(), "chmod 700 \"$HOME/.ssh\"");
            assert!(restore.is_some_and(|output| output.status.success()));
            return;
        }
        let Some(output) = run_local_sh(temp.path(), &script) else {
            return;
        };
        let restore = run_local_sh(temp.path(), "chmod 700 \"$HOME/.ssh\"");
        assert!(restore.is_some_and(|result| result.status.success()));
        assert!(!output.status.success());
        assert_eq!(std::fs::read_to_string(path).unwrap(), contents);
    }

    #[test]
    fn authorize_repairs_a_missing_trailing_newline_before_appending() {
        let temp = tempfile::tempdir().unwrap();
        let ssh = temp.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        let path = ssh.join("authorized_keys");
        std::fs::write(&path, "ssh-ed25519 AAAAOTHER unrelated").unwrap();
        let public = "ssh-ed25519 AAAATARGET new-comment";
        let id = key_id(public).unwrap();
        let script = authorize_script(public, &id).unwrap();
        let Some(output) = run_local_sh(temp.path(), &script) else {
            return;
        };
        assert!(output.status.success());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "ssh-ed25519 AAAAOTHER unrelated\nssh-ed25519 AAAATARGET new-comment\n"
        );
        let probe = authorized_probe_script(&id);
        let Some(probe_output) = run_local_sh(temp.path(), &probe) else {
            return;
        };
        assert!(probe_output.status.success());
    }

    #[test]
    fn authorize_fails_when_the_append_did_not_publish_the_key() {
        let temp = tempfile::tempdir().unwrap();
        let ssh = temp.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        let path = ssh.join("authorized_keys");
        let contents = "ssh-ed25519 AAAAOTHER unrelated\n";
        std::fs::write(&path, contents).unwrap();
        let public = "ssh-ed25519 AAAATARGET new-comment";
        let script = authorize_script(public, &key_id(public).unwrap()).unwrap();
        let script = format!("printf() {{ return 0; }}\n{script}");
        let Some(output) = run_local_sh(temp.path(), &script) else {
            return;
        };
        assert!(!output.status.success());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
    }

    #[test]
    fn deauthorize_fails_when_the_publish_did_not_remove_the_key() {
        let temp = tempfile::tempdir().unwrap();
        let ssh = temp.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        let path = ssh.join("authorized_keys");
        let contents = "ssh-ed25519 AAAAOTHER unrelated\nssh-ed25519 AAAATARGET old\n";
        std::fs::write(&path, contents).unwrap();
        let id = key_id("ssh-ed25519 AAAATARGET x").unwrap();
        let script = deauthorize_script(&id).unwrap();
        let script = format!("mv() {{ return 0; }}\n{script}");
        let Some(output) = run_local_sh(temp.path(), &script) else {
            return;
        };
        assert!(!output.status.success());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
        let entries = std::fs::read_dir(&ssh)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].as_os_str(),
            std::ffi::OsStr::new("authorized_keys")
        );
    }

    #[test]
    fn invalid_public_keys_are_refused() {
        assert!(key_id("ssh-ed25519").is_err());
        assert!(key_id("ssh-ed25519' AAAA").is_err());
        assert!(key_id("ssh-ed25519 AAAA$(touch)").is_err());
    }

    #[test]
    fn remote_scripts_do_not_embed_private_material_and_are_idempotent() {
        let public = "ssh-ed25519 AAAATEST rotation@test";
        let id = key_id(public).unwrap();
        let add = authorize_script(public, &id).unwrap();
        let remove = deauthorize_script(&id).unwrap();
        assert!(add.contains(public));
        assert!(remove.contains("AAAATEST"));
        assert!(add.contains("key_matches"));
        assert!(remove.contains("mktemp"));
        assert!(!remove.contains("||"));
        assert!(!add.contains("PRIVATE"));
        assert!(!remove.contains("PRIVATE"));
    }

    #[test]
    fn pair_promotion_can_be_rolled_back_without_losing_either_pair() {
        let temp = tempfile::tempdir().unwrap();
        let current = temp.path().join("key");
        let staging = temp.path().join("key.next");
        std::fs::write(&current, "old-private").unwrap();
        std::fs::write(public_path(&current), "old-public").unwrap();
        std::fs::write(&staging, "new-private").unwrap();
        std::fs::write(public_path(&staging), "new-public").unwrap();
        let backup = temp.path().join("key.retiring");
        promote_pair(&current, &staging, &backup).unwrap();
        assert_eq!(std::fs::read_to_string(&current).unwrap(), "new-private");
        restore_pair(&current, &staging, &backup).unwrap();
        assert_eq!(std::fs::read_to_string(&current).unwrap(), "old-private");
        assert_eq!(std::fs::read_to_string(&staging).unwrap(), "new-private");
    }

    #[test]
    fn restore_failure_at_each_rename_preserves_the_promoted_pair_and_backup() {
        for fail_at in 1..=4 {
            let temp = tempfile::tempdir().unwrap();
            let current = temp.path().join("key");
            let staging = temp.path().join("key.next");
            let backup = temp.path().join("key.retiring");
            std::fs::write(&current, "new-private").unwrap();
            std::fs::write(public_path(&current), "new-public").unwrap();
            std::fs::write(&backup, "old-private").unwrap();
            std::fs::write(public_path(&backup), "old-public").unwrap();

            let mut step = 0;
            let result = restore_pair_with(&current, &staging, &backup, |from, to| {
                step += 1;
                if step == fail_at {
                    Err(std::io::Error::other("injected restore failure"))
                } else {
                    std::fs::rename(from, to)
                }
            });
            assert!(result.is_err(), "rename {fail_at} should fail");
            assert_eq!(
                std::fs::read_to_string(&current).unwrap(),
                "new-private",
                "rename {fail_at} lost canonical private key"
            );
            assert_eq!(
                std::fs::read_to_string(public_path(&current)).unwrap(),
                "new-public",
                "rename {fail_at} lost canonical public key"
            );
            assert_eq!(
                std::fs::read_to_string(&backup).unwrap(),
                "old-private",
                "rename {fail_at} lost backup private key"
            );
            assert_eq!(
                std::fs::read_to_string(public_path(&backup)).unwrap(),
                "old-public",
                "rename {fail_at} lost backup public key"
            );
            assert!(!staging.exists(), "rename {fail_at} left partial staging");
            assert!(
                !public_path(&staging).exists(),
                "rename {fail_at} left partial staging public key"
            );
        }
    }

    #[test]
    fn rollback_attempts_every_instance_and_reports_all_failures() {
        let mut visited = Vec::new();
        let failures = attempt_all(4, |index| {
            visited.push(index);
            if index == 1 || index == 3 {
                bail!("restore failed")
            }
            Ok(())
        });
        assert_eq!(visited, vec![0, 1, 2, 3]);
        assert_eq!(
            failures.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
            vec![1, 3]
        );
    }

    #[test]
    fn sprite_proof_argv_forces_the_staged_identity_without_a_control_master() {
        let argv = crate::agent::sprite_ssh_argv_for_custody(
            "/usr/bin/thegn",
            "/work/project",
            Path::new("/state/key.next"),
            "sprite",
            "",
            crate::agent::SpriteSshCustody {
                provider: "sprites",
                account: "sprites-work-acde0123",
                instance: "sprite-123",
            },
        );
        let joined = argv.join(" ");
        assert!(joined.contains("-i /state/key.next"), "{joined}");
        assert!(joined.contains("IdentitiesOnly=yes"), "{joined}");
        assert!(joined.contains("ControlMaster=no"), "{joined}");
        assert!(joined.contains("ControlPath=none"), "{joined}");
        assert!(joined.contains("--expected-provider sprites"), "{joined}");
        assert!(
            joined.contains("--expected-account sprites-work-acde0123"),
            "{joined}"
        );
        assert!(
            joined.contains("--expected-instance sprite-123"),
            "{joined}"
        );
    }

    const ID: &str = "0123456789abcdef";

    /// A private key directory (tempdirs are owner-only) holding the
    /// canonical managed pair `managed_ed25519{,.pub}`.
    fn key_dir() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        thegn_core::fsperm::restrict_dir_to_owner(temp.path()).unwrap();
        let key = temp.path().join("managed_ed25519");
        std::fs::write(&key, "private").unwrap();
        std::fs::write(public_path(&key), "public").unwrap();
        (temp, key)
    }

    fn custody_for(key: &Path, recovery: Option<RotationRecovery>) -> AuthorizedKeyRecord {
        AuthorizedKeyRecord {
            key_path: key.to_path_buf(),
            ..custody_record("instance-a", recovery)
        }
    }

    fn plant(path: &Path) {
        std::fs::write(path, "leftover").unwrap();
    }

    #[test]
    fn rotation_sibling_name_matcher_table() {
        let stem = "managed_ed25519";
        let accepted = [
            format!("{stem}.next-{ID}"),
            format!("{stem}.next-{ID}.pub"),
            format!("{stem}.retiring-{ID}"),
            format!("{stem}.retiring-{ID}.pub"),
            // short_hash is base36: letters past `f` are in the alphabet.
            format!("{stem}.next-zyxwvutsrq012345"),
        ];
        for name in &accepted {
            assert!(is_rotation_sibling_name(stem, name), "must accept {name}");
        }
        let rejected = [
            format!("{stem}.staging-{ID}"),
            format!("{stem}.next-{}", &ID[..15]),
            format!("{stem}.next-{ID}0"),
            format!("{stem}.retiring-{}", ID.to_ascii_uppercase()),
            format!("{stem}.next-0123456789abcde_"),
            format!("other.next-{ID}"),
            format!("x{stem}.next-{ID}"),
            format!("{stem}next-{ID}"),
            format!("{stem}.next{ID}"),
            format!("{stem}.next-{ID}.pub.pub"),
            format!("{stem}.next-{ID}.bak"),
            stem.to_string(),
            format!("{stem}.pub"),
        ];
        for name in &rejected {
            assert!(!is_rotation_sibling_name(stem, name), "must reject {name}");
        }
        // The matcher accepts exactly what the rotation itself mints.
        let key = Path::new("/keys/managed_ed25519");
        for label in SIBLING_LABELS {
            let sibling = rotation_sibling(key, label);
            for path in [sibling.clone(), public_path(&sibling)] {
                let name = path.file_name().unwrap().to_str().unwrap();
                assert!(is_rotation_sibling_name(stem, name), "minted {name}");
            }
        }
    }

    #[test]
    fn sweep_removes_only_unreferenced_regular_siblings() {
        let (temp, key) = key_dir();
        let orphan = temp.path().join(format!("managed_ed25519.retiring-{ID}"));
        plant(&orphan);
        plant(&public_path(&orphan));
        // An in-flight recovery of another row still names this pair.
        let auxiliary = temp.path().join("managed_ed25519.next-aaaaaaaaaaaaaaaa");
        let backup = temp
            .path()
            .join("managed_ed25519.retiring-bbbbbbbbbbbbbbbb");
        for path in [&auxiliary, &backup] {
            plant(path);
            plant(&public_path(path));
        }
        let unrelated = temp.path().join(format!("other.next-{ID}"));
        plant(&unrelated);
        let custody = [
            custody_for(&key, None),
            AuthorizedKeyRecord {
                instance: "instance-b".to_string(),
                ..custody_for(
                    &key,
                    Some(RotationRecovery {
                        key_path: auxiliary.clone(),
                        backup_key_path: Some(backup.clone()),
                        ..recovery_marker("pending")
                    }),
                )
            },
        ];

        let planned = plan_orphan_sweep(&key, &custody).unwrap().unwrap();
        assert_eq!(planned.orphans, vec![orphan.clone(), public_path(&orphan)]);
        assert!(orphan.exists(), "planning (the dry run) deletes nothing");
        drop(planned);

        assert_eq!(sweep_orphan_siblings(&key, &custody), 2);
        assert!(!orphan.exists() && !public_path(&orphan).exists());
        for path in [&key, &auxiliary, &backup] {
            assert!(path.exists(), "{} must survive", path.display());
            assert!(public_path(path).exists());
        }
        assert!(unrelated.exists(), "another stem is never swept");
        assert_eq!(sweep_orphan_siblings(&key, &custody), 0);
    }

    #[test]
    fn sweep_leaves_a_sibling_named_symlink_and_its_target() {
        let (temp, key) = key_dir();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("precious");
        std::fs::write(&target, "keep me").unwrap();
        let link = temp.path().join(format!("managed_ed25519.retiring-{ID}"));
        if thegn_core::fsperm::create_symlink(&target, &link, false).is_err() {
            return;
        }
        assert_eq!(sweep_orphan_siblings(&key, &[custody_for(&key, None)]), 0);
        assert!(std::fs::symlink_metadata(&link).is_ok(), "link survives");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep me");
    }

    #[test]
    fn sweep_skips_keys_sharing_a_sibling_namespace() {
        let (temp, key) = key_dir();
        // `managed_ed25519.x` also mints `managed_ed25519.next-*` siblings.
        let neighbour = temp.path().join("managed_ed25519.x");
        let orphan = temp.path().join(format!("managed_ed25519.retiring-{ID}"));
        plant(&orphan);
        let custody = [custody_for(&key, None), custody_for(&neighbour, None)];
        assert!(shares_sibling_namespace(&key, &custody));
        assert!(shares_sibling_namespace(&neighbour, &custody));
        assert!(plan_orphan_sweep(&key, &custody).unwrap().is_none());
        assert_eq!(sweep_orphan_siblings(&key, &custody), 0);
        assert_eq!(sweep_orphan_siblings(&neighbour, &custody), 0);
        assert!(orphan.exists());

        // The same stem in another directory is a different namespace.
        let elsewhere = tempfile::tempdir().unwrap();
        let custody = [
            custody_for(&key, None),
            custody_for(&elsewhere.path().join("managed_ed25519"), None),
        ];
        assert!(!shares_sibling_namespace(&key, &custody));
        assert_eq!(sweep_orphan_siblings(&key, &custody), 1);
        assert!(!orphan.exists());
    }

    #[test]
    fn sweep_skips_a_non_regular_entry() {
        let (temp, key) = key_dir();
        let directory = temp.path().join(format!("managed_ed25519.next-{ID}"));
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("inner"), "x").unwrap();
        assert_eq!(sweep_orphan_siblings(&key, &[custody_for(&key, None)]), 0);
        assert!(directory.join("inner").exists());
    }

    #[test]
    fn sweep_refuses_a_key_dir_that_is_not_owner_only() {
        let (temp, key) = key_dir();
        let orphan = temp.path().join(format!("managed_ed25519.retiring-{ID}"));
        plant(&orphan);
        if thegn_core::fsperm::set_mode_for_test(temp.path(), 0o755).is_err() {
            return;
        }
        assert!(plan_orphan_sweep(&key, &[custody_for(&key, None)]).is_err());
        assert_eq!(sweep_orphan_siblings(&key, &[custody_for(&key, None)]), 0);
        assert!(orphan.exists());
    }
}
