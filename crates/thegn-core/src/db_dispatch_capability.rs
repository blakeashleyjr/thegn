//! Hashed, row-bound worker capabilities for dispatch mutations (THE-206).

use crate::db::Db;
use crate::db_dispatch::MAX_LEASE_TTL_SECS;
use crate::issue::AgentDispatchStatus;
use anyhow::Result;
use rusqlite::OptionalExtension as _;
use sha2::{Digest as _, Sha256};
use std::fmt::Write as _;

/// The only worker operations admitted by a dispatch capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CapabilityVerb {
    Report,
    Note,
}

impl CapabilityVerb {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Report => "report",
            Self::Note => "note",
        }
    }
}

/// A typed refusal from capability minting or verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityRefusal {
    Malformed,
    UnknownToken,
    Revoked,
    Expired,
    WrongRow,
    VerbNotGranted,
    RowMissing,
    RowNotLive,
    MissingSession,
    SessionChanged,
    StaleRunGen,
    EmptyVerbs,
    InvalidTtl,
    InvalidStoredVerbs,
}

impl CapabilityRefusal {
    pub fn kind(self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::UnknownToken => "unknown_token",
            Self::Revoked => "revoked",
            Self::Expired => "expired",
            Self::WrongRow => "wrong_row",
            Self::VerbNotGranted => "verb_not_granted",
            Self::RowMissing => "row_missing",
            Self::RowNotLive => "row_not_live",
            Self::MissingSession => "missing_session",
            Self::SessionChanged => "session_changed",
            Self::StaleRunGen => "stale_run_gen",
            Self::EmptyVerbs => "empty_verbs",
            Self::InvalidTtl => "invalid_ttl",
            Self::InvalidStoredVerbs => "invalid_stored_verbs",
        }
    }
}

impl std::fmt::Display for CapabilityRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.kind())
    }
}

impl std::error::Error for CapabilityRefusal {}

/// Capability identity and timing returned by mint and successful verify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityGrant {
    pub dispatch_id: i64,
    pub session_id: String,
    pub run_gen: i64,
    pub verbs: Vec<CapabilityVerb>,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
}

/// Caller-supplied capability scope and lifetime.
pub struct CapabilityMint<'a> {
    pub dispatch_id: i64,
    pub verbs: &'a [CapabilityVerb],
    pub ttl_ms: i64,
    pub now_ms: i64,
}

pub const MAX_CAPABILITY_TTL_MS: i64 = MAX_LEASE_TTL_SECS * 1000;

/// Hash the 32-byte secret to the lowercase hexadecimal storage key.
pub fn capability_token_digest(secret: &[u8; 32]) -> String {
    let digest = Sha256::digest(secret);
    let mut text = String::with_capacity(64);
    for byte in digest {
        write!(&mut text, "{byte:02x}").expect("writing to String cannot fail");
    }
    text
}

/// Parse the lowercase 64-character wire representation of a secret.
pub fn parse_capability_token(text: &str) -> std::result::Result<[u8; 32], CapabilityRefusal> {
    if text.len() != 64
        || !text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(CapabilityRefusal::Malformed);
    }
    let mut secret = [0_u8; 32];
    for (index, pair) in text.as_bytes().chunks_exact(2).enumerate() {
        let high = hex_nibble(pair[0]).ok_or(CapabilityRefusal::Malformed)?;
        let low = hex_nibble(pair[1]).ok_or(CapabilityRefusal::Malformed)?;
        secret[index] = (high << 4) | low;
    }
    Ok(secret)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn encode_verbs(
    verbs: &[CapabilityVerb],
) -> std::result::Result<(String, Vec<CapabilityVerb>), CapabilityRefusal> {
    if verbs.is_empty() {
        return Err(CapabilityRefusal::EmptyVerbs);
    }
    let mut canonical = verbs.to_vec();
    canonical.sort_by_key(|verb| verb.as_str());
    canonical.dedup();
    Ok((
        canonical
            .iter()
            .map(|verb| verb.as_str())
            .collect::<Vec<_>>()
            .join(","),
        canonical,
    ))
}

fn decode_verbs(raw: &str) -> std::result::Result<Vec<CapabilityVerb>, CapabilityRefusal> {
    let mut verbs = Vec::new();
    for value in raw.split(',') {
        let verb = match value {
            "report" => CapabilityVerb::Report,
            "note" => CapabilityVerb::Note,
            _ => return Err(CapabilityRefusal::InvalidStoredVerbs),
        };
        if verbs.contains(&verb) {
            return Err(CapabilityRefusal::InvalidStoredVerbs);
        }
        verbs.push(verb);
    }
    if verbs.is_empty()
        || verbs
            .windows(2)
            .any(|pair| pair[0].as_str() >= pair[1].as_str())
    {
        return Err(CapabilityRefusal::InvalidStoredVerbs);
    }
    Ok(verbs)
}

impl Db {
    /// Persist only the digest, bound to the currently live dispatch run.
    pub fn mint_dispatch_capability(
        &self,
        secret: &[u8; 32],
        mint: &CapabilityMint<'_>,
    ) -> Result<std::result::Result<CapabilityGrant, CapabilityRefusal>> {
        let (stored_verbs, verbs) = match encode_verbs(mint.verbs) {
            Ok(encoded) => encoded,
            Err(refusal) => return Ok(Err(refusal)),
        };
        if !(1..=MAX_CAPABILITY_TTL_MS).contains(&mint.ttl_ms) {
            return Ok(Err(CapabilityRefusal::InvalidTtl));
        }
        let conn = self.conn();
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<std::result::Result<CapabilityGrant, CapabilityRefusal>> {
            let row = conn
                .query_row(
                    "SELECT status, session_id, run_gen FROM agent_dispatches WHERE id=?1",
                    [mint.dispatch_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    },
                )
                .optional()?;
            let Some((status, session, run_gen)) = row else {
                return Ok(Err(CapabilityRefusal::RowMissing));
            };
            if !AgentDispatchStatus::parse(&status).is_active() {
                return Ok(Err(CapabilityRefusal::RowNotLive));
            }
            let Some(session_id) = session.filter(|session_id| !session_id.is_empty()) else {
                return Ok(Err(CapabilityRefusal::MissingSession));
            };
            let expires_at_ms = mint.now_ms.saturating_add(mint.ttl_ms);
            let grant = CapabilityGrant {
                dispatch_id: mint.dispatch_id,
                session_id: session_id.clone(),
                run_gen,
                verbs,
                created_at_ms: mint.now_ms,
                expires_at_ms,
            };
            conn.execute(
                "INSERT INTO dispatch_capabilities
             (token_sha256, dispatch_id, session_id, run_gen, verbs, created_at_ms, expires_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    capability_token_digest(secret),
                    mint.dispatch_id,
                    session_id,
                    run_gen,
                    stored_verbs,
                    mint.now_ms,
                    expires_at_ms
                ],
            )?;
            Ok(Ok(grant))
        })();
        match result {
            Ok(grant) => {
                conn.execute_batch("COMMIT")?;
                Ok(grant)
            }
            Err(error) => {
                conn.execute_batch("ROLLBACK")?;
                Err(error)
            }
        }
    }

    /// Verify one capability and its dispatch-row binding in a single read.
    pub fn verify_dispatch_capability(
        &self,
        secret: &[u8; 32],
        expected_dispatch_id: i64,
        verb: CapabilityVerb,
        now_ms: i64,
    ) -> Result<std::result::Result<CapabilityGrant, CapabilityRefusal>> {
        let row = self
            .conn()
            .query_row(
                "SELECT c.dispatch_id, c.session_id, c.run_gen, c.verbs, c.created_at_ms,
                    c.expires_at_ms, c.revoked_at_ms, d.status, d.session_id, d.run_gen
             FROM dispatch_capabilities c LEFT JOIN agent_dispatches d ON d.id=c.dispatch_id
             WHERE c.token_sha256=?1",
                [capability_token_digest(secret)],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, Option<i64>>(6)?,
                        row.get::<_, Option<String>>(7)?,
                        row.get::<_, Option<String>>(8)?,
                        row.get::<_, Option<i64>>(9)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            dispatch_id,
            session_id,
            run_gen,
            raw_verbs,
            created_at_ms,
            expires_at_ms,
            revoked_at_ms,
            status,
            current_session,
            current_gen,
        )) = row
        else {
            return Ok(Err(CapabilityRefusal::UnknownToken));
        };
        if revoked_at_ms.is_some() {
            return Ok(Err(CapabilityRefusal::Revoked));
        }
        if now_ms >= expires_at_ms {
            return Ok(Err(CapabilityRefusal::Expired));
        }
        if dispatch_id != expected_dispatch_id {
            return Ok(Err(CapabilityRefusal::WrongRow));
        }
        let verbs = match decode_verbs(&raw_verbs) {
            Ok(verbs) => verbs,
            Err(refusal) => return Ok(Err(refusal)),
        };
        if !verbs.contains(&verb) {
            return Ok(Err(CapabilityRefusal::VerbNotGranted));
        }
        let Some(status) = status else {
            return Ok(Err(CapabilityRefusal::RowMissing));
        };
        if !AgentDispatchStatus::parse(&status).is_active() {
            return Ok(Err(CapabilityRefusal::RowNotLive));
        }
        let Some(current_session) = current_session else {
            return Ok(Err(CapabilityRefusal::SessionChanged));
        };
        if current_session != session_id {
            return Ok(Err(CapabilityRefusal::SessionChanged));
        }
        if current_gen != Some(run_gen) {
            return Ok(Err(CapabilityRefusal::StaleRunGen));
        }
        Ok(Ok(CapabilityGrant {
            dispatch_id,
            session_id,
            run_gen,
            verbs,
            created_at_ms,
            expires_at_ms,
        }))
    }

    /// Revoke every currently usable capability for one dispatch row.
    pub fn revoke_dispatch_capabilities(&self, dispatch_id: i64, now_ms: i64) -> Result<usize> {
        Ok(self.conn().execute(
            "UPDATE dispatch_capabilities SET revoked_at_ms=?1 WHERE dispatch_id=?2 AND revoked_at_ms IS NULL",
            rusqlite::params![now_ms, dispatch_id],
        )?)
    }
}

#[cfg(test)]
#[path = "db_dispatch_capability_tests.rs"]
mod tests;
