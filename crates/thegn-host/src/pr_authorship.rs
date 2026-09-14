//! One admission path for PR queue, CI autofix and durable review handoff.
//! Evidence is fresh, location-bound and rechecked after preparation, before
//! consuming an attempt/claim. This is not a global credential freeze: an
//! external account change after the last check remains THE-541 territory.

use thegn_core::forge::{
    Forge,
    authorship::{GithubRepository, PrAuthorship},
    model::PrStatus,
};
use thegn_core::remote::GitLoc;

#[derive(Debug, Clone)]
pub(crate) struct Permit {
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
    forge: &dyn Forge,
    loc: &GitLoc,
    selected_forge: &str,
    pr: &PrStatus,
) -> Result<Option<Permit>, &'static str> {
    if !own_only {
        return Ok(None);
    }
    if selected_forge != forge.id() || !forge.caps().pr_authorship {
        return Err(HELD);
    }
    let origin = loc.git_out(&["remote", "get-url", "origin"]).ok_or(HELD)?;
    let repository = GithubRepository::from_origin(&origin).ok_or(HELD)?;
    let proof = forge.pr_authorship(loc, pr.number).map_err(|_| HELD)?;
    if !proof.authorizes(selected_forge, &repository, pr)
        || loc.git_out(&["remote", "get-url", "origin"]).as_deref() != Some(origin.as_str())
        || loc.git_out(&["rev-parse", "HEAD"]).as_deref() != Some(pr.head_ref_oid.as_str())
    {
        return Err(HELD);
    }
    Ok(Some(Permit { origin, proof }))
}

/// Fresh proof must match the preparation's authority and account generation.
/// Only the existing verified-push review path may admit an advanced head.
pub(crate) fn revalidate(
    permit: Option<&Permit>,
    forge: &dyn Forge,
    loc: &GitLoc,
    pr: &PrStatus,
    verified_head_advanced: bool,
) -> Result<(), &'static str> {
    let Some(before) = permit else {
        return Ok(());
    };
    let after = acquire(true, forge, loc, &before.proof.provider, pr)?.ok_or(HELD)?;
    if before.origin != after.origin
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
