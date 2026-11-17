//! `thegn fleet cost`'s Vercel section: per-sandbox current-session accrual
//! and snapshot storage for this controller's sandboxes.
//!
//! Every read here is a GET that never resumes: pricing a stopped sandbox must
//! not buy compute. A listing gap, a failed read or an unreadable ledger makes
//! the report incomplete (`false`), never an error.

use std::collections::BTreeMap;

use anyhow::Result;
use thegn_core::vercel::usage::{self as core_usage, PRO_IAD1};

use super::VercelProvider;

/// One sandbox's cost row (an estimate; current session only).
#[derive(Debug, Clone, PartialEq)]
pub struct UsageRow {
    pub name: String,
    pub status: String,
    pub session: Option<thegn_core::vercel::usage::Accrual>,
    pub snapshots_usd_month: f64,
    pub snapshot_bytes: u64,
}

/// The status text for a name the ledger holds that Vercel no longer has.
const GONE: &str = "gone";
/// The status text when the sandbox could not be read.
const UNKNOWN: &str = "unknown";

impl VercelProvider {
    /// The cost rows plus whether the listing was complete.
    ///
    /// Names are the union of this account's ledger records and the sandboxes
    /// Vercel lists as tagged for this controller. Rows are sorted by name.
    pub async fn usage_report(&self) -> Result<(Vec<UsageRow>, bool)> {
        let mut complete = true;
        // name -> status already known from the listing.
        let mut names: BTreeMap<String, Option<String>> = BTreeMap::new();
        match self.controller().await {
            Ok(controller) => match self.listed(&controller).await {
                crate::vps::Inventory::Complete(rows) => {
                    for view in rows {
                        names.insert(view.name.clone(), Some(super::status_text(view.status)));
                    }
                }
                crate::vps::Inventory::Partial { rows, .. } => {
                    complete = false;
                    for view in rows {
                        names.insert(view.name.clone(), Some(super::status_text(view.status)));
                    }
                }
                crate::vps::Inventory::Unknown(reason) => {
                    complete = false;
                    tracing::debug!("vercel usage: listing unavailable: {reason}");
                }
            },
            Err(error) => {
                complete = false;
                tracing::debug!("vercel usage: controller id unavailable: {error:#}");
            }
        }
        match self.account() {
            Ok(account) => match self.ledger_vercel_names(&account).await {
                Ok(ledger) => {
                    for name in ledger {
                        names.entry(name).or_insert(None);
                    }
                }
                Err(error) => {
                    complete = false;
                    tracing::debug!("vercel usage: ledger unreadable: {error:#}");
                }
            },
            Err(error) => {
                complete = false;
                tracing::debug!("vercel usage: account fingerprint unavailable: {error:#}");
            }
        }

        let now = super::now_ms();
        let mut rows = Vec::with_capacity(names.len());
        for (name, listed_status) in names {
            if super::checked_name(&name).is_err() {
                complete = false;
                continue;
            }
            let status = match listed_status {
                Some(status) => status,
                // A ledger-only name: one GET, never a resume.
                None => match self.api().get(&name, false).await {
                    Ok(Some(view)) => super::status_text(view.status),
                    Ok(None) => GONE.to_string(),
                    Err(error) => {
                        complete = false;
                        tracing::debug!(sandbox = %name, "vercel usage: read failed: {error:#}");
                        UNKNOWN.to_string()
                    }
                },
            };
            if status == GONE {
                rows.push(UsageRow {
                    name,
                    status,
                    session: None,
                    snapshots_usd_month: 0.0,
                    snapshot_bytes: 0,
                });
                continue;
            }
            let session = match self.api().usage(&name).await {
                Ok(Some(usage)) => core_usage::session_accrual(&usage, now, &PRO_IAD1),
                Ok(None) => None,
                Err(error) => {
                    complete = false;
                    tracing::debug!(sandbox = %name, "vercel usage: session read failed: {error:#}");
                    None
                }
            };
            let (snapshots_usd_month, snapshot_bytes) = match self.api().snapshot_usage(&name).await
            {
                Ok(snapshots) => (
                    core_usage::snapshot_month_usd(&snapshots, &PRO_IAD1),
                    snapshots
                        .iter()
                        .filter(|s| s.status.trim().eq_ignore_ascii_case("created"))
                        .fold(0u64, |sum, s| sum.saturating_add(s.size_bytes)),
                ),
                Err(error) => {
                    complete = false;
                    tracing::debug!(sandbox = %name, "vercel usage: snapshot read failed: {error:#}");
                    (0.0, 0)
                }
            };
            rows.push(UsageRow {
                name,
                status,
                session,
                snapshots_usd_month,
                snapshot_bytes,
            });
        }
        Ok((rows, complete))
    }
}
