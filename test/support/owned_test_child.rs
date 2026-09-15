//! Shared test-only exact-child custody, extracted from the static CLI fixture.
//! Each including test binary owns its own bounded slot registry.
use std::process::{Child, Command, ExitStatus};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

// Reserve before spawn. A failed final reap retains the exact child AND its
// private filesystem until the outer test-process watchdog settles this binary.
// No background reaper or unbounded Drop wait is introduced. This is test-only.
enum Slot {
    Vacant,
    Reserved,
    Held {
        _child: Child,
        _root: Arc<tempfile::TempDir>,
    },
}
fn slots() -> &'static Mutex<[Slot; 32]> {
    static SLOTS: OnceLock<Mutex<[Slot; 32]>> = OnceLock::new();
    SLOTS.get_or_init(|| Mutex::new(std::array::from_fn(|_| Slot::Vacant)))
}
fn lock_slots() -> std::sync::MutexGuard<'static, [Slot; 32]> {
    slots().lock().unwrap_or_else(|error| error.into_inner())
}

// A retained std::process::Child is insufficient after observation loses
// identity (e.g. Unix ECHILD). Conservatively revoke all subsequent process
// syscalls on ANY observation error, including cleanup/unwind retries.
#[derive(Default)]
pub(super) struct ProcessObservation {
    lost: bool,
}
impl ProcessObservation {
    pub(super) fn if_owned<T>(&self, operation: impl FnOnce() -> T) -> Option<T> {
        if self.lost { None } else { Some(operation()) }
    }
    pub(super) fn observe<T>(
        &mut self,
        operation: impl FnOnce() -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        let result = self.if_owned(operation).unwrap_or_else(|| {
            Err(std::io::Error::other(
                "owned child observation identity lost",
            ))
        });
        if result.is_err() {
            self.lost = true;
        }
        result
    }
}

pub(crate) struct OwnedChild {
    child: Option<Child>,
    root: Arc<tempfile::TempDir>,
    slot: usize,
    observation: ProcessObservation,
}
impl OwnedChild {
    pub(crate) fn spawn(command: &mut Command, root: Arc<tempfile::TempDir>) -> Self {
        let slot = {
            let mut slots = lock_slots();
            let index = slots
                .iter()
                .position(|s| matches!(s, Slot::Vacant))
                .expect("private CLI child custody capacity exhausted");
            slots[index] = Slot::Reserved;
            index
        };
        let mut owned = Self {
            child: None,
            root,
            slot,
            observation: ProcessObservation::default(),
        };
        owned.child = Some(command.spawn().expect("spawn owned thegn binary"));
        owned
    }
    pub(crate) fn poll(&mut self) -> Option<ExitStatus> {
        let status = self
            .observation
            .observe(|| self.child.as_mut().unwrap().try_wait())
            .expect("observe owned CLI child; subsequent syscalls revoked on error");
        if status.is_some() {
            self.child.take();
        }
        status
    }
    pub(crate) fn terminate(&mut self, budget: Duration) -> bool {
        let Some(child) = self.child.as_mut() else {
            return true;
        };
        // Exact retained Child only; errors still require observing or retaining it.
        if self.observation.if_owned(|| child.kill()).is_none() {
            return false;
        }
        let until = Instant::now() + budget;
        loop {
            match self.observation.observe(|| child.try_wait()) {
                Ok(Some(_)) => {
                    self.child.take();
                    return true;
                }
                Ok(None) => {}
                Err(_) => return false,
            }
            if Instant::now() >= until {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let settled = self.terminate(Duration::from_millis(500));
        let mut slots = lock_slots();
        slots[self.slot] = if settled {
            Slot::Vacant
        } else {
            Slot::Held {
                _child: self.child.take().unwrap(),
                _root: Arc::clone(&self.root),
            }
        };
    }
}
