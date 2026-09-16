//! One reserved OS-thread slot. No reaper thread and no capacity release until
//! an exact handle is finished and joined. Tests use private instances.
use super::Shared;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

struct Entry {
    token: u64,
    shared: Arc<Shared>,
    handle: JoinHandle<()>,
}
enum Occupant {
    Vacant,
    Reserved(u64),
    Running(Entry),
    Retiring(u64),
    // Production Slot is process-static. Retain the one exceptional join
    // payload forever rather than invoke an arbitrary destructor on the UI or
    // shutdown caller. This terminal state refuses replacement admission.
    Failed {
        _token: u64,
        _payload: Box<dyn std::any::Any + Send>,
    },
}
struct State {
    serial: u64,
    occupant: Occupant,
}
pub(super) struct Slot(Mutex<State>);
impl Default for Slot {
    fn default() -> Self {
        Self(Mutex::new(State {
            serial: 0,
            occupant: Occupant::Vacant,
        }))
    }
}
pub(super) fn global() -> Arc<Slot> {
    static SLOT: OnceLock<Arc<Slot>> = OnceLock::new();
    SLOT.get_or_init(|| Arc::new(Slot::default())).clone()
}
impl Slot {
    #[cfg(test)]
    pub(super) fn is_vacant(&self) -> bool {
        matches!(
            self.0
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .occupant,
            Occupant::Vacant
        )
    }
    pub(super) fn reserve(
        self: &Arc<Self>,
        shared: Arc<Shared>,
    ) -> Result<Reservation, &'static str> {
        // Retire outside the slot lock, retaining the Retiring token throughout.
        let prior = {
            let state = self
                .0
                .lock()
                .map_err(|_| "process sampler custody poisoned")?;
            match &state.occupant {
                Occupant::Running(entry) if entry.handle.is_finished() => Some(entry.token),
                Occupant::Vacant => None,
                Occupant::Failed { .. } => {
                    return Err("process sampler retains fatal failure custody; restart required");
                }
                _ => return Err("previous process sampler still owns its worker"),
            }
        };
        if let Some(token) = prior {
            self.reap_finished(token);
        }
        let mut state = self
            .0
            .lock()
            .map_err(|_| "process sampler custody poisoned")?;
        if !matches!(state.occupant, Occupant::Vacant) {
            return Err("previous process sampler still owns its worker");
        }
        let token = state
            .serial
            .checked_add(1)
            .ok_or("process sampler custody generation exhausted")?;
        state.serial = token;
        state.occupant = Occupant::Reserved(token);
        Ok(Reservation {
            slot: self.clone(),
            shared,
            token,
            handle: None,
            installed: false,
        })
    }
    pub(super) fn reap_finished(&self, token: u64) -> bool {
        let entry = {
            let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
            match &state.occupant {
                Occupant::Running(entry) if entry.token == token && entry.handle.is_finished() => {}
                Occupant::Vacant => return true,
                Occupant::Failed { .. } => return true,
                Occupant::Reserved(current) | Occupant::Retiring(current) if *current != token => {
                    return true;
                }
                Occupant::Running(entry) if entry.token != token => return true,
                _ => return false,
            }
            match std::mem::replace(&mut state.occupant, Occupant::Retiring(token)) {
                Occupant::Running(entry) => entry,
                _ => unreachable!("running occupant checked with the same lock"),
            }
        };
        // is_finished was observed above, and admission still sees Retiring.
        let outcome = entry.handle.join();
        match outcome {
            Ok(()) => {
                let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
                if matches!(state.occupant, Occupant::Retiring(current) if current == token) {
                    state.occupant = Occupant::Vacant;
                }
            }
            Err(payload) => {
                entry
                    .shared
                    .finish(Some("process sampler thread join failed; restart required"));
                let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
                // The private Retiring invariant excludes every other writer
                // until this exact joined handle has been classified.
                state.occupant = Occupant::Failed {
                    _token: token,
                    _payload: payload,
                };
            }
        }
        true
    }
}

pub(super) struct Reservation {
    slot: Arc<Slot>,
    shared: Arc<Shared>,
    token: u64,
    pub(super) handle: Option<JoinHandle<()>>,
    installed: bool,
}
impl Reservation {
    fn put_handle(&mut self) {
        // Only this reservation can alter its Reserved state. All other APIs
        // refuse it; poisoning cannot remove it. No callbacks or allocation
        // occur after taking the handle or before it is installed.
        let mut state = self
            .slot
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(handle) = self.handle.take() {
            state.occupant = Occupant::Running(Entry {
                token: self.token,
                shared: self.shared.clone(),
                handle,
            });
        }
    }
    pub(super) fn install(mut self) -> u64 {
        self.put_handle();
        self.installed = true;
        self.token
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        if self.installed {
            return;
        }
        self.shared.stop();
        if self.handle.is_some() {
            self.put_handle();
        } else {
            let mut state = self
                .slot
                .0
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if matches!(state.occupant, Occupant::Reserved(token) if token == self.token) {
                state.occupant = Occupant::Vacant;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    #[test]
    fn successful_spawn_is_retained_on_installation_unwind_even_when_poisoned() {
        for poison in [false, true] {
            let slot = Arc::new(Slot::default());
            let shared = Arc::new(Shared::new());
            let (release, wait) = mpsc::channel();
            let mut reservation = slot.reserve(shared.clone()).unwrap();
            let token = reservation.token;
            reservation.handle = Some(std::thread::spawn(move || {
                let _released_or_fixture_dropped = wait.recv();
            }));
            if poison {
                let _injected_panic = std::panic::catch_unwind(|| {
                    let _held = slot.0.lock().unwrap();
                    panic!("injected custody poison");
                });
            }
            let _injected_panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _reservation = reservation;
                panic!("injected panic after successful spawn");
            }));
            assert!(shared.lock().stop);
            assert!(slot.reserve(Arc::new(Shared::new())).is_err());
            assert!(!slot.reap_finished(token));
            release.send(()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !slot.reap_finished(token) {
                assert!(Instant::now() < deadline, "owned fixture did not return");
                std::thread::yield_now();
            }
            assert_eq!(slot.reserve(Arc::new(Shared::new())).is_err(), poison);
        }
    }
}

#[cfg(test)]
mod payload_tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    #[test]
    fn joined_failure_payload_is_retained_without_running_blocking_drop() {
        struct Payload {
            entered: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
        }
        impl Drop for Payload {
            fn drop(&mut self) {
                let _observed_or_fixture_dropped = self.entered.send(());
                let _released_or_fixture_dropped = self.release.recv();
            }
        }
        let slot = Arc::new(Slot::default());
        let shared = Arc::new(Shared::new());
        let (entered, observed) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let payload = Payload {
            entered,
            release: gate,
        };
        let mut reservation = slot.reserve(shared.clone()).unwrap();
        reservation.handle = Some(std::thread::spawn(move || std::panic::panic_any(payload)));
        let token = reservation.install();
        let (completed, completion) = mpsc::channel();
        let helper_slot = slot.clone();
        let helper = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !helper_slot.reap_finished(token) {
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
            assert!(matches!(
                shared.status.borrow().phase,
                super::super::Phase::Failed(_)
            ));
            assert!(helper_slot.reserve(Arc::new(Shared::new())).is_err());
            assert!(
                matches!(helper_slot.0.lock().unwrap().occupant, Occupant::Failed {_token: held, ..} if held == token)
            );
            assert!(helper_slot.reap_finished(token));
            let owner = super::super::ProcessWorker {
                slot: helper_slot.clone(),
                token,
                shared,
            };
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap();
            assert_eq!(
                runtime.block_on(owner.shutdown_until(tokio::time::Instant::now())),
                super::super::Settlement::Failed
            );
            drop(owner);
            assert!(helper_slot.reserve(Arc::new(Shared::new())).is_err());
            let _completed_or_fixture_dropped = completed.send(());
        });
        // If a regression invokes blocking Drop, this caller can still release
        // its fixture gate. Always release and join BEFORE asserting results.
        let returned = completion.recv_timeout(Duration::from_secs(10));
        let destructor_entered = observed.try_recv();
        let released = release.send(());
        let joined = helper.join();
        assert!(
            returned.is_ok(),
            "cleanup did not return before payload release"
        );
        assert!(destructor_entered.is_err(), "caller invoked opaque Drop");
        assert!(released.is_ok());
        assert!(joined.is_ok());
        // Unlike the process-static production slot, this private instance is
        // explicitly destroyed after releasing its fixture-only destructor.
        drop(slot);
        observed.recv_timeout(Duration::from_secs(10)).unwrap();
    }
}
