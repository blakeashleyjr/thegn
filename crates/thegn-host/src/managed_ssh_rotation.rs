//! Transactional managed-SSH-key rotation.
//!
//! Every instance first receives and verifies the replacement. Only after the
//! entire scope verifies do we promote the local pair and remove the old key.
//! A de-authorization failure re-adds the old key everywhere before restoring
//! the old local pair, preserving the invariant that partial failure leaves both
//! keys usable instead of stranding any managed machine.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use thegn_core::config::Config;
use thegn_core::managed_ssh::{AuthorizedKeyRecord, RotationRecovery};

static ROTATION_NONCE: AtomicU64 = AtomicU64::new(0);

pub struct RotationSummary {
    pub scopes: usize,
    pub instances: usize,
    pub dry_run: bool,
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
        }
        return Ok(RotationSummary {
            scopes: scopes.len(),
            instances,
            dry_run: true,
        });
    }

    for (key_path, records) in &scopes {
        rotate_scope(cfg, key_path, records)?;
    }
    Ok(RotationSummary {
        scopes: scopes.len(),
        instances,
        dry_run: false,
    })
}

fn rotate_scope(cfg: &Config, key_path: &Path, records: &[AuthorizedKeyRecord]) -> Result<()> {
    let mut tracked = records.to_vec();
    let resumed_recovery = tracked
        .iter()
        .any(|record| record.rotation_recovery.is_some());
    recover_pending(cfg, key_path, &mut tracked).with_context(|| {
        format!(
            "recover an interrupted managed SSH rotation for {}",
            key_path.display()
        )
    })?;
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
    let staging = rotation_sibling(key_path, "next");
    let backup = rotation_sibling(key_path, "retiring");
    crate::agent::generate_managed_keypair_at(&staging, "thegn-managed-rotation")?;
    let staging_public_path = public_path(&staging);
    let new_public = std::fs::read_to_string(&staging_public_path).with_context(|| {
        format!(
            "read replacement public key {}",
            staging_public_path.display()
        )
    })?;
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
        let cleanup = clear_unstarted_recovery(&mut tracked, &staging);
        return Err(combine_recovery_error(
            error.context("record replacement-key recovery plan"),
            cleanup,
            &staging,
        ));
    }

    // Phase 1: old-key transport authorizes the replacement, then a separately
    // constructed provider proves an actual command with the replacement key.
    for index in 0..tracked.len() {
        let phase = (|| -> Result<()> {
            let record = &tracked[index];
            let pc = provider_config(cfg, record, key_path)?;
            let old = crate::provider_factory::provider_for_named_with_key(
                pc,
                &record.instance,
                Some((key_path.to_path_buf(), old_public.trim().to_string())),
            )
            .ok_or_else(|| anyhow!("{} provider is unavailable", record.provider))?;
            run_checked(
                &old,
                &record.instance,
                &authorize_script(&new_public),
                "authorize replacement key",
            )?;
            Ok(())
        })();
        if let Err(error) = phase {
            let cleanup = recover_pending(cfg, key_path, &mut tracked);
            return Err(combine_recovery_error(error, cleanup, &staging));
        }
        let mut recovery = tracked[index]
            .rotation_recovery
            .clone()
            .expect("recovery plan was persisted before remote mutation");
        recovery.authorized = true;
        if let Err(error) = update_recovery(&mut tracked[index], recovery) {
            let cleanup = recover_pending(cfg, key_path, &mut tracked);
            return Err(combine_recovery_error(
                error.context("record replacement authorization"),
                cleanup,
                &staging,
            ));
        }

        let proof = (|| -> Result<()> {
            let record = &tracked[index];
            let pc = provider_config(cfg, record, key_path)?;
            if record.provider == "sprites" {
                run_sprite_checked(record, &staging, "true", "verify replacement key")
            } else {
                let replacement = crate::provider_factory::provider_for_named_with_key(
                    pc,
                    &record.instance,
                    Some((staging.clone(), new_public.trim().to_string())),
                )
                .ok_or_else(|| {
                    anyhow!(
                        "{} replacement-key provider is unavailable",
                        record.provider
                    )
                })?;
                run_checked(
                    &replacement,
                    &record.instance,
                    "true",
                    "verify replacement key",
                )
            }
        })();
        if let Err(error) = proof {
            let cleanup = recover_pending(cfg, key_path, &mut tracked);
            return Err(combine_recovery_error(error, cleanup, &staging));
        }
        let mut recovery = tracked[index]
            .rotation_recovery
            .clone()
            .expect("authorized recovery plan remains present");
        recovery.verified = true;
        if let Err(error) = update_recovery(&mut tracked[index], recovery) {
            let cleanup = recover_pending(cfg, key_path, &mut tracked);
            return Err(combine_recovery_error(
                error.context("record replacement verification"),
                cleanup,
                &staging,
            ));
        }
    }

    // Resolve credentials and construct every canonical-path replacement
    // provider before touching local custody. Nothing after this point can fail
    // merely because a token/keyring backend disappeared between phases.
    let canonical_providers = tracked
        .iter()
        .map(|record| {
            let pc = provider_config(cfg, record, key_path)?;
            crate::provider_factory::provider_for_named_with_key(
                pc,
                &record.instance,
                Some((key_path.to_path_buf(), new_public.trim().to_string())),
            )
            .ok_or_else(|| {
                anyhow!(
                    "{} canonical replacement-key provider is unavailable",
                    record.provider
                )
            })
        })
        .collect::<Result<Vec<_>>>();
    let canonical_providers = match canonical_providers {
        Ok(providers) => providers,
        Err(error) => {
            let cleanup = recover_pending(cfg, key_path, &mut tracked);
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
            let cleanup = recover_pending(cfg, key_path, &mut tracked);
            return Err(combine_recovery_error(
                error.context("record old-key retirement boundary"),
                cleanup,
                &staging,
            ));
        }
    }

    // Promote locally while retaining a rollback copy of the old pair. Remote
    // de-authorization below uses providers reconstructed against the canonical
    // path, so every future caller also sees the replacement immediately.
    if let Err(error) = promote_pair(key_path, &staging, &backup) {
        let cleanup = recover_pending(cfg, key_path, &mut tracked);
        return Err(combine_recovery_error(
            error.context("promote replacement managed SSH key"),
            cleanup,
            &staging,
        ));
    }

    let mut deauthorized = 0usize;
    for (record, replacement) in tracked.iter().zip(&canonical_providers) {
        if let Err(error) = run_checked(
            replacement,
            &record.instance,
            &deauthorize_script(&old_public),
            "de-authorize old key",
        ) {
            // Best effort, across the whole scope rather than only completed
            // records: append is idempotent and this maximizes the both-keys-live
            // safety invariant before restoring the old local pair.
            let restore_errors = attempt_all(tracked.len(), |index| {
                run_checked(
                    &canonical_providers[index],
                    &tracked[index].instance,
                    &authorize_script(&old_public),
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
                let cleanup = recover_pending(cfg, key_path, &mut tracked);
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
    }

    // Mark the all-remotes-retired boundary durably before clearing individual
    // recovery rows. If a later custody write fails partway, any surviving row
    // proves it is safe to complete forward to the canonical replacement.
    for record in &mut tracked {
        if let Some(recovery) = &mut record.rotation_recovery {
            recovery.retiring_completed = true;
        }
    }
    let completion_errors = tracked
        .iter()
        .filter_map(|record| {
            thegn_core::managed_ssh::record_authorized_at(&thegn_core::managed_ssh::dir(), record)
                .err()
        })
        .collect::<Vec<_>>();
    if let Some(error) = completion_errors.into_iter().next() {
        let cleanup = recover_pending(cfg, key_path, &mut tracked);
        return Err(combine_recovery_error(
            error.context("record completed remote retirement boundary"),
            cleanup,
            &staging,
        ));
    }

    for record in &tracked {
        thegn_core::managed_ssh::record_authorized_with_proxy_worktree(
            &record.provider,
            &record.account,
            &record.instance,
            key_path,
            &new_public,
            record.proxy_worktree.as_deref(),
        )?;
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
        16,
    );
    key_path.with_extension(format!("{label}-{identity}"))
}

fn update_recovery(record: &mut AuthorizedKeyRecord, recovery: RotationRecovery) -> Result<()> {
    record.rotation_recovery = Some(recovery);
    thegn_core::managed_ssh::record_authorized_at(&thegn_core::managed_ssh::dir(), record)
}

fn clear_recovery(record: &mut AuthorizedKeyRecord) -> Result<()> {
    record.rotation_recovery = None;
    thegn_core::managed_ssh::record_authorized_at(&thegn_core::managed_ssh::dir(), record)
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
fn recover_pending(
    cfg: &Config,
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
        for &index in &pending {
            let record = &records[index];
            thegn_core::managed_ssh::record_authorized_with_proxy_worktree(
                &record.provider,
                &record.account,
                &record.instance,
                key_path,
                &canonical_public,
                record.proxy_worktree.as_deref(),
            )?;
            records[index].key_path = key_path.to_path_buf();
            records[index].key_fingerprint = recovery.key_fingerprint.clone();
            records[index].authorized_at = thegn_core::util::now();
            records[index].rotation_recovery = None;
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
            let pc = provider_config(cfg, record, key_path)?;
            let auxiliary = crate::provider_factory::provider_for_named_with_key(
                pc,
                &record.instance,
                Some((
                    auxiliary_key_path.clone(),
                    auxiliary_public.trim().to_string(),
                )),
            )
            .ok_or_else(|| anyhow!("{} recovery provider is unavailable", record.provider))?;
            run_checked(
                &auxiliary,
                &record.instance,
                &authorize_script(&primary_public),
                "restore primary key during rotation recovery",
            )?;
        }
    }

    for &index in &pending {
        let record = &records[index];
        let pc = provider_config(cfg, record, key_path)?;
        let primary = crate::provider_factory::provider_for_named_with_key(
            pc,
            &record.instance,
            Some((primary_key_path.clone(), primary_public.trim().to_string())),
        )
        .ok_or_else(|| anyhow!("{} recovery provider is unavailable", record.provider))?;
        run_checked(
            &primary,
            &record.instance,
            &deauthorize_script(&auxiliary_public),
            "remove auxiliary key during rotation recovery",
        )?;
    }

    if restore_local {
        let backup = recovery
            .backup_key_path
            .as_deref()
            .expect("promoted recovery has a retiring path");
        restore_pair(key_path, &recovery.key_path, backup)?;
    }
    for &index in &pending {
        clear_recovery(&mut records[index])?;
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

fn authorize_script(public_key: &str) -> String {
    let key = thegn_core::util::sh_quote(public_key.trim());
    format!(
        "set -eu; mkdir -p \"$HOME/.ssh\"; chmod 700 \"$HOME/.ssh\"; touch \"$HOME/.ssh/authorized_keys\"; chmod 600 \"$HOME/.ssh/authorized_keys\"; grep -qF -- {key} \"$HOME/.ssh/authorized_keys\" || printf '%s\\n' {key} >> \"$HOME/.ssh/authorized_keys\""
    )
}

fn deauthorize_script(public_key: &str) -> String {
    let key = thegn_core::util::sh_quote(public_key.trim());
    format!(
        "set -eu; f=\"$HOME/.ssh/authorized_keys\"; t=\"$f.thegn.$$\"; grep -vF -- {key} \"$f\" > \"$t\" || true; chmod 600 \"$t\"; mv \"$t\" \"$f\""
    )
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
mod tests {
    use super::*;

    #[test]
    fn remote_scripts_do_not_embed_private_material_and_are_idempotent() {
        let public = "ssh-ed25519 AAAATEST rotation@test";
        let add = authorize_script(public);
        let remove = deauthorize_script(public);
        assert!(add.contains(public));
        assert!(remove.contains(public));
        assert!(add.contains("grep -qF"));
        assert!(remove.contains("grep -vF"));
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
}
