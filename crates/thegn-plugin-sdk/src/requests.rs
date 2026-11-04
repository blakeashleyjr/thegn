//! Correlation for plugin-originated RPC, separate from durable inbound receipts.
use std::collections::BTreeMap;
use std::time::Instant;
use thegn_core::plugin_api::RpcResponse;

pub const MAX_PENDING_REQUESTS: usize = 32;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestLimit {
    Full,
    IdsExhausted,
}

/// One tracker per originating session/direction. Callers own admission and
/// clocks: cancel a reservation when its write fails, and schedule work only
/// when `next_deadline` returns Some. Local cancellation emits no wire verb.
pub struct RequestTracker {
    next: u64,
    pending: BTreeMap<u64, Instant>,
}
impl Default for RequestTracker {
    fn default() -> Self {
        Self {
            next: 1,
            pending: BTreeMap::new(),
        }
    }
}
impl RequestTracker {
    pub fn begin(&mut self, deadline: Instant) -> Result<u64, RequestLimit> {
        if self.pending.len() >= MAX_PENDING_REQUESTS {
            return Err(RequestLimit::Full);
        }
        let id = self.next;
        self.next = self.next.checked_add(1).ok_or(RequestLimit::IdsExhausted)?;
        self.pending.insert(id, deadline);
        Ok(id)
    }
    pub fn cancel(&mut self, id: u64) -> bool {
        self.pending.remove(&id).is_some()
    }
    pub fn complete(&mut self, response: RpcResponse, now: Instant) -> Option<RpcResponse> {
        let deadline = self.pending.remove(&response.id)?;
        (deadline > now).then_some(response)
    }
    pub fn expire(&mut self, now: Instant) -> Vec<u64> {
        let expired = self
            .pending
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for id in &expired {
            self.pending.remove(id);
        }
        expired
    }
    pub fn next_deadline(&self) -> Option<Instant> {
        self.pending.values().copied().min()
    }
    pub fn len(&self) -> usize {
        self.pending.len()
    }
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[test]
    fn out_of_order_and_duplicate_replies_correlate_exactly_once() {
        let now = Instant::now();
        let mut t = RequestTracker::default();
        let a = t.begin(now + Duration::from_secs(1)).unwrap();
        let b = t.begin(now + Duration::from_secs(2)).unwrap();
        assert_eq!(
            t.complete(RpcResponse::ok(b, serde_json::json!("second")), now)
                .unwrap()
                .id,
            b
        );
        assert!(
            t.complete(RpcResponse::ok(b, serde_json::Value::Null), now)
                .is_none()
        );
        assert_eq!(
            t.complete(RpcResponse::ok(a, serde_json::json!("first")), now)
                .unwrap()
                .id,
            a
        );
        assert!(t.is_empty());
        assert!(t.next_deadline().is_none());
    }
    #[test]
    fn full_tracker_cancellation_and_deadlines_are_bounded() {
        let now = Instant::now();
        let mut t = RequestTracker::default();
        for _ in 0..MAX_PENDING_REQUESTS {
            t.begin(now + Duration::from_secs(1)).unwrap();
        }
        assert_eq!(t.begin(now), Err(RequestLimit::Full));
        assert!(t.cancel(1));
        assert!(
            t.complete(RpcResponse::ok(1, serde_json::Value::Null), now)
                .is_none()
        );
        let last = t.begin(now + Duration::from_secs(2)).unwrap();
        assert_eq!(t.expire(now + Duration::from_secs(1)).len(), 31);
        assert_eq!(t.next_deadline(), Some(now + Duration::from_secs(2)));
        assert!(
            t.complete(
                RpcResponse::ok(last, serde_json::Value::Null),
                now + Duration::from_secs(2)
            )
            .is_none()
        );
        assert!(t.next_deadline().is_none());
    }
    #[test]
    fn request_ids_never_wrap_or_reuse() {
        let mut t = RequestTracker {
            next: u64::MAX,
            pending: BTreeMap::new(),
        };
        assert_eq!(t.begin(Instant::now()), Err(RequestLimit::IdsExhausted));
        assert!(t.is_empty());
    }
}
