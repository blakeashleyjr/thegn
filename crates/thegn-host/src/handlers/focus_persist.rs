//! Ticketed, per-session ordering for active-tab pointer persistence.

use anyhow::Result;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

#[derive(Default)]
pub(crate) struct FocusLedger {
    next: u64,
    admitted: HashMap<String, u64>,
}

impl FocusLedger {
    pub(crate) fn ticket(&mut self) -> u64 {
        self.next += 1;
        self.next
    }
}

static LEDGER: LazyLock<Mutex<FocusLedger>> = LazyLock::new(|| Mutex::new(FocusLedger::default()));

pub(crate) fn next_ticket() -> u64 {
    LEDGER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .ticket()
}

pub(crate) fn write_in_order<F>(
    ledger: &Mutex<FocusLedger>,
    session: &str,
    ticket: u64,
    write: F,
) -> Option<Result<()>>
where
    F: FnOnce() -> Result<()>,
{
    let mut ledger = ledger
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if ledger
        .admitted
        .get(session)
        .is_some_and(|admitted| *admitted > ticket)
    {
        return None;
    }
    ledger.admitted.insert(session.to_owned(), ticket);
    Some(write())
}

pub(crate) fn write_in_order_global<F>(session: &str, ticket: u64, write: F) -> Option<Result<()>>
where
    F: FnOnce() -> Result<()>,
{
    write_in_order(&LEDGER, session, ticket, write)
}

#[cfg(test)]
mod tests {
    use super::{FocusLedger, write_in_order};
    use std::cell::RefCell;
    use std::sync::Mutex;

    #[test]
    fn older_ticket_arriving_last_is_skipped() {
        let ledger = Mutex::new(FocusLedger::default());
        let (older, newer) = {
            let mut ledger = ledger.lock().unwrap();
            (ledger.ticket(), ledger.ticket())
        };
        assert_eq!(older, 1);
        let writes = RefCell::new(Vec::new());

        let first = write_in_order(&ledger, "s", newer, || {
            writes.borrow_mut().push("b");
            Ok(())
        });
        let stale = write_in_order(&ledger, "s", older, || {
            writes.borrow_mut().push("a");
            Ok(())
        });

        assert!(first.is_some_and(|result| result.is_ok()));
        assert!(stale.is_none());
        assert_eq!(*writes.borrow(), ["b"]);
    }

    #[test]
    fn in_order_tickets_both_write() {
        let ledger = Mutex::new(FocusLedger::default());
        let (older, newer) = {
            let mut ledger = ledger.lock().unwrap();
            (ledger.ticket(), ledger.ticket())
        };
        let writes = RefCell::new(Vec::new());

        let first = write_in_order(&ledger, "s", older, || {
            writes.borrow_mut().push("a");
            Ok(())
        });
        let second = write_in_order(&ledger, "s", newer, || {
            writes.borrow_mut().push("b");
            Ok(())
        });

        assert!(first.is_some_and(|result| result.is_ok()));
        assert!(second.is_some_and(|result| result.is_ok()));
        assert_eq!(*writes.borrow(), ["a", "b"]);
    }

    #[test]
    fn sessions_are_independent() {
        let ledger = Mutex::new(FocusLedger::default());
        let (older, newer) = {
            let mut ledger = ledger.lock().unwrap();
            (ledger.ticket(), ledger.ticket())
        };
        let writes = RefCell::new(Vec::new());

        let x = write_in_order(&ledger, "x", newer, || {
            writes.borrow_mut().push("x");
            Ok(())
        });
        let y = write_in_order(&ledger, "y", older, || {
            writes.borrow_mut().push("y");
            Ok(())
        });

        assert!(x.is_some_and(|result| result.is_ok()));
        assert!(y.is_some_and(|result| result.is_ok()));
        assert_eq!(*writes.borrow(), ["x", "y"]);
    }

    #[test]
    fn a_failed_write_still_orders_later_stale_tickets() {
        let ledger = Mutex::new(FocusLedger::default());
        let (older, newer) = {
            let mut ledger = ledger.lock().unwrap();
            (ledger.ticket(), ledger.ticket())
        };
        let writes = RefCell::new(Vec::new());

        let failed = write_in_order(&ledger, "s", newer, || {
            writes.borrow_mut().push("failed");
            Err(anyhow::anyhow!("write failed"))
        });
        let stale = write_in_order(&ledger, "s", older, || {
            writes.borrow_mut().push("stale");
            Ok(())
        });

        assert!(failed.is_some_and(|result| result.is_err()));
        assert!(stale.is_none());
        assert_eq!(*writes.borrow(), ["failed"]);
    }
}
