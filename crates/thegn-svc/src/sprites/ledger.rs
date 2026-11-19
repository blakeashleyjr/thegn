//! Sprites ownership ledger: the file store in [`crate::vps::registry`] with
//! `provider = "sprites"`, `owner` = this host's controller id and `account` =
//! the credential-free account fingerprint.
//!
//! `create` reserves a `creating` record **before** the POST and commits it as
//! `ready` after; `destroy` asks [`prove_owned`] first and never deletes a
//! sprite it cannot prove. Legacy sprites (created before the ledger) are
//! proven only by a thegn worktree location row naming them.
//!
//! Spine stub (b160 lane 1): the signatures are final; lane 2 replaces the
//! bodies. [`prove_owned`] accepts everything here, so the spine must never
//! merge to integration without lane 2.

use std::path::Path;

use anyhow::Result;

use crate::vps::registry::{IntentGuard, VpsRecord};

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

/// Reserve a `creating` intent for `name` (`cap` 0 = no cap). Dropping the
/// armed guard removes the intent.
pub fn reserve(scope: &OwnerScope, name: &str, cap: usize) -> Result<IntentGuard> {
    reserve_at(&crate::vps::registry::dir(), scope, name, cap)
}

/// [`reserve`] against an explicit ledger directory.
pub fn reserve_at(
    _dir: &Path,
    _scope: &OwnerScope,
    name: &str,
    _cap: usize,
) -> Result<IntentGuard> {
    Ok(IntentGuard::disarmed(name))
}

/// Mark `name` `ready` after a successful create.
pub fn commit(scope: &OwnerScope, name: &str) -> Result<()> {
    commit_at(&crate::vps::registry::dir(), scope, name)
}

/// [`commit`] against an explicit ledger directory.
pub fn commit_at(_dir: &Path, _scope: &OwnerScope, _name: &str) -> Result<()> {
    Ok(())
}

/// Prove `name` is ours before a destroy. `Err` = refuse the delete.
pub fn prove_owned(scope: &OwnerScope, name: &str, legacy_ids: &[String]) -> Result<Proof> {
    prove_owned_at(&crate::vps::registry::dir(), scope, name, legacy_ids)
}

/// [`prove_owned`] against an explicit ledger directory.
pub fn prove_owned_at(
    _dir: &Path,
    _scope: &OwnerScope,
    _name: &str,
    _legacy_ids: &[String],
) -> Result<Proof> {
    Ok(Proof::Ledger)
}

/// Drop `name`'s record after the sprite is gone.
pub fn forget(name: &str) {
    forget_at(&crate::vps::registry::dir(), name);
}

/// [`forget`] against an explicit ledger directory.
pub fn forget_at(_dir: &Path, _name: &str) {}

/// Every sprites record under `scope`.
pub fn owned(scope: &OwnerScope) -> Result<OwnedView> {
    owned_at(&crate::vps::registry::dir(), scope)
}

/// [`owned`] against an explicit ledger directory.
pub fn owned_at(_dir: &Path, _scope: &OwnerScope) -> Result<OwnedView> {
    Ok(OwnedView {
        recorded: Vec::new(),
        unreadable: Vec::new(),
    })
}
