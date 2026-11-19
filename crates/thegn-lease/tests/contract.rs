use std::collections::BTreeMap;
use std::num::NonZeroU64;

use serde::Deserialize;
use thegn_lease::{AttemptLease, Commit, Error, Interruption, Phase, State, Token};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Vector {
    previous_epoch: u64,
    deadline_ms: u64,
    ttl_ms: NonZeroU64,
    operations: Vec<(String, String, u64, String)>,
}

#[test]
fn replays_shared_reassignment_vector_after_persistence() {
    let vector: Vector = serde_json::from_str(include_str!("fixtures/reassignment.json")).unwrap();
    let mut epoch = vector.previous_epoch;
    let mut active: Option<AttemptLease> = None;
    let mut tokens: BTreeMap<String, Token> = BTreeMap::new();
    for (op, worker, now, expected) in vector.operations {
        if op == "claim" {
            if let Some(old) = &active {
                assert_eq!(old.state(), &State::Closed(Interruption::LeaseExpired));
            }
            let claimed = AttemptLease::claim(
                epoch,
                format!("run-{worker}"),
                worker.clone(),
                now,
                vector.ttl_ms,
                vector.deadline_ms,
            )
            .unwrap();
            assert_eq!(claimed.token().epoch.get(), epoch + 1);
            epoch = claimed.token().epoch.get();
            tokens.insert(worker, claimed.token().clone());
            active = Some(claimed);
            assert_eq!(expected, "leased");
            continue;
        }
        let lease = active.as_mut().unwrap();
        let token = &tokens[&worker];
        let result = match op.as_str() {
            "prepare" => lease.advance(token, now, Phase::Preparing).map(|()| "ok"),
            "run" => lease.advance(token, now, Phase::Running).map(|()| "ok"),
            "committing" => lease.advance(token, now, Phase::Committing).map(|()| "ok"),
            "renew" => lease.renew(token, now, vector.ttl_ms).map(|()| "ok"),
            "expire" => lease
                .interrupt(token, now, Interruption::LeaseExpired)
                .map(|()| "ok"),
            "commit" | "conflictingCommit" => {
                let digest = if op == "conflictingCommit" {
                    "other"
                } else {
                    "report"
                };
                lease
                    .commit(token, now, digest)
                    .map(|receipt| match receipt {
                        Commit::Publish => "publish",
                        Commit::Replay => "replay",
                    })
            }
            _ => panic!("unknown operation {op}"),
        };
        let actual = match result {
            Ok(value) => value,
            Err(Error::Expired) => "expired",
            Err(Error::Fenced) => "fenced",
            Err(Error::ConflictingCommit) => "conflict",
            Err(other) => panic!("{other:?}"),
        };
        assert_eq!(actual, expected, "{op} from {worker} at {now}");
        *lease = serde_json::from_slice(&serde_json::to_vec(lease).unwrap()).unwrap();
    }
}

#[test]
fn cancellation_deadline_and_identity_gate_every_mutation() {
    let ttl = NonZeroU64::new(20).unwrap();
    let mut lease = AttemptLease::claim(0, "run".into(), "worker".into(), 5, ttl, 30).unwrap();
    let token = lease.token().clone();
    assert_eq!(lease.commit(&token, 10, "report"), Err(Error::WrongPhase));
    let mut wrong = token.clone();
    wrong.epoch = NonZeroU64::new(2).unwrap();
    assert_eq!(
        lease.interrupt(&wrong, 10, Interruption::Preempted),
        Err(Error::Fenced)
    );
    wrong = token.clone();
    wrong.worker_id = "other".into();
    assert_eq!(lease.renew(&wrong, 10, ttl), Err(Error::Fenced));
    lease.renew(&token, 18, ttl).unwrap();
    assert_eq!(lease.expires_at_ms(), 30);
    assert_eq!(lease.renew(&token, 30, ttl), Err(Error::Expired));
    assert_eq!(
        lease.interrupt(&token, 29, Interruption::TimedOut),
        Err(Error::WrongPhase)
    );
    lease
        .interrupt(&token, 29, Interruption::Cancelled)
        .unwrap();
    assert_eq!(lease.commit(&token, 29, "report"), Err(Error::Fenced));
    assert_eq!(
        AttemptLease::claim(u64::MAX, "run".into(), "w".into(), 0, ttl, 100),
        Err(Error::EpochExhausted)
    );
}
