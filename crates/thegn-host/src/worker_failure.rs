//! One-shot failure notification for a long-lived worker that unwinds. This
//! guard adds no health poll/timer and is silent on ordinary channel shutdown.

pub(crate) struct PanicNotify<F: FnMut()> {
    notify: F,
}

impl<F: FnMut()> PanicNotify<F> {
    pub(crate) fn new(notify: F) -> Self {
        Self { notify }
    }
}

impl<F: FnMut()> Drop for PanicNotify<F> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            (self.notify)();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn worker_panic_notifies_once_and_normal_shutdown_is_silent() {
        let errors = Cell::new(0);
        let wakes = Cell::new(0);
        let notify = || {
            errors.set(errors.get() + 1);
            wakes.set(wakes.get() + 1);
        };
        {
            let _guard = PanicNotify::new(notify);
        }
        assert_eq!((errors.get(), wakes.get()), (0, 0));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = PanicNotify::new(notify);
            panic!("injected ticker failure");
        }));
        assert!(result.is_err());
        assert_eq!((errors.get(), wakes.get()), (1, 1));
    }
}
