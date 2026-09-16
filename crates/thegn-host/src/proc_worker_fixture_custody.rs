//! Private test-worker custody; independent of production admission. A failed
//! fixture retains its exact handle in a bounded registry until process exit.
//! Thirty-two slots exceed the number of simultaneous worker fixtures in this
//! module; exhaustion refuses admission before spawning another thread.
use super::super::{Shared, custody::Slot};
use std::sync::{Arc, Mutex, mpsc::Sender};
use std::time::{Duration, Instant};

static RETAINED: Mutex<[Option<Arc<Slot>>; 32]> = Mutex::new([const { None }; 32]);
pub(super) struct Lease {
    index: usize,
    slot: Arc<Slot>,
}
impl Lease {
    pub fn reserve(slot: Arc<Slot>) -> Self {
        let mut slots = RETAINED.lock().unwrap_or_else(|error| error.into_inner());
        let index = slots
            .iter()
            .position(Option::is_none)
            .expect("private fixture custody is full");
        slots[index] = Some(slot.clone());
        Self { index, slot }
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        if self.slot.is_vacant() {
            let mut slots = RETAINED.lock().unwrap_or_else(|error| error.into_inner());
            if slots[self.index]
                .as_ref()
                .is_some_and(|slot| Arc::ptr_eq(slot, &self.slot))
            {
                slots[self.index] = None;
            }
        }
        // Running, retiring or terminal payload custody is retained. Never
        // detach an unfinished fixture handle just because an assertion failed.
    }
}
pub(super) struct Cleanup {
    lease: Lease,
    shared: Arc<Shared>,
    token: u64,
    gates: Vec<Sender<()>>,
}
impl Cleanup {
    pub fn new(lease: Lease, shared: Arc<Shared>, token: u64, gates: Vec<Sender<()>>) -> Self {
        Self {
            lease,
            shared,
            token,
            gates,
        }
    }
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        // Release fixture-owned OS-call stand-ins before bounded settlement.
        // This guard is the first field, so receivers and worker/control owners
        // are still alive while cleanup performs its exact finished-handle join.
        for gate in &self.gates {
            let _released_or_closed = gate.send(());
        }
        self.shared.stop();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.lease.slot.reap_finished(self.token) {
            if Instant::now() >= deadline {
                if !std::thread::panicking() {
                    panic!("private process worker remains held after fixture cleanup deadline");
                }
                return; // Lease Drop retains the still-owned slot.
            }
            // Cleanup-only polling; never part of worker steady-state evidence.
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
