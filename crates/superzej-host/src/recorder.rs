use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};

pub struct Recorder {
    file: File,
    start: Instant,
    path: PathBuf,
}

#[allow(dead_code)]
impl Recorder {
    pub fn new(cols: usize, rows: usize) -> Result<Self> {
        let dir = std::env::var("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".to_string()))
                    .join(".local/state")
            })
            .join("superzej/recordings");

        std::fs::create_dir_all(&dir)?;

        let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
        let path = dir.join(format!("superzej_{timestamp}.cast"));

        let mut file = File::create(&path).context("create cast file")?;

        // Asciinema v2 header
        let header = serde_json::json!({
            "version": 2,
            "width": cols,
            "height": rows,
            "timestamp": chrono::Utc::now().timestamp(),
            "env": {
                "TERM": "superzej",
                "SHELL": std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
            }
        });

        writeln!(file, "{}", header)?;

        Ok(Self {
            file,
            start: Instant::now(),
            path,
        })
    }

    pub fn write_frame(&mut self, data: &str) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }

        let elapsed = self.start.elapsed().as_secs_f64();
        let frame = serde_json::json!([elapsed, "o", data]);

        writeln!(self.file, "{}", frame)?;
        Ok(())
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }
}
