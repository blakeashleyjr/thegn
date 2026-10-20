use super::{
    CapabilityMint, CapabilityRefusal as Refusal, CapabilityVerb as Verb, MAX_CAPABILITY_TTL_MS,
    capability_token_digest, parse_capability_token,
};
use crate::db::Db;
use crate::issue::{AgentDispatchStatus, NewDispatch};
use crate::store::NotificationStore;

const REPORT: [Verb; 1] = [Verb::Report];
const BOTH: [Verb; 2] = [Verb::Report, Verb::Note];

fn row(db: &Db, issue: &str, session: &str) -> i64 {
    let id = db
        .put_agent_dispatch(NewDispatch {
            session_id: Some(session),
            ..NewDispatch::new(issue, "/wt/capability", "worker")
        })
        .unwrap();
    db.update_dispatch_status(id, AgentDispatchStatus::Running)
        .unwrap();
    id
}

fn mint(db: &Db, id: i64, secret: &[u8; 32], verbs: &[Verb]) {
    let grant = db
        .mint_dispatch_capability(
            secret,
            &CapabilityMint {
                dispatch_id: id,
                verbs,
                ttl_ms: 500,
                now_ms: 100,
            },
        )
        .unwrap()
        .unwrap();
    assert_eq!(grant.dispatch_id, id);
}

fn refusal(db: &Db, secret: &[u8; 32], id: i64, verb: Verb, now_ms: i64, expected: Refusal) {
    assert_eq!(
        db.verify_dispatch_capability(secret, id, verb, now_ms)
            .unwrap(),
        Err(expected)
    );
}

#[test]
fn capability_valid_token_verifies_for_own_row_and_granted_verb() {
    let db = Db::open_memory().unwrap();
    let id = row(&db, "linear:CAP-1", "session-a");
    let secret = [0x31; 32];
    mint(&db, id, &secret, &BOTH);
    let verified = db
        .verify_dispatch_capability(&secret, id, Verb::Note, 101)
        .unwrap()
        .unwrap();
    assert_eq!(verified.dispatch_id, id);
    assert_eq!(verified.session_id, "session-a");
    assert_eq!(verified.run_gen, 0);
}

#[test]
fn capability_for_row_a_is_refused_on_row_b() {
    let db = Db::open_memory().unwrap();
    let first = row(&db, "linear:CAP-2", "session-a");
    let second = row(&db, "linear:CAP-3", "session-b");
    let secret = [0x32; 32];
    mint(&db, first, &secret, &REPORT);
    refusal(&db, &secret, second, Verb::Report, 101, Refusal::WrongRow);
}

#[test]
fn capability_stale_run_gen_is_refused() {
    let db = Db::open_memory().unwrap();
    let id = row(&db, "linear:CAP-4", "session-a");
    let secret = [0x33; 32];
    mint(&db, id, &secret, &REPORT);
    db.stamp_dispatch_run(id, "session-a", "relaunch.md")
        .unwrap();
    refusal(&db, &secret, id, Verb::Report, 101, Refusal::StaleRunGen);
}

#[test]
fn capability_changed_session_is_refused() {
    let db = Db::open_memory().unwrap();
    let id = row(&db, "linear:CAP-5", "session-a");
    let secret = [0x34; 32];
    mint(&db, id, &secret, &REPORT);
    db.stamp_dispatch_run(id, "session-b", "relaunch.md")
        .unwrap();
    refusal(&db, &secret, id, Verb::Report, 101, Refusal::SessionChanged);
}

#[test]
fn capability_terminal_row_is_refused() {
    for status in [
        AgentDispatchStatus::Done,
        AgentDispatchStatus::Failed,
        AgentDispatchStatus::Unknown,
    ] {
        let db = Db::open_memory().unwrap();
        let id = row(&db, "linear:CAP-6", "session-a");
        let secret = [0x35; 32];
        mint(&db, id, &secret, &REPORT);
        db.update_dispatch_status(id, status).unwrap();
        refusal(&db, &secret, id, Verb::Report, 101, Refusal::RowNotLive);
    }
}

#[test]
fn capability_expired_and_revoked_are_refused() {
    let db = Db::open_memory().unwrap();
    let id = row(&db, "linear:CAP-7", "session-a");
    let expired = [0x36; 32];
    mint(&db, id, &expired, &REPORT);
    refusal(&db, &expired, id, Verb::Report, 600, Refusal::Expired);
    let revoked = [0x37; 32];
    mint(&db, id, &revoked, &REPORT);
    assert_eq!(db.revoke_dispatch_capabilities(id, 150).unwrap(), 2);
    refusal(&db, &revoked, id, Verb::Report, 151, Refusal::Revoked);
}

#[test]
fn capability_ungranted_verb_is_refused() {
    let db = Db::open_memory().unwrap();
    let id = row(&db, "linear:CAP-8", "session-a");
    let secret = [0x38; 32];
    mint(&db, id, &secret, &REPORT);
    refusal(&db, &secret, id, Verb::Note, 101, Refusal::VerbNotGranted);
}

#[test]
fn capability_unknown_and_malformed_tokens_are_refused() {
    let db = Db::open_memory().unwrap();
    assert_eq!(
        parse_capability_token("A".repeat(64).as_str()),
        Err(Refusal::Malformed)
    );
    assert_eq!(
        parse_capability_token("f".repeat(63).as_str()),
        Err(Refusal::Malformed)
    );
    refusal(&db, &[0x39; 32], 7, Verb::Report, 1, Refusal::UnknownToken);
    let parsed = parse_capability_token("00".repeat(32).as_str()).unwrap();
    assert_eq!(parsed, [0; 32]);
    assert_eq!(Refusal::Malformed.kind(), "malformed");
    assert_eq!(Refusal::Malformed.to_string(), "malformed");
}

#[test]
fn capability_refusal_order_is_pinned() {
    let db = Db::open_memory().unwrap();
    let id = row(&db, "linear:CAP-9", "session-a");
    let secret = [0x3a; 32];
    mint(&db, id, &secret, &REPORT);
    db.revoke_dispatch_capabilities(id, 120).unwrap();
    refusal(&db, &secret, id + 1, Verb::Note, 600, Refusal::Revoked);

    let expired_secret = [0x4c; 32];
    mint(&db, id, &expired_secret, &REPORT);
    refusal(
        &db,
        &expired_secret,
        id + 1,
        Verb::Note,
        600,
        Refusal::Expired,
    );

    let wrong_row_secret = [0x4d; 32];
    mint(&db, id, &wrong_row_secret, &REPORT);
    refusal(
        &db,
        &wrong_row_secret,
        id + 1,
        Verb::Note,
        101,
        Refusal::WrongRow,
    );

    let verb_before_liveness_secret = [0x4e; 32];
    let terminal_id = row(&db, "linear:CAP-15", "session-terminal");
    mint(&db, terminal_id, &verb_before_liveness_secret, &REPORT);
    db.update_dispatch_status(terminal_id, AgentDispatchStatus::Done)
        .unwrap();
    refusal(
        &db,
        &verb_before_liveness_secret,
        terminal_id,
        Verb::Note,
        101,
        Refusal::VerbNotGranted,
    );

    let missing_row_secret = [0x4a; 32];
    let deleted_id = row(&db, "linear:CAP-13", "session-deleted");
    mint(&db, deleted_id, &missing_row_secret, &REPORT);
    db.conn()
        .execute("DELETE FROM agent_dispatches WHERE id=?1", [deleted_id])
        .unwrap();
    refusal(
        &db,
        &missing_row_secret,
        deleted_id,
        Verb::Report,
        101,
        Refusal::RowMissing,
    );

    let invalid_verbs_secret = [0x4b; 32];
    let invalid_verbs_id = row(&db, "linear:CAP-14", "session-invalid-verbs");
    mint(&db, invalid_verbs_id, &invalid_verbs_secret, &REPORT);
    db.conn()
        .execute(
            "UPDATE dispatch_capabilities SET verbs='merge' WHERE token_sha256=?1",
            [capability_token_digest(&invalid_verbs_secret)],
        )
        .unwrap();
    refusal(
        &db,
        &invalid_verbs_secret,
        invalid_verbs_id,
        Verb::Report,
        101,
        Refusal::InvalidStoredVerbs,
    );
}

#[test]
fn capability_mint_refuses_missing_inactive_sessionless_rows_and_bad_ttl() {
    let db = Db::open_memory().unwrap();
    let missing = CapabilityMint {
        dispatch_id: 404,
        verbs: &REPORT,
        ttl_ms: 1,
        now_ms: 0,
    };
    assert_eq!(
        db.mint_dispatch_capability(&[0x41; 32], &missing).unwrap(),
        Err(Refusal::RowMissing)
    );
    let inactive_id = row(&db, "linear:CAP-10", "session-a");
    db.update_dispatch_status(inactive_id, AgentDispatchStatus::Done)
        .unwrap();
    let inactive = CapabilityMint {
        dispatch_id: inactive_id,
        verbs: &REPORT,
        ttl_ms: 1,
        now_ms: 0,
    };
    assert_eq!(
        db.mint_dispatch_capability(&[0x42; 32], &inactive).unwrap(),
        Err(Refusal::RowNotLive)
    );
    let sessionless_id = db
        .put_agent_dispatch(NewDispatch::new(
            "linear:CAP-11",
            "/wt/capability",
            "worker",
        ))
        .unwrap();
    let sessionless = CapabilityMint {
        dispatch_id: sessionless_id,
        verbs: &REPORT,
        ttl_ms: 1,
        now_ms: 0,
    };
    assert_eq!(
        db.mint_dispatch_capability(&[0x43; 32], &sessionless)
            .unwrap(),
        Err(Refusal::MissingSession)
    );
    let empty = CapabilityMint {
        dispatch_id: inactive_id,
        verbs: &[],
        ttl_ms: 1,
        now_ms: 0,
    };
    assert_eq!(
        db.mint_dispatch_capability(&[0x44; 32], &empty).unwrap(),
        Err(Refusal::EmptyVerbs)
    );
    for ttl_ms in [0, -1, MAX_CAPABILITY_TTL_MS + 1] {
        let invalid = CapabilityMint {
            dispatch_id: inactive_id,
            verbs: &REPORT,
            ttl_ms,
            now_ms: 0,
        };
        assert_eq!(
            db.mint_dispatch_capability(&[0x45; 32], &invalid).unwrap(),
            Err(Refusal::InvalidTtl)
        );
    }
}

#[test]
fn capability_secret_is_never_stored() {
    let db = Db::open_memory().unwrap();
    let id = row(&db, "linear:CAP-12", "session-a");
    let secret = [0x46; 32];
    mint(&db, id, &secret, &REPORT);
    let (stored, stored_verbs, stored_session): (String, String, String) = db
        .conn()
        .query_row(
            "SELECT token_sha256, verbs, session_id FROM dispatch_capabilities",
            [],
            |record| Ok((record.get(0)?, record.get(1)?, record.get(2)?)),
        )
        .unwrap();
    assert_eq!(stored, capability_token_digest(&secret));
    assert_eq!(stored.len(), 64);
    let plaintext = "46".repeat(32);
    assert!(
        [stored, stored_verbs, stored_session]
            .iter()
            .all(|value| value != &plaintext)
    );
}
