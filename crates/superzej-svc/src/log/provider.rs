use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::sync::Arc;
use superzej_core::log::parser::ParsedLog;
use tokio::sync::mpsc;

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
        tokio::spawn(async move {
            if let Ok(file) = File::open(&path) {
                let mut reader = BufReader::new(file);
                let mut line = String::new();
                let mut batch = Vec::new();

                // Very rudimentary tail logic for POC
                // Real implementation needs `notify` and seeking to handle rotation and live appends
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) => {
                            if !batch.is_empty() {
                                let _ = tx.send(batch.clone());
                                batch.clear();
                                waker();
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                        Ok(_) => {
                            batch.push(superzej_core::log::parser::parse_log(line.trim_end()));
                            if batch.len() >= 100 {
                                // Batch limit
                                if tx.send(batch.clone()).is_err() {
                                    break;
                                }
                                batch.clear();
                                waker();
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        });
    }
}
