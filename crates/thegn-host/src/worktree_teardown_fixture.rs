//! Observe/refuse entry to explicit runtime teardown for one owned test path.
//! Automatic cleanup must never reach this ambient-resource boundary.
use std::{cell::Cell, cell::RefCell, path::Path, path::PathBuf, rc::Rc};

type Observation = (PathBuf, Rc<Cell<usize>>);
thread_local! {
    static OBSERVATION: RefCell<Option<Observation>> = const { RefCell::new(None) };
}

pub(crate) struct RuntimeTeardownObservation {
    previous: Option<Observation>,
    calls: Rc<Cell<usize>>,
}
impl RuntimeTeardownObservation {
    pub(crate) fn install(path: &Path) -> Self {
        let calls = Rc::new(Cell::new(0));
        let previous = OBSERVATION.with(|slot| slot.replace(Some((path.into(), calls.clone()))));
        Self { previous, calls }
    }
    pub(crate) fn calls(&self) -> usize {
        self.calls.get()
    }
}
impl Drop for RuntimeTeardownObservation {
    fn drop(&mut self) {
        OBSERVATION.with(|slot| {
            slot.replace(self.previous.take());
        });
    }
}

pub(super) fn refuse_observed_path(path: &Path) -> Result<(), String> {
    OBSERVATION.with(|slot| {
        if let Some((expected, calls)) = slot.borrow().as_ref()
            && expected == path
        {
            calls.set(calls.get() + 1);
            return Err("private fixture refused explicit runtime teardown".into());
        }
        Ok(())
    })
}
