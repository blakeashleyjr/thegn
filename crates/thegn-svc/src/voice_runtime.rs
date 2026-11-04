//! Short-lived HTTP runtimes used by blocking dictation/setup adapters.
use std::future::Future;
use std::time::Duration;

pub(crate) fn run<F: Future>(operation: F) -> std::io::Result<F::Output> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(operation);
    // Dropping a runtime waits indefinitely for blocking OS DNS resolution.
    // The request future is already gone (including its audio and credentials).
    // Close async work without letting an uninterruptible resolver hold up a
    // cancellation/deadline; only the OS lookup may finish in the background.
    runtime.shutdown_timeout(Duration::from_millis(20));
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_http_runtime_does_not_wait_for_blocking_resolver_cleanup() {
        let (release, blocked) = std::sync::mpsc::channel();
        let (started, ready) = std::sync::mpsc::channel();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(1));
            release.send(()).unwrap();
        });
        let began = std::time::Instant::now();
        run(async move {
            let resolver = tokio::task::spawn_blocking(move || {
                started.send(()).unwrap();
                blocked.recv().unwrap();
            });
            ready.recv_timeout(Duration::from_secs(2)).unwrap();
            // Equivalent to cancelling a request while its OS resolver is busy.
            drop(resolver);
        })
        .unwrap();
        let elapsed = began.elapsed();
        releaser.join().unwrap();
        assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
    }
}
