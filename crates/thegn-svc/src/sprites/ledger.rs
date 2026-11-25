//! Sprites ownership ledger: the file store in [`crate::vps::registry`] with
//! `provider = "sprites"`, `owner` = this host's controller id and `account` =
//! the credential-free account fingerprint.
//!
//! `create` reserves a `creating` record **before** the POST and commits it as
//! `ready` after; `destroy` asks [`prove_owned`] first and never deletes a
//! sprite it cannot prove. Only a record this host wrote before the create
//! proves a sprite: a sprite with no such record (one made before the ledger,
//! or one a worktree location row merely names) is never proven.
//!
//! The registry directory is shared with VPS/Fly/Vercel records: every read
//! here filters on `provider == "sprites"`, and nothing here removes or
//! rewrites another provider's record.
//!
//! [`adopt`] is the one exception to "only a record written before the create
//! proves a sprite": adoption is the only way a sprite not created by this host
//! becomes provable; it needs an explicit, confirmed user action
//! (`thegn env adopt`, which asks the user to type the sprite's name).

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::vps::registry::{self, IntentGuard, ReserveError, ReserveScope, VpsRecord};

/// The ledger `provider` value for sprites records.
pub const LEDGER_PROVIDER: &str = "sprites";

/// Who may create and destroy: this host's controller id and the provider
/// account fingerprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerScope {
    pub owner: String,
    pub account: String,
}

/// How a sprite was proven to be ours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proof {
    /// A ledger record under this scope.
    Ledger,
}

/// The sprites records under one scope.
pub struct OwnedView {
    pub recorded: Vec<VpsRecord>,
    /// Record names that exist but could not be read.
    pub unreadable: Vec<String>,
}

fn record(scope: &OwnerScope, name: &str, state: &str, created_at: i64) -> VpsRecord {
    VpsRecord {
        name: name.to_string(),
        provider: LEDGER_PROVIDER.into(),
        state: state.into(),
        instance_id: if state == "ready" {
            name.to_string()
        } else {
            String::new()
        },
        ip: String::new(),
        created_at,
        owner: scope.owner.clone(),
        account: scope.account.clone(),
    }
}

fn in_scope(record: &VpsRecord, scope: &OwnerScope) -> bool {
    record.provider == LEDGER_PROVIDER
        && !scope.owner.is_empty()
        && !scope.account.is_empty()
        && record.owner == scope.owner
        && record.account == scope.account
}

fn reserve_error(name: &str, error: ReserveError) -> anyhow::Error {
    match error {
        ReserveError::AtCap { occupied, cap } => anyhow::anyhow!(
            "sprites: max_instances reached ({occupied} recorded; cap {cap}); refusing to create {name}"
        ),
        ReserveError::OwnedByOther { .. } => anyhow::anyhow!(
            "sprites: {name} is recorded by another controller; refusing to reuse it"
        ),
        ReserveError::OtherAccount { .. } => anyhow::anyhow!(
            "sprites: {name} is recorded under another Sprites account; refusing to reuse it"
        ),
        ReserveError::Store(error) => error.context(format!(
            "sprites: cannot write the ledger record for {name}"
        )),
    }
}

/// Reserve a `creating` intent for `name` (`cap` 0 = no cap). Dropping the
/// armed guard removes the intent.
pub fn reserve(scope: &OwnerScope, name: &str, cap: usize) -> Result<IntentGuard> {
    reserve_at(&registry::dir(), scope, name, cap)
}

/// [`reserve`] against an explicit ledger directory.
pub fn reserve_at(dir: &Path, scope: &OwnerScope, name: &str, cap: usize) -> Result<IntentGuard> {
    registry::checked_name(name).context("sprites: invalid sprite name")?;
    if scope.owner.is_empty() || scope.account.is_empty() {
        bail!("sprites: no owner scope; refusing to create an unrecorded sprite");
    }
    let cap = if cap == 0 { usize::MAX } else { cap };
    let intent = record(scope, name, "creating", thegn_core::util::now());
    // The other-provider refusal runs under the reserve lock, on the same
    // single strict read as the owner/account checks.
    let mut foreign = None;
    let reserved = registry::reserve_checked_at(
        dir,
        &intent,
        cap,
        ReserveScope::Provider(LEDGER_PROVIDER),
        |existing| {
            if existing.provider != LEDGER_PROVIDER {
                foreign = Some(existing.provider.clone());
                bail!("recorded by another provider");
            }
            Ok(())
        },
    );
    if let Some(provider) = foreign {
        bail!(
            "sprites: the name {name} is recorded by another provider ({provider}); refusing to reuse it"
        );
    }
    reserved.map_err(|error| reserve_error(name, error))?;
    Ok(IntentGuard::at(dir, name))
}

/// What [`adopt`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdoptOutcome {
    /// A new `ready` record now proves the sprite ours.
    Adopted,
    /// A record under this scope already existed; nothing was written.
    AlreadyOurs,
}

/// Record an existing sprite (one this host did not create) as ours, so a
/// later destroy can prove it. Only `thegn env adopt`, after the user confirmed
/// the sprite's name, may call this. `cap` 0 = no cap.
pub fn adopt(scope: &OwnerScope, name: &str, cap: usize) -> Result<AdoptOutcome> {
    adopt_at(&registry::dir(), scope, name, cap)
}

/// [`adopt`] against an explicit ledger directory.
///
/// - a record for another provider, controller or account → refused;
/// - a sprites record under `scope` → [`AdoptOutcome::AlreadyOurs`] (no write);
/// - no record → a `ready` record with `created_at = now` (the lifetime counts
///   from adoption), reserved under the sprites cap.
///
/// The decision and the write are one step under the ledger lock (taken once,
/// in [`registry::reserve_checked_at`], with one strict read of the existing
/// record): an unreadable record is an error, never treated as absent.
pub fn adopt_at(dir: &Path, scope: &OwnerScope, name: &str, cap: usize) -> Result<AdoptOutcome> {
    /// What the locked check found in an existing record.
    enum Found {
        Ours,
        OtherProvider(String),
        OtherScope,
    }
    registry::checked_name(name).context("sprites: invalid sprite name")?;
    if scope.owner.is_empty() || scope.account.is_empty() {
        bail!("sprites: no owner scope; refusing to adopt {name}");
    }
    let cap = if cap == 0 { usize::MAX } else { cap };
    let adopted = record(scope, name, "ready", thegn_core::util::now());
    // Any existing record aborts the write from inside the lock; `found` says
    // why. Only an absent record reaches the cap check and the write.
    let mut found = None;
    let reserved = registry::reserve_checked_at(
        dir,
        &adopted,
        cap,
        ReserveScope::Provider(LEDGER_PROVIDER),
        |existing| {
            found = Some(if existing.provider != LEDGER_PROVIDER {
                Found::OtherProvider(existing.provider.clone())
            } else if in_scope(existing, scope) {
                Found::Ours
            } else {
                Found::OtherScope
            });
            bail!("an existing record is never rewritten by adopt")
        },
    );
    match found {
        Some(Found::Ours) => Ok(AdoptOutcome::AlreadyOurs),
        Some(Found::OtherProvider(provider)) => bail!(
            "sprites: the name {name} is recorded by another provider ({provider}); refusing to adopt it"
        ),
        Some(Found::OtherScope) => {
            bail!("sprites: {name} is recorded by another controller/account; refusing to adopt it")
        }
        None => {
            reserved.map_err(|error| reserve_error(name, error))?;
            Ok(AdoptOutcome::Adopted)
        }
    }
}

/// Mark `name` `ready` after a successful create.
pub fn commit(scope: &OwnerScope, name: &str) -> Result<()> {
    commit_at(&registry::dir(), scope, name)
}

/// [`commit`] against an explicit ledger directory. Under the ledger lock,
/// with one strict read: an in-scope sprites record becomes `ready` (keeping
/// the intent's `created_at`); an absent record (forgotten meanwhile) or one
/// held by another controller, account or provider is refused.
pub fn commit_at(dir: &Path, scope: &OwnerScope, name: &str) -> Result<()> {
    registry::checked_name(name).context("sprites: invalid sprite name")?;
    registry::update_locked_at(dir, name, |existing| match existing {
        Some(existing) if in_scope(&existing, scope) => Ok(Some(record(
            scope,
            name,
            "ready",
            existing.created_at,
        ))),
        Some(existing) if existing.provider != LEDGER_PROVIDER => bail!(
            "sprites: {name} is recorded by another provider ({}); refusing to mark it ready",
            existing.provider
        ),
        Some(_) => bail!(
            "sprites: {name} is recorded by another controller/account; refusing to mark it ready"
        ),
        None => bail!(
            "sprites: the ledger has no intent for {name} (it was forgotten meanwhile); refusing to mark it ready"
        ),
    })
    .with_context(|| format!("sprites: cannot mark {name} ready in the ledger"))
}

/// Prove `name` is ours before a destroy. `Err` = refuse the delete.
///
/// `legacy_ids` is ignored (kept only so the provider's call sites keep their
/// signature; production always passes an empty list): nothing but a ledger
/// record proves ownership.
pub fn prove_owned(scope: &OwnerScope, name: &str, legacy_ids: &[String]) -> Result<Proof> {
    prove_owned_at(&registry::dir(), scope, name, legacy_ids)
}

/// [`prove_owned`] against an explicit ledger directory.
///
/// - a sprites record under `scope` → [`Proof::Ledger`];
/// - any other record (another controller, account or provider) → refused;
/// - no record → refused (no worktree row or `legacy_ids` entry proves
///   anything). An unreadable record or ledger refuses too.
///
/// `legacy_ids` is ignored; see [`prove_owned`].
pub fn prove_owned_at(
    dir: &Path,
    scope: &OwnerScope,
    name: &str,
    _legacy_ids: &[String],
) -> Result<Proof> {
    registry::checked_name(name).context("sprites: invalid sprite name")?;
    if scope.owner.is_empty() || scope.account.is_empty() {
        bail!("sprites: no owner scope; refusing to treat {name} as ours");
    }
    let existing = registry::read_checked_at(dir, name)
        .with_context(|| format!("sprites: cannot read the ledger record for {name}"))?;
    match existing {
        Some(existing) if in_scope(&existing, scope) => Ok(Proof::Ledger),
        Some(existing) if existing.provider != LEDGER_PROVIDER => bail!(
            "sprites: {name} is recorded by another provider ({}); refusing to treat it as a sprite of ours",
            existing.provider
        ),
        Some(_) => bail!(
            "sprites: {name} is recorded by another controller/account; refusing to treat it as ours"
        ),
        None => bail!(
            "sprites: thegn has no record of creating {name}; refusing to delete it (it may belong to another thegn instance, e.g. stable vs dev). If it is yours: thegn env adopt {name} --env <env>"
        ),
    }
}

/// Drop `name`'s record after the sprite is gone.
pub fn forget(scope: &OwnerScope, name: &str) -> Result<()> {
    forget_at(&registry::dir(), scope, name)
}

/// [`forget`] against an explicit ledger directory. Under the ledger lock,
/// with one strict read: only a sprites record under `scope` is removed. A
/// record that is already gone is `Ok` (idempotent); an unreadable one is an
/// error; another provider's, controller's or account's record is refused and
/// left alone.
pub fn forget_at(dir: &Path, scope: &OwnerScope, name: &str) -> Result<()> {
    forget_where(dir, scope, name, |_| Ok(()))
}

/// The reaper's removal: [`forget_at`], additionally refusing unless the
/// record is still `creating` (`creating == true`, a stale intent) or still
/// not `creating` (`creating == false`, an orphan record) - the state the
/// pass judged, re-checked under the lock.
pub fn forget_reaped(scope: &OwnerScope, name: &str, creating: bool) -> Result<()> {
    forget_reaped_at(&registry::dir(), scope, name, creating)
}

/// [`forget_reaped`] against an explicit ledger directory.
pub fn forget_reaped_at(dir: &Path, scope: &OwnerScope, name: &str, creating: bool) -> Result<()> {
    forget_where(dir, scope, name, |existing| {
        if (existing.state == "creating") != creating {
            bail!("sprites: the ledger record for {name} changed state; left alone");
        }
        Ok(())
    })
}

/// One locked read-decide-remove (see [`registry::remove_locked_at`]); the
/// lock is taken exactly once, there.
fn forget_where(
    dir: &Path,
    scope: &OwnerScope,
    name: &str,
    also: impl FnOnce(&VpsRecord) -> Result<()>,
) -> Result<()> {
    registry::checked_name(name).context("sprites: invalid sprite name")?;
    registry::remove_locked_at(dir, name, |existing| {
        if existing.provider != LEDGER_PROVIDER {
            bail!(
                "sprites: the ledger name {name} belongs to another provider ({}); left alone",
                existing.provider
            );
        }
        if !in_scope(existing, scope) {
            bail!("sprites: {name} is recorded by another controller/account; left alone");
        }
        also(existing)?;
        Ok(true)
    })
    .map(|_| ())
    .with_context(|| format!("sprites: cannot forget {name} in the ledger"))
}

/// Every sprites record under `scope`.
pub fn owned(scope: &OwnerScope) -> Result<OwnedView> {
    owned_at(&registry::dir(), scope)
}

/// [`owned`] against an explicit ledger directory. `unreadable` names every
/// record that could not be read (its provider is unknown, so it may be ours).
pub fn owned_at(dir: &Path, scope: &OwnerScope) -> Result<OwnedView> {
    let view = registry::list_checked_at(dir)?;
    Ok(OwnedView {
        recorded: view
            .records
            .into_iter()
            .filter(|record| in_scope(record, scope))
            .collect(),
        unreadable: view.unreadable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> OwnerScope {
        OwnerScope {
            owner: "ctl-a".into(),
            account: "acct1-a".into(),
        }
    }

    fn other_owner() -> OwnerScope {
        OwnerScope {
            owner: "ctl-b".into(),
            account: "acct1-a".into(),
        }
    }

    fn other_account() -> OwnerScope {
        OwnerScope {
            owner: "ctl-a".into(),
            account: "acct1-b".into(),
        }
    }

    fn ledger() -> (thegn_core::testenv::IsolatedState, tempfile::TempDir) {
        (
            thegn_core::testenv::IsolatedState::new(),
            tempfile::tempdir().unwrap(),
        )
    }

    #[test]
    fn reserve_then_commit_writes_creating_then_ready() {
        let (_state, dir) = ledger();
        let mut guard = reserve_at(dir.path(), &scope(), "sx-a", 0).unwrap();
        let intent = registry::read_checked_at(dir.path(), "sx-a")
            .unwrap()
            .unwrap();
        assert_eq!(intent.provider, "sprites");
        assert_eq!(intent.state, "creating");
        assert_eq!(intent.owner, "ctl-a");
        assert_eq!(intent.account, "acct1-a");
        assert!(intent.created_at > 0);
        guard.preserve();
        drop(guard);
        commit_at(dir.path(), &scope(), "sx-a").unwrap();
        let ready = registry::read_checked_at(dir.path(), "sx-a")
            .unwrap()
            .unwrap();
        assert_eq!(ready.state, "ready");
        assert_eq!(ready.instance_id, "sx-a");
        assert_eq!(ready.created_at, intent.created_at, "intent time kept");
        assert_eq!(owned_at(dir.path(), &scope()).unwrap().recorded.len(), 1);
    }

    #[test]
    fn dropping_an_armed_reserve_guard_removes_the_intent() {
        let (_state, dir) = ledger();
        drop(reserve_at(dir.path(), &scope(), "sx-a", 0).unwrap());
        assert!(
            registry::read_checked_at(dir.path(), "sx-a")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn reserve_refuses_at_cap() {
        let (_state, dir) = ledger();
        for name in ["sx-a", "sx-b"] {
            reserve_at(dir.path(), &scope(), name, 2)
                .unwrap()
                .preserve();
        }
        let error = reserve_at(dir.path(), &scope(), "sx-c", 2)
            .err()
            .expect("the third sprite is over the cap");
        let text = format!("{error:#}");
        assert!(text.contains("max_instances"), "{text}");
        assert!(text.contains("sx-c"), "{text}");
        assert!(
            registry::read_checked_at(dir.path(), "sx-c")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn reserve_cap_ignores_other_providers_records() {
        let (_state, dir) = ledger();
        let mut vercel = record(&scope(), "vx-a", "ready", 1);
        vercel.provider = "vercel".into();
        registry::write_at(dir.path(), &vercel).unwrap();
        reserve_at(dir.path(), &scope(), "sx-a", 1)
            .unwrap()
            .preserve();
    }

    #[test]
    fn reserve_cap_zero_is_unlimited() {
        let (_state, dir) = ledger();
        for index in 0..8 {
            reserve_at(dir.path(), &scope(), &format!("sx-{index}"), 0)
                .unwrap()
                .preserve();
        }
        assert_eq!(owned_at(dir.path(), &scope()).unwrap().recorded.len(), 8);
    }

    #[test]
    fn reserve_refuses_another_scope_or_provider_without_leaking_the_token() {
        let (_state, dir) = ledger();
        reserve_at(dir.path(), &scope(), "sx-a", 0)
            .unwrap()
            .preserve();
        for other in [other_owner(), other_account()] {
            let error = reserve_at(dir.path(), &other, "sx-a", 0).err().unwrap();
            let text = format!("{error:#}");
            assert!(text.contains("sx-a"), "{text}");
            assert!(!text.contains("acct1-"), "{text}");
        }
        let mut vercel = record(&scope(), "vx-a", "ready", 1);
        vercel.provider = "vercel".into();
        registry::write_at(dir.path(), &vercel).unwrap();
        let error = reserve_at(dir.path(), &scope(), "vx-a", 0).err().unwrap();
        assert!(format!("{error:#}").contains("another provider"));
        assert_eq!(
            registry::read_checked_at(dir.path(), "vx-a")
                .unwrap()
                .unwrap()
                .provider,
            "vercel",
            "the vercel record is untouched"
        );
    }

    #[test]
    fn prove_owned_by_ledger_record() {
        let (_state, dir) = ledger();
        reserve_at(dir.path(), &scope(), "sx-a", 0)
            .unwrap()
            .preserve();
        assert_eq!(
            prove_owned_at(dir.path(), &scope(), "sx-a", &[]).unwrap(),
            Proof::Ledger
        );
    }

    #[test]
    fn a_worktree_location_row_naming_a_sprite_does_not_prove_ownership() {
        use thegn_core::store::WorkspaceStore;
        let (_state, other) = ledger();
        let db = thegn_core::db::Db::open().unwrap();
        let blob = serde_json::json!({
            "control_prefix": ["/nix/store/x/bin/thegn", "sprite-exec", "sx-row", "--"],
            "path": "/workspace",
        })
        .to_string();
        db.put_worktree("tab", "/repo", "/wt/r", "feat", Some(&blob), None)
            .unwrap();
        drop(db);
        let legacy = vec!["sx-row".to_string()];
        for dir in [registry::dir(), other.path().to_path_buf()] {
            let error = prove_owned_at(&dir, &scope(), "sx-row", &legacy).unwrap_err();
            assert!(
                format!("{error:#}").contains("no record of creating sx-row"),
                "{error:#}"
            );
            assert!(registry::read_checked_at(&dir, "sx-row").unwrap().is_none());
        }
    }

    #[test]
    fn prove_owned_refuses_foreign_records() {
        let (_state, dir) = ledger();
        reserve_at(dir.path(), &other_owner(), "sx-b", 0)
            .unwrap()
            .preserve();
        reserve_at(dir.path(), &other_account(), "sx-c", 0)
            .unwrap()
            .preserve();
        // A legacy id never overrides another scope's record.
        let legacy = vec!["sx-b".to_string(), "sx-c".to_string()];
        for name in ["sx-b", "sx-c"] {
            let error = prove_owned_at(dir.path(), &scope(), name, &legacy).unwrap_err();
            assert!(
                format!("{error:#}").contains("another controller/account"),
                "{error:#}"
            );
        }
        let mut vercel = record(&scope(), "vx-a", "ready", 1);
        vercel.provider = "vercel".into();
        registry::write_at(dir.path(), &vercel).unwrap();
        let error = prove_owned_at(dir.path(), &scope(), "vx-a", &[]).unwrap_err();
        assert!(format!("{error:#}").contains("another provider"));
    }

    #[test]
    fn prove_owned_refuses_a_missing_sprite_without_writing() {
        let (_state, dir) = ledger();
        let error = prove_owned_at(dir.path(), &scope(), "sx-none", &[]).unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            "sprites: thegn has no record of creating sx-none; refusing to delete it (it may belong to another thegn instance, e.g. stable vs dev). If it is yours: thegn env adopt sx-none --env <env>"
        );
        assert!(
            registry::read_checked_at(dir.path(), "sx-none")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn prove_owned_refuses_an_empty_scope() {
        let (_state, dir) = ledger();
        let empty = OwnerScope {
            owner: String::new(),
            account: String::new(),
        };
        assert!(prove_owned_at(dir.path(), &empty, "sx-a", &["sx-a".into()]).is_err());
    }

    #[test]
    fn forget_removes_only_sprites_records() {
        let (_state, dir) = ledger();
        reserve_at(dir.path(), &scope(), "sx-a", 0)
            .unwrap()
            .preserve();
        let mut vercel = record(&scope(), "vx-a", "ready", 1);
        vercel.provider = "vercel".into();
        registry::write_at(dir.path(), &vercel).unwrap();
        forget_at(dir.path(), &scope(), "sx-a").unwrap();
        let refused = forget_at(dir.path(), &scope(), "vx-a").unwrap_err();
        assert!(
            format!("{refused:#}").contains("another provider"),
            "{refused:#}"
        );
        // Already gone: idempotent.
        forget_at(dir.path(), &scope(), "sx-a").unwrap();
        assert!(
            registry::read_checked_at(dir.path(), "sx-a")
                .unwrap()
                .is_none()
        );
        assert!(
            registry::read_checked_at(dir.path(), "vx-a")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn adopt_writes_a_ready_record_that_proves_ownership() {
        let (_state, dir) = ledger();
        assert!(prove_owned_at(dir.path(), &scope(), "sx-adopt", &[]).is_err());
        let before = thegn_core::util::now();
        assert_eq!(
            adopt_at(dir.path(), &scope(), "sx-adopt", 0).unwrap(),
            AdoptOutcome::Adopted
        );
        let rec = registry::read_checked_at(dir.path(), "sx-adopt")
            .unwrap()
            .unwrap();
        assert_eq!(rec.provider, "sprites");
        assert_eq!(rec.state, "ready");
        assert_eq!(rec.instance_id, "sx-adopt");
        assert_eq!(rec.owner, "ctl-a");
        assert_eq!(rec.account, "acct1-a");
        assert!(rec.created_at >= before, "lifetime counts from adoption");
        assert_eq!(
            prove_owned_at(dir.path(), &scope(), "sx-adopt", &[]).unwrap(),
            Proof::Ledger
        );
    }

    #[test]
    fn adopt_an_already_recorded_sprite_is_a_no_op() {
        let (_state, dir) = ledger();
        reserve_at(dir.path(), &scope(), "sx-a", 0)
            .unwrap()
            .preserve();
        let before = registry::read_checked_at(dir.path(), "sx-a")
            .unwrap()
            .unwrap();
        assert_eq!(
            adopt_at(dir.path(), &scope(), "sx-a", 1).unwrap(),
            AdoptOutcome::AlreadyOurs
        );
        let after = registry::read_checked_at(dir.path(), "sx-a")
            .unwrap()
            .unwrap();
        assert_eq!(before, after, "an existing record is not rewritten");
    }

    #[test]
    fn adopt_refuses_foreign_owner_account_and_provider() {
        let (_state, dir) = ledger();
        reserve_at(dir.path(), &other_owner(), "sx-b", 0)
            .unwrap()
            .preserve();
        reserve_at(dir.path(), &other_account(), "sx-c", 0)
            .unwrap()
            .preserve();
        for name in ["sx-b", "sx-c"] {
            let error = adopt_at(dir.path(), &scope(), name, 0).unwrap_err();
            let text = format!("{error:#}");
            assert!(text.contains("another controller/account"), "{text}");
            assert!(!text.contains("acct1-"), "{text}");
        }
        let mut vercel = record(&scope(), "vx-a", "ready", 1);
        vercel.provider = "vercel".into();
        registry::write_at(dir.path(), &vercel).unwrap();
        let error = adopt_at(dir.path(), &scope(), "vx-a", 0).unwrap_err();
        assert!(format!("{error:#}").contains("another provider"));
        assert_eq!(
            registry::read_checked_at(dir.path(), "vx-a")
                .unwrap()
                .unwrap()
                .provider,
            "vercel",
            "the vercel record is untouched"
        );
    }

    #[test]
    fn adopt_refuses_at_cap_and_an_empty_scope() {
        let (_state, dir) = ledger();
        adopt_at(dir.path(), &scope(), "sx-a", 1).unwrap();
        let error = adopt_at(dir.path(), &scope(), "sx-b", 1).unwrap_err();
        assert!(format!("{error:#}").contains("max_instances"), "{error:#}");
        assert!(
            registry::read_checked_at(dir.path(), "sx-b")
                .unwrap()
                .is_none()
        );
        let empty = OwnerScope {
            owner: String::new(),
            account: String::new(),
        };
        assert!(adopt_at(dir.path(), &empty, "sx-c", 0).is_err());
        assert!(adopt_at(dir.path(), &scope(), "", 0).is_err());
        assert!(
            registry::read_checked_at(dir.path(), "sx-c")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn forget_refuses_another_scope_and_an_unreadable_record() {
        let (_state, dir) = ledger();
        reserve_at(dir.path(), &other_owner(), "sx-b", 0)
            .unwrap()
            .preserve();
        let refused = forget_at(dir.path(), &scope(), "sx-b").unwrap_err();
        assert!(
            format!("{refused:#}").contains("another controller/account"),
            "{refused:#}"
        );
        assert!(
            registry::read_checked_at(dir.path(), "sx-b")
                .unwrap()
                .is_some()
        );
        std::fs::write(dir.path().join("sx-c.json"), b"{not json").unwrap();
        assert!(forget_at(dir.path(), &scope(), "sx-c").is_err());
        assert!(dir.path().join("sx-c.json").exists());
    }

    #[test]
    fn commit_refuses_an_absent_or_foreign_record() {
        let (_state, dir) = ledger();
        let absent = commit_at(dir.path(), &scope(), "sx-a").unwrap_err();
        assert!(format!("{absent:#}").contains("no intent"), "{absent:#}");
        assert!(
            registry::read_checked_at(dir.path(), "sx-a")
                .unwrap()
                .is_none(),
            "a refused commit writes nothing"
        );
        reserve_at(dir.path(), &other_owner(), "sx-b", 0)
            .unwrap()
            .preserve();
        assert!(commit_at(dir.path(), &scope(), "sx-b").is_err());
        assert_eq!(
            registry::read_checked_at(dir.path(), "sx-b")
                .unwrap()
                .unwrap()
                .state,
            "creating"
        );
    }

    #[test]
    fn forget_reaped_rechecks_the_state_under_the_lock() {
        let (_state, dir) = ledger();
        reserve_at(dir.path(), &scope(), "sx-a", 0)
            .unwrap()
            .preserve();
        commit_at(dir.path(), &scope(), "sx-a").unwrap();
        // Judged a stale intent, but it is `ready` now: left alone.
        assert!(forget_reaped_at(dir.path(), &scope(), "sx-a", true).is_err());
        assert!(
            registry::read_checked_at(dir.path(), "sx-a")
                .unwrap()
                .is_some()
        );
        forget_reaped_at(dir.path(), &scope(), "sx-a", false).unwrap();
        assert!(
            registry::read_checked_at(dir.path(), "sx-a")
                .unwrap()
                .is_none()
        );
    }

    /// Run `a` and `b` on two threads released by one barrier; join each with
    /// a bound, panicking with the thread's name if it does not finish.
    fn race<A, B>(a: (&'static str, A), b: (&'static str, B)) -> (Result<()>, Result<()>)
    where
        A: FnOnce() -> Result<()> + Send + 'static,
        B: FnOnce() -> Result<()> + Send + 'static,
    {
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let spawn = |name: &'static str, run: Box<dyn FnOnce() -> Result<()> + Send>| {
            let barrier = barrier.clone();
            let (sender, receiver) = std::sync::mpsc::channel();
            std::thread::Builder::new()
                .name(name.into())
                .spawn(move || {
                    barrier.wait();
                    sender.send(run()).unwrap_or_default(); // best-effort: the joiner may have timed out
                })
                .unwrap();
            (name, receiver)
        };
        let first = spawn(a.0, Box::new(a.1));
        let second = spawn(b.0, Box::new(b.1));
        let join = |(name, receiver): (&str, std::sync::mpsc::Receiver<Result<()>>)| {
            receiver
                .recv_timeout(std::time::Duration::from_secs(30))
                .unwrap_or_else(|error| {
                    panic!("thread {name} did not finish within 30s ({error}); a lock deadlock?")
                })
        };
        (join(first), join(second))
    }

    fn assert_not_unreadable(name: &str, result: &Result<()>) {
        if let Err(error) = result {
            let text = format!("{error:#}");
            assert!(
                !text.contains("unreadable existing record") && !text.contains("is unreadable"),
                "thread {name} saw an unreadable record: {text}"
            );
        }
    }

    #[test]
    fn concurrent_commit_and_forget_end_in_a_legal_state() {
        for round in 0..50 {
            let (_state, dir) = ledger();
            let path = dir.path().to_path_buf();
            reserve_at(&path, &scope(), "sx-a", 0).unwrap().preserve();
            let (commit_dir, forget_dir) = (path.clone(), path.clone());
            let (committed, forgot) = race(
                ("commit", move || commit_at(&commit_dir, &scope(), "sx-a")),
                ("forget", move || forget_at(&forget_dir, &scope(), "sx-a")),
            );
            assert_not_unreadable("commit", &committed);
            assert_not_unreadable("forget", &forgot);
            forgot.unwrap_or_else(|error| panic!("round {round}: forget failed: {error:#}"));
            // Either order ends with no record: commit-then-forget removes the
            // `ready` record; forget-then-commit refuses (no intent left).
            assert!(
                registry::read_checked_at(&path, "sx-a").unwrap().is_none(),
                "round {round}: a record survived (commit: {committed:?})"
            );
            if let Err(error) = committed {
                assert!(
                    format!("{error:#}").contains("no intent"),
                    "round {round}: {error:#}"
                );
            }
        }
    }

    #[test]
    fn concurrent_forget_and_reserve_end_in_a_legal_state() {
        for round in 0..50 {
            let (_state, dir) = ledger();
            let path = dir.path().to_path_buf();
            reserve_at(&path, &scope(), "sx-a", 0).unwrap().preserve();
            commit_at(&path, &scope(), "sx-a").unwrap();
            let (forget_dir, reserve_dir) = (path.clone(), path.clone());
            let (forgot, reserved) = race(
                ("forget", move || forget_at(&forget_dir, &scope(), "sx-a")),
                ("reserve", move || {
                    reserve_at(&reserve_dir, &scope(), "sx-a", 0).map(|mut guard| guard.preserve())
                }),
            );
            assert_not_unreadable("forget", &forgot);
            assert_not_unreadable("reserve", &reserved);
            forgot.unwrap_or_else(|error| panic!("round {round}: forget failed: {error:#}"));
            reserved.unwrap_or_else(|error| panic!("round {round}: reserve failed: {error:#}"));
            // reserve-then-forget: gone; forget-then-reserve: a fresh intent.
            match registry::read_checked_at(&path, "sx-a").unwrap() {
                None => {}
                Some(record) => {
                    assert_eq!(record.state, "creating", "round {round}: {record:?}");
                    assert!(in_scope(&record, &scope()), "round {round}: {record:?}");
                }
            }
        }
    }

    #[test]
    fn concurrent_adopt_and_reserve_end_with_exactly_one_owner() {
        for round in 0..50 {
            let (_state, dir) = ledger();
            let path = dir.path().to_path_buf();
            let (adopt_dir, reserve_dir) = (path.clone(), path.clone());
            let (adopted, reserved) = race(
                ("adopt", move || {
                    adopt_at(&adopt_dir, &scope(), "sx-a", 0).map(|_| ())
                }),
                ("reserve", move || {
                    reserve_at(&reserve_dir, &other_owner(), "sx-a", 0)
                        .map(|mut guard| guard.preserve())
                }),
            );
            assert_not_unreadable("adopt", &adopted);
            assert_not_unreadable("reserve", &reserved);
            // Exactly one wins; the loser is refused as another controller's.
            assert!(
                adopted.is_ok() != reserved.is_ok(),
                "round {round}: adopt {adopted:?}, reserve {reserved:?}"
            );
            let record = registry::read_checked_at(&path, "sx-a")
                .unwrap()
                .unwrap_or_else(|| panic!("round {round}: no record survived"));
            if adopted.is_ok() {
                assert!(in_scope(&record, &scope()), "round {round}: {record:?}");
                assert_eq!(record.state, "ready", "round {round}: {record:?}");
                let error = reserved.unwrap_err();
                assert!(
                    format!("{error:#}").contains("another controller"),
                    "round {round}: {error:#}"
                );
            } else {
                assert!(
                    in_scope(&record, &other_owner()),
                    "round {round}: {record:?}"
                );
                assert_eq!(record.state, "creating", "round {round}: {record:?}");
                let Err(error) = adopted else {
                    unreachable!("round {round}: adopt must have lost here")
                };
                assert!(
                    format!("{error:#}").contains("another controller/account"),
                    "round {round}: {error:#}"
                );
            }
        }
    }

    #[test]
    fn owned_filters_to_this_scope_and_provider() {
        let (_state, dir) = ledger();
        reserve_at(dir.path(), &scope(), "sx-a", 0)
            .unwrap()
            .preserve();
        reserve_at(dir.path(), &other_owner(), "sx-b", 0)
            .unwrap()
            .preserve();
        let mut vercel = record(&scope(), "vx-a", "ready", 1);
        vercel.provider = "vercel".into();
        registry::write_at(dir.path(), &vercel).unwrap();
        let view = owned_at(dir.path(), &scope()).unwrap();
        let names: Vec<&str> = view.recorded.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["sx-a"]);
        assert!(view.unreadable.is_empty());
    }
}
