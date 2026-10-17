//! Process-wide runtime for blocking background pollers.
//!
//! Callers must be on a plain or `spawn_blocking` thread, never the UI loop or
//! an async runtime thread. Sharing the runtime keeps async HTTP connection
//! pools alive across poll cycles.

use std::future::Future;
use std::sync::OnceLock;

static RUNTIME: OnceLock<Result<tokio::runtime::Runtime, String>> = OnceLock::new();

/// The shared current-thread runtime, or `None` if it could not be created.
pub fn runtime() -> Option<&'static tokio::runtime::Runtime> {
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .ok()
}

/// Run background async work on the shared runtime.
pub fn block_on<F: Future>(future: F) -> Option<F::Output> {
    runtime().map(|runtime| runtime.block_on(future))
}

#[cfg(test)]
mod tests {
    #[test]
    fn block_on_reuses_one_runtime() {
        let first = super::runtime().expect("runtime builds");
        let second = super::runtime().expect("runtime is cached");
        assert!(std::ptr::eq(first, second));
        assert_eq!(super::block_on(async { 7 }), Some(7));
    }
}
