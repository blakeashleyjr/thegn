//! Scoped cancellation probe shared by the fold and gate paths.
//!
//! A thread-local probe keeps cancellation attached to the one synchronous
//! fold worker that owns the gate process. Nested scopes restore their parent.

use std::cell::RefCell;
use std::rc::Rc;

thread_local! {
    static PROBE: RefCell<Option<Rc<dyn Fn() -> bool>>> = RefCell::new(None);
}

pub(crate) struct CancelScope {
    previous: Option<Rc<dyn Fn() -> bool>>,
}

pub(crate) fn scope(probe: impl Fn() -> bool + 'static) -> CancelScope {
    let probe: Rc<dyn Fn() -> bool> = Rc::new(probe);
    let previous = PROBE.with(|slot| slot.replace(Some(probe)));
    CancelScope { previous }
}

pub(crate) fn requested() -> bool {
    PROBE.with(|slot| slot.borrow().as_ref().is_some_and(|probe| probe()))
}

impl Drop for CancelScope {
    fn drop(&mut self) {
        PROBE.with(|slot| {
            drop(slot.replace(self.previous.take()));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    #[test]
    fn no_scope_is_not_cancelled_and_nested_scope_restores_outer_probe() {
        assert!(!requested());
        let outer_value = Rc::new(Cell::new(false));
        let probe_value = Rc::clone(&outer_value);
        let _outer = scope(move || probe_value.get());
        assert!(!requested());
        outer_value.set(true);
        {
            let _inner = scope(|| true);
            assert!(requested());
        }
        assert!(requested());
    }
}
