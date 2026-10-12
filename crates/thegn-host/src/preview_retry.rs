//! Latest-wins retry slot for self-contained preview graphics updates.

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
}
