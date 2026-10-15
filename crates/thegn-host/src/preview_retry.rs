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

    pub(crate) fn is_pending(&self) -> bool {
        self.pending.is_some()
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

    #[cfg(test)]
    pub(crate) fn is_pending(&self) -> bool {
        self.delete_pending || self.draw.is_some()
    }

    pub(crate) fn blocks_output(&self, visible: bool) -> bool {
        self.delete_pending || (visible && self.draw.is_some())
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

/// Retains the newest admissible host clipboard copy. An oversized copy is
/// warned about and dropped without displacing an already retained copy.
#[derive(Debug, Default)]
pub(crate) struct PendingClipboardOob {
    pending: Option<Vec<u8>>,
    oversized: Option<usize>,
}

impl PendingClipboardOob {
    pub(crate) fn offer(
        &mut self,
        bytes: Vec<u8>,
        submit: impl FnMut(Vec<u8>) -> Result<(), Vec<u8>>,
    ) {
        if bytes.len() > crate::frame_writer::OOB_QUEUE_BYTES {
            self.oversized = Some(bytes.len());
            tracing::warn!(
                target: "thegn::frame",
                bytes = bytes.len(),
                "dropping oversized host clipboard OSC 52"
            );
            return;
        }
        self.pending = Some(bytes);
        self.retry(submit);
    }

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

    pub(crate) fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub(crate) fn take_oversized(&mut self) -> Option<usize> {
        self.oversized.take()
    }
}

pub(crate) fn oversized_copy_status(bytes: usize) -> String {
    const KIB: usize = 1024;
    const MIB: usize = KIB * KIB;
    let size = if bytes >= 1024 * 1024 {
        let whole = bytes / MIB;
        let tenths = ((bytes % MIB) * 10 + MIB - 1) / MIB;
        if tenths == 10 {
            format!("{}.0 MiB", whole + 1)
        } else {
            format!("{whole}.{tenths} MiB")
        }
    } else if bytes >= KIB {
        let whole = bytes / KIB;
        let tenths = ((bytes % KIB) * 10 + KIB / 2) / KIB;
        if tenths == 10 {
            format!("{}.0 KiB", whole + 1)
        } else {
            format!("{whole}.{tenths} KiB")
        }
    } else {
        format!("{bytes} bytes")
    };
    let limit_mib = crate::frame_writer::OOB_QUEUE_BYTES / MIB;
    format!("Nothing copied: {size} is over the {limit_mib} MiB terminal clipboard limit")
}

/// Coalesces refused bells into one retained BEL.
#[derive(Debug, Default)]
pub(crate) struct PendingBellOob {
    pending: bool,
}

impl PendingBellOob {
    pub(crate) fn offer(&mut self, submit: impl FnMut(Vec<u8>) -> Result<(), Vec<u8>>) {
        self.pending = true;
        self.retry(submit);
    }

    pub(crate) fn retry(&mut self, mut submit: impl FnMut(Vec<u8>) -> Result<(), Vec<u8>>) {
        if self.pending && submit(b"\x07".to_vec()).is_ok() {
            self.pending = false;
        }
    }

    pub(crate) fn is_pending(&self) -> bool {
        self.pending
    }
}

/// The muse marker is ordered after retained output and may be cancelled by
/// newly observed PTY input before a later retry.
#[derive(Debug, Default)]
pub(crate) struct PendingReadyMarker {
    pending: bool,
}

impl PendingReadyMarker {
    pub(crate) fn offer(&mut self, submit: impl FnMut(Vec<u8>) -> Result<(), Vec<u8>>) {
        self.pending = true;
        self.retry(false, submit);
    }

    pub(crate) fn retry(
        &mut self,
        input_pending: bool,
        mut submit: impl FnMut(Vec<u8>) -> Result<(), Vec<u8>>,
    ) {
        if !self.pending || input_pending {
            return;
        }
        if submit(MUSE_READY_MARKER.to_vec()).is_ok() {
            self.pending = false;
        }
    }

    pub(crate) fn cancel(&mut self) {
        self.pending = false;
    }

    pub(crate) fn is_pending(&self) -> bool {
        self.pending
    }
}

pub(crate) const MUSE_READY_MARKER: &[u8] = b"\x1b]5379;muse:ready\x07";

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

    #[test]
    fn refused_clipboard_is_retained_and_retried_once() {
        let mut slot = PendingClipboardOob::default();
        slot.offer(vec![1, 2], Err);
        assert_eq!(slot.pending(), Some([1, 2].as_slice()));

        let submitted = RefCell::new(Vec::new());
        slot.retry(|bytes| {
            submitted.borrow_mut().push(bytes);
            Ok(())
        });
        slot.retry(|bytes| {
            submitted.borrow_mut().push(bytes);
            Ok(())
        });
        assert_eq!(*submitted.borrow(), vec![vec![1, 2]]);
    }

    #[test]
    fn newer_clipboard_offer_supersedes_refused_copy() {
        let mut slot = PendingClipboardOob::default();
        slot.offer(vec![1], Err);
        slot.offer(vec![2], Err);
        assert_eq!(slot.pending(), Some([2].as_slice()));
        let submitted = RefCell::new(Vec::new());
        slot.retry(|bytes| {
            submitted.borrow_mut().push(bytes);
            Ok(())
        });
        assert_eq!(*submitted.borrow(), vec![vec![2]]);
    }

    #[test]
    fn oversized_clipboard_is_dropped_without_replacing_pending_copy() {
        let mut slot = PendingClipboardOob::default();
        let submitted = RefCell::new(Vec::new());
        let oversized_len = crate::frame_writer::OOB_QUEUE_BYTES + 1;
        slot.offer(vec![2; oversized_len], |bytes| {
            submitted.borrow_mut().push(bytes);
            Ok(())
        });
        assert!(submitted.borrow().is_empty());
        assert_eq!(slot.pending(), None);
        assert_eq!(slot.take_oversized(), Some(oversized_len));
        assert_eq!(slot.take_oversized(), None);

        slot.offer(vec![1], Err);
        slot.offer(vec![2; oversized_len], |bytes| {
            submitted.borrow_mut().push(bytes);
            Ok(())
        });
        assert!(submitted.borrow().is_empty());
        assert_eq!(slot.pending(), Some([1].as_slice()));
        assert_eq!(slot.take_oversized(), Some(oversized_len));
        assert_eq!(slot.pending(), Some([1].as_slice()));
    }

    #[test]
    fn oversized_copy_status_names_size_and_limit() {
        let status = oversized_copy_status(5 * 1024 * 1024 + 205 * 1024);
        assert!(status.contains("Nothing copied"));
        assert!(status.contains("5.2 MiB"));
        assert!(status.contains("4 MiB"));

        let just_over_limit = oversized_copy_status(crate::frame_writer::OOB_QUEUE_BYTES + 1);
        assert!(!just_over_limit.contains("4.0 MiB"));
    }

    #[test]
    fn refused_bell_is_retained_and_sent_once() {
        let mut bell = PendingBellOob::default();
        bell.offer(Err);
        assert!(bell.is_pending());

        let submitted = RefCell::new(Vec::new());
        bell.retry(|bytes| {
            submitted.borrow_mut().push(bytes);
            Ok(())
        });
        bell.retry(|bytes| {
            submitted.borrow_mut().push(bytes);
            Ok(())
        });
        assert_eq!(*submitted.borrow(), vec![b"\x07".to_vec()]);
        assert!(!bell.is_pending());
    }

    #[test]
    fn bells_while_refused_coalesce_into_one() {
        let mut bell = PendingBellOob::default();
        bell.offer(Err);
        bell.offer(Err);
        bell.offer(Err);
        assert!(bell.is_pending());

        let submitted = RefCell::new(Vec::new());
        bell.retry(|bytes| {
            submitted.borrow_mut().push(bytes);
            Ok(())
        });
        assert_eq!(*submitted.borrow(), vec![b"\x07".to_vec()]);
        assert!(!bell.is_pending());
    }

    #[test]
    fn refused_ready_marker_waits_for_input_clear_and_can_be_cancelled() {
        let mut slot = PendingReadyMarker::default();
        slot.offer(Err);
        assert!(slot.is_pending());

        let submitted = RefCell::new(Vec::new());
        slot.retry(true, |bytes| {
            submitted.borrow_mut().push(bytes);
            Ok(())
        });
        assert!(submitted.borrow().is_empty());
        assert!(slot.is_pending());

        slot.retry(false, |bytes| {
            submitted.borrow_mut().push(bytes);
            Ok(())
        });
        assert_eq!(*submitted.borrow(), vec![MUSE_READY_MARKER.to_vec()]);
        assert!(!slot.is_pending());

        slot.offer(Err);
        slot.cancel();
        assert!(!slot.is_pending());
    }

    #[test]
    fn hidden_retained_corner_draw_does_not_block_ready_marker() {
        let mut corner = PendingCornerOob::default();
        corner.offer_draw(vec![1], false, Err);
        assert!(corner.is_pending());
        assert!(!corner.blocks_output(false));
        assert!(corner.blocks_output(true));

        let mut marker = PendingReadyMarker::default();
        marker.offer(Err);
        let submitted = RefCell::new(Vec::new());
        if !corner.blocks_output(false) {
            marker.retry(false, |bytes| {
                submitted.borrow_mut().push(bytes);
                Ok(())
            });
        }
        assert_eq!(*submitted.borrow(), vec![MUSE_READY_MARKER.to_vec()]);
        assert!(!marker.is_pending());
    }
}
