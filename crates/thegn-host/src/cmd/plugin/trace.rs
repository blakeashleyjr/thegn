//! Bounded preview traces. Live protocol payloads are never recorded.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;

const MAX_TRACE_BYTES: usize = 8 << 20;
const MAX_RECORDS: usize = 20_000;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Record {
    pub version: u8,
    pub sequence: u64,
    pub elapsed_ms: u64,
    pub session: String,
    pub direction: String,
    pub redacted: bool,
    pub frame: Value,
}

#[derive(Default)]
pub(super) struct Trace {
    records: Vec<Record>,
    bytes: usize,
    synthetic: bool,
    enabled: bool,
}
impl Trace {
    pub fn new(synthetic: bool) -> Self {
        Self {
            synthetic,
            enabled: true,
            ..Self::default()
        }
    }
    pub fn enabled(&self) -> bool {
        self.enabled
    }
    #[cfg(test)]
    pub fn record_count(&self) -> usize {
        self.records.len()
    }
    pub fn push(
        &mut self,
        direction: &str,
        session: &str,
        elapsed_ms: u64,
        frame: &Value,
    ) -> Result<()> {
        if !self.enabled() {
            return Ok(());
        }
        // Even synthetic captures cannot contain authentication/host-call traffic.
        let permitted = frame
            .get("method")
            .and_then(Value::as_str)
            .is_some_and(|method| matches!(method, "update" | "on_event"));
        let full = self.synthetic && permitted;
        let frame = if full {
            frame.clone()
        } else {
            json!({"method": frame.get("method").and_then(Value::as_str),
                "id":frame.get("id").and_then(Value::as_u64),
                "error":frame.get("error").is_some(),"payload":"redacted"})
        };
        let record = Record {
            version: 1,
            sequence: self.records.len() as u64,
            elapsed_ms,
            session: session.into(),
            direction: direction.into(),
            redacted: !full,
            frame,
        };
        let len = serde_json::to_vec(&record)?.len() + 1;
        if self.records.len() >= MAX_RECORDS || len > MAX_TRACE_BYTES.saturating_sub(self.bytes) {
            self.enabled = false;
            bail!("preview trace limit reached; existing capture retained");
        }
        self.bytes += len;
        self.records.push(record);
        Ok(())
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        let mut bytes = Vec::with_capacity(self.bytes);
        for record in &self.records {
            serde_json::to_writer(&mut bytes, record)?;
            bytes.push(b'\n');
        }
        thegn_core::fsperm::write_owner_only_new(path, &bytes)
            .context("cannot create private preview trace (destination must be new)")
    }
}

pub(super) fn read(path: &Path) -> Result<Vec<Record>> {
    use std::io::{BufRead, BufReader, Read};
    let file = thegn_core::fsperm::open_regular_file_nofollow(path)?;
    if file.metadata()?.len() > MAX_TRACE_BYTES as u64 {
        bail!("preview trace exceeds eight MiB");
    }
    let mut records = Vec::new();
    // A growing input cannot escape the total-byte limit after the metadata check.
    let reader = BufReader::new(file.take(MAX_TRACE_BYTES as u64 + 1));
    let mut total = 0;
    for line in reader.lines() {
        let line = line?;
        total += line.len() + 1;
        if total > MAX_TRACE_BYTES || records.len() >= MAX_RECORDS {
            bail!("preview trace exceeds limits");
        }
        let record: Record = serde_json::from_str(&line).context("invalid preview trace record")?;
        if record.version != 1 || record.sequence != records.len() as u64 {
            bail!("unsupported preview trace version or nonsequential record");
        }
        if !matches!(record.direction.as_str(), "incoming" | "outgoing") {
            bail!("invalid preview trace direction");
        }
        if record.redacted {
            bail!("redacted live trace cannot be replayed; record a synthetic fixture");
        }
        records.push(record);
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn live_trace_excludes_payloads_and_credentials() {
        let mut trace = Trace::new(false);
        trace.push("incoming", "s1", 0, &json!({"method":"update","params":{"app":{"secret":"token-value","text":"private task"}}})).unwrap();
        let encoded = String::from_utf8(serde_json::to_vec(&trace.records).unwrap()).unwrap();
        assert!(!encoded.contains("token-value"));
        assert!(!encoded.contains("private task"));
        assert!(trace.records[0].redacted);
    }
    #[test]
    fn synthetic_traces_still_exclude_host_secrets() {
        let mut trace = Trace::new(true);
        trace.push("outgoing", "s1", 0, &json!({"method":"host.call","params":{"capability":"secret.get","token":"never-record"}})).unwrap();
        trace
            .push(
                "incoming",
                "s1",
                1,
                &json!({"params":{"token":"methodless-secret"}}),
            )
            .unwrap();
        let encoded = String::from_utf8(serde_json::to_vec(&trace.records).unwrap()).unwrap();
        assert!(!encoded.contains("never-record"));
        assert!(!encoded.contains("methodless-secret"));
    }
    #[test]
    fn bounded_roundtrip_and_existing_files_are_protected() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("trace.jsonl");
        let mut trace = Trace::new(true);
        trace
            .push(
                "incoming",
                "fixture",
                0,
                &json!({"method":"update","params":{}}),
            )
            .unwrap();
        trace.save(&path).unwrap();
        assert_eq!(read(&path).unwrap().len(), 1);
        assert!(trace.save(&path).is_err());
        let huge = json!({"method":"update","payload":"x".repeat(MAX_TRACE_BYTES)});
        assert!(trace.push("incoming", "fixture", 1, &huge).is_err());
        assert_eq!(trace.records.len(), 1);
    }
    #[test]
    fn unknown_versions_and_redacted_replays_fail() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("trace.jsonl");
        let mut trace = Trace::new(true);
        trace
            .push("incoming", "fixture", 0, &json!({"method":"update"}))
            .unwrap();
        let mut encoded = serde_json::to_value(&trace.records[0]).unwrap();
        encoded["version"] = json!(99);
        std::fs::write(&path, format!("{}\n", encoded)).unwrap();
        assert!(
            read(&path).is_err(),
            "unknown trace versions must be refused"
        );

        let mut live = Trace::new(false);
        live.push("incoming", "live", 0, &json!({"method":"update"}))
            .unwrap();
        live.save(&path.with_file_name("redacted.jsonl")).unwrap();
        assert!(read(&path.with_file_name("redacted.jsonl")).is_err());
    }
}
