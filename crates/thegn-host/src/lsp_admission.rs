//! Fair, bounded admission for LSP work started by one worktree.

use std::collections::{HashMap, VecDeque};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use thegn_svc::lsp::{CancelToken, LspClient, LspError};

pub const MAX_INFLIGHT_PER_ROOT: usize = 4;
pub const MAX_QUEUED_PER_ROOT: usize = 16;
pub const ADMISSION_WAIT: Duration = Duration::from_secs(10);

#[derive(Default)]
struct RootState {
    in_flight: usize,
    queue: VecDeque<Waiter>,
}

struct Waiter {
    id: u64,
    tx: mpsc::Sender<Result<(), LspError>>,
}

#[derive(Default)]
struct AdmissionState {
    roots: HashMap<PathBuf, RootState>,
}

/// Per-root FIFO limiter. Permit drops transfer a slot to the queue head.
pub struct RootAdmission {
    state: Arc<Mutex<AdmissionState>>,
    next_id: AtomicU64,
    wait: Duration,
}

impl Default for RootAdmission {
    fn default() -> Self {
        Self::new()
    }
}

impl RootAdmission {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(AdmissionState::default())),
            next_id: AtomicU64::new(1),
            wait: ADMISSION_WAIT,
        }
    }

    #[cfg(test)]
    fn with_wait(wait: Duration) -> Self {
        Self {
            wait,
            ..Self::new()
        }
    }

    pub fn admit(&self, root: &Path, cancel: Option<&CancelToken>) -> Result<RootPermit, LspError> {
        if cancel.is_some_and(CancelToken::is_cancelled) {
            return Err(LspError::Cancelled);
        }
        let root = root.to_path_buf();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cancel.is_some_and(CancelToken::is_cancelled) {
            return Err(LspError::Cancelled);
        }
        let entry = state.roots.entry(root.clone()).or_default();
        if entry.in_flight < MAX_INFLIGHT_PER_ROOT && entry.queue.is_empty() {
            entry.in_flight += 1;
            return Ok(RootPermit {
                state: self.state.clone(),
                root,
            });
        }
        if entry.queue.len() >= MAX_QUEUED_PER_ROOT {
            return Err(LspError::Overloaded);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        entry.queue.push_back(Waiter { id, tx: tx.clone() });
        drop(state);

        if let Some(cancel) = cancel
            && let Err(error) = cancel.on_cancel(move || {
                if let Err(send_error) = tx.send(Err(LspError::Cancelled)) {
                    tracing::debug!(
                        target: "thegn::lsp",
                        %send_error,
                        "cancelled admission waiter was already gone"
                    );
                }
            })
        {
            if !self.remove_waiter(&root, id) {
                drop(RootPermit {
                    state: self.state.clone(),
                    root: root.clone(),
                });
            }
            return Err(error);
        }

        match rx.recv_timeout(self.wait) {
            Ok(Ok(())) => Ok(RootPermit {
                state: self.state.clone(),
                root,
            }),
            Ok(Err(error)) => {
                if !self.remove_waiter(&root, id) {
                    drop(RootPermit {
                        state: self.state.clone(),
                        root,
                    });
                }
                Err(error)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if !self.remove_waiter(&root, id) {
                    drop(RootPermit {
                        state: self.state.clone(),
                        root,
                    });
                }
                Err(LspError::Overloaded)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if !self.remove_waiter(&root, id) {
                    drop(RootPermit {
                        state: self.state.clone(),
                        root,
                    });
                }
                Err(LspError::Overloaded)
            }
        }
    }

    fn remove_waiter(&self, root: &Path, id: u64) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = state.roots.get_mut(root) {
            let old_len = entry.queue.len();
            entry.queue.retain(|waiter| waiter.id != id);
            let removed = entry.queue.len() < old_len;
            if entry.in_flight == 0 && entry.queue.is_empty() {
                state.roots.remove(root);
            }
            return removed;
        }
        false
    }

    #[cfg(test)]
    fn root_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .roots
            .len()
    }

    #[cfg(test)]
    pub(crate) fn in_flight(&self, root: &Path) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .roots
            .get(root)
            .map_or(0, |entry| entry.in_flight)
    }
}

/// An in-flight root slot, handed directly to the next queued caller on drop.
pub struct RootPermit {
    state: Arc<Mutex<AdmissionState>>,
    root: PathBuf,
}

impl Drop for RootPermit {
    fn drop(&mut self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(entry) = state.roots.get_mut(&self.root) else {
            return;
        };
        while let Some(waiter) = entry.queue.pop_front() {
            match waiter.tx.send(Ok(())) {
                Ok(()) => return,
                Err(error) => {
                    tracing::debug!(
                        target: "thegn::lsp",
                        %error,
                        "skipping abandoned LSP admission waiter"
                    )
                }
            }
        }
        entry.in_flight = entry.in_flight.saturating_sub(1);
        if entry.in_flight == 0 {
            state.roots.remove(&self.root);
        }
    }
}

/// Keeps admission while the client request is in scope.
pub struct AdmittedClient {
    _permit: RootPermit,
    client: Arc<LspClient>,
}

impl AdmittedClient {
    pub fn new(permit: RootPermit, client: Arc<LspClient>) -> Self {
        Self {
            _permit: permit,
            client,
        }
    }
}

impl Deref for AdmittedClient {
    type Target = LspClient;
    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn fill(admission: &RootAdmission, root: &Path) -> Vec<RootPermit> {
        (0..MAX_INFLIGHT_PER_ROOT)
            .map(|_| admission.admit(root, None).unwrap())
            .collect()
    }

    #[test]
    fn fifth_caller_on_a_root_waits_and_is_admitted_in_fifo_order() {
        let admission = Arc::new(RootAdmission::new());
        let root = PathBuf::from("/root");
        let permits = fill(&admission, &root);
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut joins = Vec::new();
        for name in ["A", "B"] {
            let thread_admission = admission.clone();
            let order = order.clone();
            let thread_root = root.clone();
            joins.push(thread::spawn(move || {
                let _permit = thread_admission.admit(&thread_root, None).unwrap();
                order.lock().unwrap().push(name);
            }));
            while admission
                .state
                .lock()
                .unwrap()
                .roots
                .get(&root)
                .is_none_or(|entry| entry.queue.len() < joins.len())
            {
                thread::yield_now();
            }
        }
        let mut permits = permits;
        drop(permits.pop());
        while order.lock().unwrap().is_empty() {
            thread::yield_now();
        }
        assert_eq!(order.lock().unwrap()[0], "A");
        drop(permits);
        for join in joins {
            join.join().unwrap();
        }
        assert_eq!(*order.lock().unwrap(), vec!["A", "B"]);
    }

    #[test]
    fn full_queue_returns_overloaded_immediately() {
        let admission = Arc::new(RootAdmission::new());
        let root = PathBuf::from("/root");
        let permits = fill(&admission, &root);
        let mut joins = Vec::new();
        for _ in 0..MAX_QUEUED_PER_ROOT {
            let thread_admission = admission.clone();
            let thread_root = root.clone();
            joins.push(thread::spawn(move || {
                thread_admission.admit(&thread_root, None)
            }));
            while admission
                .state
                .lock()
                .unwrap()
                .roots
                .get(&root)
                .is_none_or(|entry| entry.queue.len() < joins.len())
            {
                thread::yield_now();
            }
        }
        assert!(matches!(
            admission.admit(&root, None),
            Err(LspError::Overloaded)
        ));
        drop(permits);
        for join in joins {
            drop(join.join().unwrap());
        }
    }

    #[test]
    fn queued_waiter_times_out_as_overloaded_without_taking_a_slot() {
        let admission = Arc::new(RootAdmission::with_wait(Duration::from_millis(5)));
        let root = PathBuf::from("/root");
        let permits = fill(&admission, &root);
        assert!(matches!(
            admission.admit(&root, None),
            Err(LspError::Overloaded)
        ));
        assert_eq!(
            admission.state.lock().unwrap().roots[&root].in_flight,
            MAX_INFLIGHT_PER_ROOT
        );
        drop(permits);
    }

    #[test]
    fn cancelled_waiter_leaves_the_queue_and_the_slot_goes_to_the_next() {
        let admission = Arc::new(RootAdmission::new());
        let root = PathBuf::from("/root");
        let permits = fill(&admission, &root);
        let token = CancelToken::new();
        let a_admission = admission.clone();
        let a_root = root.clone();
        let a_token = token.clone();
        let a = thread::spawn(move || a_admission.admit(&a_root, Some(&a_token)));
        while admission.state.lock().unwrap().roots[&root]
            .queue
            .is_empty()
        {
            thread::yield_now();
        }
        let b_admission = admission.clone();
        let b_root = root.clone();
        let b = thread::spawn(move || b_admission.admit(&b_root, None));
        while admission.state.lock().unwrap().roots[&root].queue.len() < 2 {
            thread::yield_now();
        }
        token.cancel();
        assert!(matches!(a.join().unwrap(), Err(LspError::Cancelled)));
        let mut permits = permits;
        drop(permits.pop());
        let b_permit = b.join().unwrap().unwrap();
        assert_eq!(
            admission.state.lock().unwrap().roots[&root].in_flight,
            MAX_INFLIGHT_PER_ROOT
        );
        drop(b_permit);
        drop(permits);
    }

    #[test]
    fn other_roots_are_not_blocked_by_a_saturated_root() {
        let admission = RootAdmission::new();
        let _permits = fill(&admission, Path::new("/one"));
        let other = admission.admit(Path::new("/two"), None).unwrap();
        drop(other);
    }

    #[test]
    fn dropping_the_last_permit_removes_the_root_entry() {
        let admission = RootAdmission::new();
        drop(admission.admit(Path::new("/root"), None).unwrap());
        assert_eq!(admission.root_count(), 0);
    }

    #[test]
    fn new_symbol_query_cancels_the_previous_token() {
        let mut session = crate::search_everywhere::PaletteSession::new(Vec::new());
        let previous = session.symbol_search.0.clone();
        let current = session.symbol_search.replace();
        assert!(previous.is_cancelled());
        assert!(!current.is_cancelled());
    }

    #[test]
    fn dropping_the_palette_session_cancels_its_symbol_search() {
        let session = crate::search_everywhere::PaletteSession::new(Vec::new());
        let token = session.symbol_search.0.clone();
        drop(session);
        assert!(token.is_cancelled());
    }
}
