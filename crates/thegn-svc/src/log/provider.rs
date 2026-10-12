use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// How long to sleep between polls when the file is at EOF. The log view is a
/// low-frequency surface; a 1s cadence is plenty responsive while keeping the
/// tailer far off the ~0%-idle hot path (unlike the old 100ms/10Hz spin).
const IDLE_POLL: Duration = Duration::from_secs(1);

pub trait LogProvider: Send + Sync {
    /// Start fetching and streaming logs into the provided sender
    fn start_stream(&self, tx: super::ingress::LogIngress, waker: Arc<dyn Fn() + Send + Sync>);
}

pub struct FileLogProvider {
    pub path: PathBuf,
}

/// Clamp and parse source lines into bounded pushes. `push` returns whether
/// its queue transitioned from empty and therefore needs a host wake.
fn push_bounded(
    lines: Vec<String>,
    mut push: impl FnMut(Vec<thegn_core::log::parser::ParsedLog>) -> bool,
) -> bool {
    let mut woke = false;
    let mut batch = Vec::with_capacity(super::ingress::MAX_BATCH_RECORDS);
    let mut batch_bytes = 0usize;
    for mut line in lines {
        super::ingress::clamp_record(&mut line);
        let parsed = thegn_core::log::parser::parse_log(&line);
        let parsed_bytes = super::ingress::parsed_cost(&parsed);
        if !batch.is_empty()
            && (batch.len() >= super::ingress::MAX_BATCH_RECORDS
                || batch_bytes.saturating_add(parsed_bytes) > super::ingress::MAX_BATCH_BYTES)
        {
            woke |= push(std::mem::take(&mut batch));
            batch = Vec::with_capacity(super::ingress::MAX_BATCH_RECORDS);
            batch_bytes = 0;
        }
        batch_bytes = batch_bytes.saturating_add(parsed_bytes);
        batch.push(parsed);
    }
    if !batch.is_empty() {
        woke |= push(batch);
    }
    woke
}

impl LogProvider for FileLogProvider {
    fn start_stream(&self, tx: super::ingress::LogIngress, waker: Arc<dyn Fn() + Send + Sync>) {
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
    tx: super::ingress::LogIngress,
    waker: Arc<dyn Fn() + Send + Sync>,
) {
    let mut follower = super::tail::FileFollower::new(path.to_path_buf());
    while !tx.is_orphaned() {
        // The follower bounds source reads to 64 KiB plus its 1 MiB partial
        // record cap. Parsed copies stay in smaller batches, so a poll with
        // many short lines cannot become one huge transient parsed batch.
        if push_bounded(follower.poll(), |batch| tx.push(batch)) {
            waker();
        }
        if !follower.has_more() {
            std::thread::sleep(IDLE_POLL);
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
    fn bounded_push_splits_many_short_lines() {
        let mut sizes = Vec::new();
        let woke = push_bounded((0..200).map(|i| format!("line-{i}")).collect(), |batch| {
            sizes.push(batch.len());
            false
        });

        assert!(!woke);
        assert_eq!(sizes.iter().sum::<usize>(), 200);
        assert!(sizes.len() > 1);
        assert!(
            sizes
                .iter()
                .all(|size| *size <= super::super::ingress::MAX_BATCH_RECORDS)
        );
    }

    #[test]
    fn bounded_push_truncates_an_oversized_record() {
        let mut records = Vec::new();
        assert!(!push_bounded(vec!["x".repeat(100 * 1024)], |batch| {
            records.extend(batch);
            false
        }));

        assert_eq!(records.len(), 1);
        assert!(records[0].original.ends_with(" [record truncated]"));
        assert!(
            records[0].original.len()
                <= super::super::ingress::MAX_RECORD_BYTES + " [record truncated]".len()
        );
    }

    #[test]
    fn tails_only_appends_not_history() {
        let dir = std::env::temp_dir().join(format!("tg-logprov-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir); // best-effort: test tmp cleanup
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("thegn.log");
        // Pre-existing history that must NOT be replayed.
        std::fs::write(&path, "old line 1\nold line 2\n").unwrap();

        let tx = super::super::ingress::LogIngress::new();
        let rx = tx.clone();
        let p = path.clone();
        let handle = std::thread::spawn(move || tail_file(&p, tx, noop_waker()));

        // Give the tailer a moment to open + seek to end, then append.
        std::thread::sleep(Duration::from_millis(50));
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(f, "new line").unwrap();
        }

        // The first (and only) batch we receive must be the appended line, never
        // the pre-existing history.
        let batch = loop {
            let batch = rx.take(100);
            if !batch.is_empty() {
                break batch;
            }
            std::thread::sleep(Duration::from_millis(100));
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
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
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
        let _ = handle.join(); // best-effort: join failure must not mask the assert above
        let _ = std::fs::remove_dir_all(&dir); // best-effort: test tmp cleanup
    }
}
