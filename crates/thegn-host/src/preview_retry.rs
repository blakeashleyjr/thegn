//! Retry slots for self-contained preview and corner graphics updates.

use crate::kitty_relay;

#[derive(Debug, Default)]
pub(crate) struct PendingPreviewOob {
    pending: Option<Vec<u8>>,
}

impl PendingPreviewOob {
    /// Offer a newer rendered blob. Empty frames carry no update and leave an
    /// already refused blob intact.
    pub(crate) fn offer(
        &mut self,
        bytes: Vec<u8>,
        submit: impl FnMut(Vec<u8>) -> Result<(), Vec<u8>>,
    ) {
        if bytes.is_empty() {
            return;
        }
        self.pending = Some(bytes);
        self.retry(submit);
    }

    /// Retry the retained update on an existing loop wake.
    pub(crate) fn retry(&mut self, mut submit: impl FnMut(Vec<u8>) -> Result<(), Vec<u8>>) {
        let Some(bytes) = self.pending.take() else {
            return;
        };
        if let Err(bytes) = submit(bytes) {
            self.pending = Some(bytes);
        }
    }

    #[cfg(test)]
    fn pending(&self) -> Option<&[u8]> {
        self.pending.as_deref()
    }
}

/// A must-deliver corner delete followed by at most one replaceable draw.
/// Draws are complete, self-contained relay units; replacing one requires a
/// delete first so partially visible prior graphics cannot survive.
#[derive(Debug, Default)]
pub(crate) struct PendingCornerOob {
    delete_pending: bool,
    draw: Option<Vec<u8>>,
}

impl PendingCornerOob {
    /// Keep a host delete ahead of every later draw and discard stale geometry.
    pub(crate) fn require_delete(&mut self) {
        self.delete_pending = true;
        self.draw = None;
    }

    /// Retain the newest complete draw. Hidden draws wait until the corner is
    /// visible; every admission failure leaves the bytes here for a later wake.
    pub(crate) fn offer_draw(
        &mut self,
        bytes: Vec<u8>,
        visible: bool,
        submit: impl FnMut(Vec<u8>) -> Result<(), Vec<u8>>,
    ) {
        if bytes.is_empty() {
            return;
        }
        if visible && self.draw.is_some() {
            self.delete_pending = true;
        }
        self.draw = Some(bytes);
        self.retry(visible, submit);
    }

    /// Retry on an existing OOB capacity or producer wake, without a cell frame.
    pub(crate) fn retry(
        &mut self,
        visible: bool,
        mut submit: impl FnMut(Vec<u8>) -> Result<(), Vec<u8>>,
    ) {
        if self.delete_pending {
            if submit(kitty_relay::delete_all().to_vec()).is_err() {
                return;
            }
            self.delete_pending = false;
        }
        if visible
            && !self.delete_pending
            && let Some(bytes) = self.draw.take()
            && let Err(bytes) = submit(bytes)
        {
            self.draw = Some(bytes);
        }
    }

    #[cfg(test)]
    fn state(&self) -> (bool, Option<&[u8]>) {
        (self.delete_pending, self.draw.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn refused_preview_is_retained_and_submitted_once_when_capacity_returns() {
        let mut slot = PendingPreviewOob::default();
        slot.offer(vec![1, 2], Err);
        assert_eq!(slot.pending(), Some([1, 2].as_slice()));

        let submitted = RefCell::new(Vec::new());
        slot.retry(|bytes| {
            submitted.borrow_mut().push(bytes);
            Ok(())
        });
        assert_eq!(*submitted.borrow(), vec![vec![1, 2]]);
        slot.retry(|bytes| {
            submitted.borrow_mut().push(bytes);
            Ok(())
        });
        assert_eq!(*submitted.borrow(), vec![vec![1, 2]]);
    }

    #[test]
    fn newer_blob_replaces_pending_and_empty_blob_does_not_clear_it() {
        let mut slot = PendingPreviewOob::default();
        slot.offer(vec![1], Err);
        slot.offer(Vec::new(), |_| Ok(()));
        assert_eq!(slot.pending(), Some([1].as_slice()));
        slot.offer(vec![2], Err);
        assert_eq!(slot.pending(), Some([2].as_slice()));
    }

    #[test]
    fn refused_delete_stays_ahead_of_hidden_and_later_draws() {
        let mut slot = PendingCornerOob::default();
        slot.require_delete();
        slot.retry(false, Err);
        slot.offer_draw(vec![1], false, Err);
        slot.offer_draw(vec![2], false, Err);
        assert_eq!(slot.state(), (true, Some([2].as_slice())));

        let submitted = RefCell::new(Vec::new());
        slot.retry(true, |bytes| {
            submitted.borrow_mut().push(bytes);
            Ok(())
        });
        assert_eq!(
            *submitted.borrow(),
            vec![kitty_relay::delete_all().to_vec(), vec![2]]
        );
    }

    #[test]
    fn hidden_draws_wait_and_only_latest_is_replayed_once_on_clear() {
        let mut slot = PendingCornerOob::default();
        let submitted = RefCell::new(Vec::new());
        slot.offer_draw(vec![1], false, |bytes| {
            submitted.borrow_mut().push(bytes);
            Ok(())
        });
        slot.offer_draw(vec![2], false, |bytes| {
            submitted.borrow_mut().push(bytes);
            Ok(())
        });
        assert!(submitted.borrow().is_empty());

        slot.retry(true, |bytes| {
            submitted.borrow_mut().push(bytes);
            Ok(())
        });
        slot.retry(true, |bytes| {
            submitted.borrow_mut().push(bytes);
            Ok(())
        });
        assert_eq!(*submitted.borrow(), vec![vec![2]]);
    }

    #[test]
    fn refused_visible_draws_replace_with_delete_then_latest() {
        let mut slot = PendingCornerOob::default();
        slot.offer_draw(vec![1], true, Err);
        slot.offer_draw(vec![2], true, Err);
        assert_eq!(slot.state(), (true, Some([2].as_slice())));

        let submitted = RefCell::new(Vec::new());
        slot.retry(true, |bytes| {
            submitted.borrow_mut().push(bytes);
            Ok(())
        });
        assert_eq!(
            *submitted.borrow(),
            vec![kitty_relay::delete_all().to_vec(), vec![2]]
        );
    }

    #[test]
    fn require_delete_discards_draw_and_empty_offer_preserves_pending_state() {
        let mut slot = PendingCornerOob::default();
        slot.offer_draw(vec![1], false, |_| Ok(()));
        assert_eq!(slot.state(), (false, Some([1].as_slice())));
        slot.require_delete();
        assert_eq!(slot.state(), (true, None));
        slot.offer_draw(vec![2], false, |_| Ok(()));
        let pending = slot.state();
        let pending = (pending.0, pending.1.map(<[u8]>::to_vec));
        slot.offer_draw(Vec::new(), true, |_| Ok(()));
        let actual = slot.state();
        assert_eq!((actual.0, actual.1.map(<[u8]>::to_vec)), pending);
    }
}
