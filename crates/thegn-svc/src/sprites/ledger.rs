//! Sprites ownership ledger: the file store in [`crate::vps::registry`] with
//! `provider = "sprites"`, `owner` = this host's controller id and `account` =
//! the credential-free account fingerprint.
//!
//! `create` reserves a `creating` record **before** the POST and commits it as
//! `ready` after; `destroy` asks [`prove_owned`] first and never deletes a
//! sprite it cannot prove. Legacy sprites (created before the ledger) are
//! proven only by a thegn worktree location row naming them (the explicit
//! `legacy_ids`, else this host's worktree rows in the state DB), and a legacy
//! proof adopts the sprite into the ledger once.
//!
//! The registry directory is shared with VPS/Fly/Vercel records: every read
//! here filters on `provider == "sprites"`, and nothing here removes or
//! rewrites another provider's record.

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
    /// A thegn worktree location row names it (adopted into the ledger).
    Legacy,
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

/// Refuse a name another provider's record already holds (the registry is
/// shared; its own reserve checks owner and account only).
fn refuse_foreign_provider(dir: &Path, name: &str) -> Result<()> {
    if let Some(existing) = registry::read_checked_at(dir, name)?
        && existing.provider != LEDGER_PROVIDER
    {
        bail!(
            "sprites: the name {name} is recorded by another provider ({}); refusing to reuse it",
            existing.provider
        );
    }
    Ok(())
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
    refuse_foreign_provider(dir, name)?;
    let cap = if cap == 0 { usize::MAX } else { cap };
    let intent = record(scope, name, "creating", thegn_core::util::now());
    registry::reserve_at(dir, &intent, cap, ReserveScope::Provider(LEDGER_PROVIDER))
        .map_err(|error| reserve_error(name, error))?;
    Ok(IntentGuard::at(dir, name))
}

/// Mark `name` `ready` after a successful create.
pub fn commit(scope: &OwnerScope, name: &str) -> Result<()> {
    commit_at(&registry::dir(), scope, name)
}

/// [`commit`] against an explicit ledger directory. Keeps the intent's
/// `created_at`; a record that vanished in between is rewritten (the sprite
/// exists and is ours); one held by another scope or provider is refused.
pub fn commit_at(dir: &Path, scope: &OwnerScope, name: &str) -> Result<()> {
    let created_at = match registry::read_checked_at(dir, name)? {
        Some(existing) if in_scope(&existing, scope) => existing.created_at,
        Some(_) => bail!(
            "sprites: {name} is recorded by another controller/account; refusing to mark it ready"
        ),
        None => thegn_core::util::now(),
    };
    registry::write_at(dir, &record(scope, name, "ready", created_at))
        .with_context(|| format!("sprites: cannot mark {name} ready in the ledger"))
}

/// Prove `name` is ours before a destroy. `Err` = refuse the delete.
pub fn prove_owned(scope: &OwnerScope, name: &str, legacy_ids: &[String]) -> Result<Proof> {
    prove_owned_at(&registry::dir(), scope, name, legacy_ids)
}

/// [`prove_owned`] against an explicit ledger directory.
///
/// - a sprites record under `scope` → [`Proof::Ledger`];
/// - any other record (another controller, account or provider) → refused;
/// - no record, and (for the host's own ledger directory only) the one-time
///   [`adopt_legacy_once_at`] adopted it from a pre-ledger worktree row, or
///   `name` is in `legacy_ids` (a `ready` record is written) →
///   [`Proof::Legacy`];
/// - otherwise refused. An unreadable record or ledger refuses too.
pub fn prove_owned_at(
    dir: &Path,
    scope: &OwnerScope,
    name: &str,
    legacy_ids: &[String],
) -> Result<Proof> {
    registry::checked_name(name).context("sprites: invalid sprite name")?;
    if scope.owner.is_empty() || scope.account.is_empty() {
        bail!("sprites: no owner scope; refusing to treat {name} as ours");
    }
    let existing = registry::read_checked_at(dir, name)
        .with_context(|| format!("sprites: cannot read the ledger record for {name}"))?;
    match existing {
        Some(existing) if in_scope(&existing, scope) => return Ok(Proof::Ledger),
        Some(existing) if existing.provider != LEDGER_PROVIDER => bail!(
            "sprites: {name} is recorded by another provider ({}); refusing to treat it as a sprite of ours",
            existing.provider
        ),
        Some(_) => bail!(
            "sprites: {name} is recorded by another controller/account; refusing to treat it as ours"
        ),
        None => {
            // The state DB backs only the host's own ledger, and only the
            // worktree rows that existed at the first ledger use (adopted
            // once): a row written later (user config can name any sprite)
            // proves nothing. An explicit ledger directory (tests, a foreign
            // root) is proven by `legacy_ids` alone.
            if dir == registry::state_dir().as_path() {
                adopt_legacy_once_at(dir, scope).context(
                    "sprites: cannot adopt pre-ledger sprites from worktree rows; refusing",
                )?;
                if let Some(r) = registry::read_checked_at(dir, name)?
                    && in_scope(&r, scope)
                {
                    return Ok(Proof::Legacy);
                }
            }
        }
    }
    let legacy = legacy_ids.iter().any(|id| id == name);
    if !legacy {
        bail!(
            "sprites: thegn has no record of creating {name}; refusing to delete it. Delete it in the Sprites dashboard if it is yours."
        );
    }
    // Adopt once, under the reserve lock (so a concurrent foreign record wins).
    let adopted = record(scope, name, "ready", thegn_core::util::now());
    registry::reserve_at(
        dir,
        &adopted,
        usize::MAX,
        ReserveScope::Provider(LEDGER_PROVIDER),
    )
    .map_err(|error| reserve_error(name, error))
    .with_context(|| format!("sprites: cannot adopt legacy sprite {name} into the ledger"))?;
    Ok(Proof::Legacy)
}

/// Adopt every sprite this host's worktree rows name into the ledger, once
/// per ledger directory (a marker file records it). Rows written after the
/// first adoption prove nothing.
pub fn adopt_legacy_once(scope: &OwnerScope) -> Result<()> {
    adopt_legacy_once_at(&registry::dir(), scope)
}

/// [`adopt_legacy_once`] against an explicit ledger directory. A store error
/// leaves the marker unwritten (the next call retries).
pub fn adopt_legacy_once_at(dir: &Path, scope: &OwnerScope) -> Result<()> {
    if std::fs::symlink_metadata(dir.join(registry::SPRITES_LEGACY_MARKER)).is_ok() {
        return Ok(());
    }
    let ids = legacy_ids_from_db()?;
    for id in ids {
        if registry::read_checked_at(dir, &id)?.is_some() {
            continue;
        }
        match registry::reserve_at(
            dir,
            &record(scope, &id, "ready", thegn_core::util::now()),
            usize::MAX,
            ReserveScope::Provider(LEDGER_PROVIDER),
        ) {
            Ok(())
            | Err(ReserveError::OwnedByOther { .. })
            | Err(ReserveError::OtherAccount { .. })
            | Err(ReserveError::AtCap { .. }) => {}
            Err(ReserveError::Store(e)) => return Err(e),
        }
    }
    registry::open_lock_at(dir, registry::SPRITES_LEGACY_MARKER).map(drop)
}

/// The sprite names this host's worktree location rows name (the legacy
/// ownership proof). Blocking: opens the state DB.
pub fn legacy_ids_from_db() -> Result<Vec<String>> {
    use thegn_core::store::WorkspaceStore;
    let db = thegn_core::db::Db::open().context("open the state database")?;
    let rows = db.worktrees().context("read worktree locations")?;
    let mut ids: Vec<String> = rows
        .iter()
        .filter_map(|row| sprite_id_from_location(&row.location))
        .collect();
    ids.sort();
    ids.dedup();
    Ok(ids)
}

/// The sprite a worktree `location` blob names, if any: a v1 provider blob
/// whose control prefix is the `sprite-exec <name>` self-bridge (or the vendor
/// `sprite exec -s <name>`), or a v2 provider blob for `builtin:sprites`.
pub fn sprite_id_from_location(location: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(location.trim()).ok()?;
    let valid = |id: &str| registry::checked_name(id).is_ok().then(|| id.to_string());
    if value.get("v").and_then(serde_json::Value::as_u64) == Some(2) {
        if value.get("kind").and_then(serde_json::Value::as_str) != Some("provider")
            || value.get("ref").and_then(serde_json::Value::as_str) != Some("builtin:sprites")
        {
            return None;
        }
        return valid(value.get("id")?.as_str()?);
    }
    let prefix: Vec<&str> = value
        .get("control_prefix")?
        .as_array()?
        .iter()
        .map(|part| part.as_str())
        .collect::<Option<_>>()?;
    if let Some(at) = prefix.iter().position(|part| *part == "sprite-exec") {
        return valid(prefix.get(at + 1).copied()?);
    }
    let vendor = prefix
        .first()
        .is_some_and(|exe| Path::new(exe).file_name().is_some_and(|f| f == "sprite"));
    if vendor && prefix.get(1) == Some(&"exec") {
        let at = prefix.iter().position(|part| *part == "-s")?;
        return valid(prefix.get(at + 1).copied()?);
    }
    None
}

/// Drop `name`'s record after the sprite is gone.
pub fn forget(name: &str) {
    forget_at(&registry::dir(), name);
}

/// [`forget`] against an explicit ledger directory. Only a sprites record is
/// removed: another provider's record of the same name, or an unreadable one,
/// is left alone.
pub fn forget_at(dir: &Path, name: &str) {
    match registry::read_checked_at(dir, name) {
        Ok(Some(existing)) if existing.provider == LEDGER_PROVIDER => {
            registry::remove_at(dir, name);
        }
        Ok(Some(existing)) => {
            tracing::warn!(target: "thegn::sprites", name, provider = %existing.provider, "sprites: the ledger name belongs to another provider; left alone");
        }
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(target: "thegn::sprites", name, error = %format!("{error:#}"), "sprites: ledger record unreadable; left alone");
        }
    }
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
    fn prove_owned_legacy_adopts_a_ready_record() {
        let (_state, dir) = ledger();
        let legacy = vec!["sx-old".to_string()];
        assert_eq!(
            prove_owned_at(dir.path(), &scope(), "sx-old", &legacy).unwrap(),
            Proof::Legacy
        );
        let adopted = registry::read_checked_at(dir.path(), "sx-old")
            .unwrap()
            .unwrap();
        assert_eq!(adopted.state, "ready");
        assert_eq!(adopted.provider, "sprites");
        assert_eq!(adopted.owner, "ctl-a");
        assert_eq!(adopted.account, "acct1-a");
        // Adopted once: the next proof is the ledger's.
        assert_eq!(
            prove_owned_at(dir.path(), &scope(), "sx-old", &[]).unwrap(),
            Proof::Ledger
        );
    }

    #[test]
    fn prove_owned_legacy_from_a_worktree_location_row() {
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
        // An explicit (non-host) ledger directory never consults the DB.
        assert!(prove_owned_at(other.path(), &scope(), "sx-row", &[]).is_err());
        // The host's own ledger (isolated) does.
        let host = registry::state_dir();
        assert_eq!(
            prove_owned_at(&host, &scope(), "sx-row", &[]).unwrap(),
            Proof::Legacy
        );
        assert!(prove_owned_at(&host, &scope(), "sx-other", &[]).is_err());
        // A row written after the one-time adoption proves nothing.
        let late = serde_json::json!({
            "control_prefix": ["/nix/store/x/bin/thegn", "sprite-exec", "sx-late", "--"],
            "path": "/workspace",
        })
        .to_string();
        let db = thegn_core::db::Db::open().unwrap();
        db.put_worktree("tab", "/repo", "/wt/late", "feat", Some(&late), None)
            .unwrap();
        drop(db);
        assert!(prove_owned_at(&host, &scope(), "sx-late", &[]).is_err());
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
        // Even a legacy id never overrides another scope's record.
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
            "sprites: thegn has no record of creating sx-none; refusing to delete it. Delete it in the Sprites dashboard if it is yours."
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
        forget_at(dir.path(), "sx-a");
        forget_at(dir.path(), "vx-a");
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

    #[test]
    fn sprite_id_from_location_reads_v1_and_v2_blobs() {
        let v1 =
            r#"{"control_prefix":["/bin/thegn","sprite-exec","sx-1","--"],"path":"/workspace"}"#;
        assert_eq!(sprite_id_from_location(v1).as_deref(), Some("sx-1"));
        let vendor = r#"{"control_prefix":["sprite","exec","-s","sx-2","--"],"path":"/workspace"}"#;
        assert_eq!(sprite_id_from_location(vendor).as_deref(), Some("sx-2"));
        let v2 = r#"{"v":2,"kind":"provider","ref":"builtin:sprites","env":"s","id":"sx-3","path":"/w"}"#;
        assert_eq!(sprite_id_from_location(v2).as_deref(), Some("sx-3"));
        for other in [
            "",
            "local",
            r#"{"host":"box","path":"/w"}"#,
            r#"{"control_prefix":["/bin/thegn","vercel-exec","vx-1","--"],"path":"/w"}"#,
            r#"{"v":2,"kind":"provider","ref":"builtin:vercel","env":"v","id":"vx","path":"/w"}"#,
            r#"{"control_prefix":["/bin/thegn","sprite-exec","../evil","--"],"path":"/w"}"#,
        ] {
            assert_eq!(sprite_id_from_location(other), None, "{other}");
        }
    }
}
