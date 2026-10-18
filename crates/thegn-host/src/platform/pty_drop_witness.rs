//! Test-only `MasterPty` wrapper that records when the underlying master closes.

use portable_pty::{MasterPty, PtySize};

pub(crate) struct DropWitnessMaster {
    inner: Option<Box<dyn MasterPty + Send>>,
    on_drop: Option<Box<dyn FnOnce() + Send>>,
}

impl DropWitnessMaster {
    pub(crate) fn new(inner: Box<dyn MasterPty + Send>, on_drop: Box<dyn FnOnce() + Send>) -> Self {
        Self {
            inner: Some(inner),
            on_drop: Some(on_drop),
        }
    }

    fn inner(&self) -> &dyn MasterPty {
        self.inner
            .as_deref()
            .expect("PTY master witness was already dropped")
    }
}

impl MasterPty for DropWitnessMaster {
    fn resize(&self, size: PtySize) -> anyhow::Result<(), anyhow::Error> {
        self.inner().resize(size)
    }

    fn get_size(&self) -> anyhow::Result<PtySize, anyhow::Error> {
        self.inner().get_size()
    }

    fn try_clone_reader(&self) -> anyhow::Result<Box<dyn std::io::Read + Send>, anyhow::Error> {
        self.inner().try_clone_reader()
    }

    fn take_writer(&self) -> anyhow::Result<Box<dyn std::io::Write + Send>, anyhow::Error> {
        self.inner().take_writer()
    }

    #[cfg(unix)]
    fn process_group_leader(&self) -> Option<libc::pid_t> {
        self.inner().process_group_leader()
    }

    #[cfg(unix)]
    fn as_raw_fd(&self) -> Option<portable_pty::unix::RawFd> {
        self.inner().as_raw_fd()
    }

    #[cfg(unix)]
    fn tty_name(&self) -> Option<std::path::PathBuf> {
        self.inner().tty_name()
    }

    #[cfg(unix)]
    fn get_termios(&self) -> Option<nix::sys::termios::Termios> {
        self.inner().get_termios()
    }
}

impl Drop for DropWitnessMaster {
    fn drop(&mut self) {
        drop(self.inner.take());
        if let Some(on_drop) = self.on_drop.take() {
            on_drop();
        }
    }
}
