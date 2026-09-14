//! One admission path for PR queue, CI autofix and durable review handoff.
//! Evidence is fresh, location-bound and rechecked after preparation, before
//! consuming an attempt/claim. This is not a global credential freeze: an
//! external account change after the last check remains THE-541 territory.

use thegn_core::db::Db;
use thegn_core::forge::{
    Forge,
    authorship::{GithubRepository, PrAuthorship},
    model::PrStatus,
};
use thegn_core::remote::GitLoc;
use thegn_core::store::WorkspaceStore;

#[derive(Debug, Clone)]
pub(crate) struct Permit {
    worktree: std::path::PathBuf,
    origin: String,
    proof: PrAuthorship,
}

impl Permit {
    pub(crate) fn matches_review(&self, repository: &str, number: u64) -> bool {
        self.proof.number == number
            && format!(
                "{}/{}",
                self.proof.repository.owner, self.proof.repository.name
            )
            .eq_ignore_ascii_case(repository)
    }
}

pub(crate) const HELD: &str = "own-PR automation held: current author, account, repository and head must be verified by the selected forge";

pub(crate) fn acquire(
    own_only: bool,
    db: &Db,
    forge: &dyn Forge,
    loc: &GitLoc,
    selected_forge: &str,
    expected_number: u64,
    pr: &PrStatus,
) -> Result<Option<Permit>, &'static str> {
    if !own_only {
        return Ok(None);
    }
    if expected_number == 0
        || pr.number != expected_number
        || selected_forge != forge.id()
        || !forge.caps().pr_authorship
    {
        return Err(HELD);
    }
    let worktree = local_execution_location(db, loc)?;
    let origin = loc.git_out(&["remote", "get-url", "origin"]).ok_or(HELD)?;
    let repository = GithubRepository::from_origin(&origin).ok_or(HELD)?;
    let proof = forge
        .pr_authorship(loc, expected_number)
        .map_err(|_| HELD)?;
    if !proof.authorizes(selected_forge, &repository, pr)
        || loc.git_out(&["remote", "get-url", "origin"]).as_deref() != Some(origin.as_str())
        || loc.git_out(&["rev-parse", "HEAD"]).as_deref() != Some(pr.head_ref_oid.as_str())
    {
        return Err(HELD);
    }
    local_execution_location(db, loc)?;
    Ok(Some(Permit {
        worktree,
        origin,
        proof,
    }))
}

/// The agent runner can use a local shell even for a remote control-plane
/// location. Until that execution authority is verified, own-only automation
/// must not authorize against a remote/provider proof or silently read a local
/// checkout with the same path. Database failures and malformed metadata hold.
fn local_execution_location(db: &Db, loc: &GitLoc) -> Result<std::path::PathBuf, &'static str> {
    const LOCATION_HELD: &str = "own-PR automation held: local agent execution authority must be verified; remote/provider handoff needs an explicit verified route";
    let GitLoc::Local(path) = loc else {
        return Err(LOCATION_HELD);
    };
    let location = db
        .location_for(&path.to_string_lossy())
        .map_err(|_| LOCATION_HELD)?;
    if location
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty() && value.trim() != "local")
    {
        return Err(LOCATION_HELD);
    }
    Ok(path.clone())
}

/// Fresh proof must match the preparation's authority and account generation.
/// Only the existing verified-push review path may admit an advanced head.
pub(crate) fn revalidate(
    permit: Option<&Permit>,
    db: &Db,
    forge: &dyn Forge,
    loc: &GitLoc,
    pr: &PrStatus,
    verified_head_advanced: bool,
) -> Result<(), &'static str> {
    let Some(before) = permit else {
        return Ok(());
    };
    let after = acquire(
        true,
        db,
        forge,
        loc,
        &before.proof.provider,
        before.proof.number,
        pr,
    )?
    .ok_or(HELD)?;
    if before.worktree != after.worktree
        || before.origin != after.origin
        || !before.proof.same_context(&after.proof)
        || (!verified_head_advanced && before.proof.head != after.proof.head)
    {
        return Err(HELD);
    }
    Ok(())
}

#[cfg(test)]
#[path = "pr_authorship_tests.rs"]
pub(crate) mod tests;
