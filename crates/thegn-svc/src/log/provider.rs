use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use thegn_core::log::parser::ParsedLog;

/// How long to sleep between polls when the file is at EOF. The log view is a
/// low-frequency surface; a 1s cadence is plenty responsive while keeping the
/// tailer far off the ~0%-idle hot path (unlike the old 100ms/10Hz spin).
const IDLE_POLL: Duration = Duration::from_secs(1);

pub trait LogProvider: Send + Sync {
    /// Start fetching and streaming logs into the provided sender
    fn start_stream(
        &self,
        tx: tokio::sync::mpsc::UnboundedSender<Vec<ParsedLog>>,
        waker: Arc<dyn Fn() + Send + Sync>,
    );
}

pub struct FileLogProvider {
    pub path: PathBuf,
}

impl LogProvider for FileLogProvider {
    fn start_stream(
        &self,
        tx: tokio::sync::mpsc::UnboundedSender<Vec<ParsedLog>>,
        waker: Arc<dyn Fn() + Send + Sync>,
    ) {
        let path = self.path.clone();
        // Blocking std file I/O + a polling sleep loop belongs on a blocking
        // worker, not a plain async task on a runtime worker thread.
        tokio::task::spawn_blocking(move || tail_file(&path, tx, waker));
    }
}

/// Tail `path`, parsing appended lines into batches pushed over `tx` and pulsing
/// `waker`. Starts at end-of-file (we don't replay historical logs — the view is
/// live tail, not a backfill) and exits promptly when the consumer is gone.
fn tail_file(
    path: &std::path::Path,
    tx: tokio::sync::mpsc::UnboundedSender<Vec<ParsedLog>>,
    waker: Arc<dyn Fn() + Send + Sync>,
) {
    let Ok(file) = File::open(path) else {
        return;
    };
    let mut reader = BufReader::new(file);
    // Seek to end: we tail live appends, not the whole (potentially many-MB)
    // historical log. Backfill would parse the entire file into batches on an
    // unbounded channel whether or not the log view is ever opened.
    let _ = reader.seek(SeekFrom::End(0));

    let mut line = String::new();
    let mut batch = Vec::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                // At EOF. Flush any pending batch, then either exit (consumer
                // gone) or sleep before polling for new appends.
                if !batch.is_empty() {
                    if tx.send(std::mem::take(&mut batch)).is_err() {
                        return; // consumer dropped — stop the task, don't leak it
                    }
                    waker();
                }
                if tx.is_closed() {
                    return; // consumer dropped — don't spin forever
                }
                std::thread::sleep(IDLE_POLL);
            }
            Ok(_) => {
                batch.push(thegn_core::log::parser::parse_log(line.trim_end()));
                if batch.len() >= 100 {
                    // Batch limit — flush eagerly.
                    if tx.send(std::mem::take(&mut batch)).is_err() {
                        return;
                    }
                    waker();
                }
            }
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn noop_waker() -> Arc<dyn Fn() + Send + Sync> {
        Arc::new(|| {})
    }

    #[test]
    fn tails_only_appends_not_history() {
        let dir = std::env::temp_dir().join(format!("sz-logprov-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("thegn.log");
        // Pre-existing history that must NOT be replayed.
        std::fs::write(&path, "old line 1\nold line 2\n").unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let p = path.clone();
        let handle = std::thread::spawn(move || tail_file(&p, tx, noop_waker()));

        // Give the tailer a moment to open + seek to end, then append.
        std::thread::sleep(Duration::from_millis(50));
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
            writeln!(f, "new line").unwrap();
        }

        // The first (and only) batch we receive must be the appended line, never
        // the pre-existing history.
        let batch = loop {
            match rx.try_recv() {
                Ok(b) => break b,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(_) => panic!("channel closed unexpectedly"),
            }
        };
        assert_eq!(batch.len(), 1, "expected only the appended line: {batch:?}");
        assert!(
            batch[0].message.contains("new line"),
            "batch not the appended line: {batch:?}"
        );

        // Dropping the receiver must make the tailer exit (not spin forever).
        drop(rx);
        // Prod it out of any in-flight sleep with another append + wait.
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
            writeln!(f, "trailing").unwrap();
        }
        let joined = {
            // Poll the thread for a bounded time — it must terminate.
            let start = std::time::Instant::now();
            loop {
                if handle.is_finished() {
                    break true;
                }
                if start.elapsed() > Duration::from_secs(5) {
                    break false;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        };
        assert!(joined, "tailer did not exit after the consumer was dropped");
        let _ = handle.join();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
