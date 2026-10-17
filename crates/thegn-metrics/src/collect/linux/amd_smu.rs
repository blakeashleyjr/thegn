//! Linux reader for the per-card amdgpu `gpu_metrics` pseudo-file.
use std::fs;
use std::io;
use std::path::PathBuf;

use super::procfile::ProcFile;
use crate::collect::model::amd_smu::{AmdSmuCard, AmdSmuStatus, MAX_CARDS};
use crate::collect::parse::amd_gpu_metrics::{GpuMetrics, decode};
use crate::collect::{
    CollectError, Collector, CollectorId, TickCtx,
    env::Env,
    frame::MonitorFrame,
    health::{Absent, Health},
    schedule::Cadence,
};

#[derive(Debug)]
struct CardReader {
    index: u8,
    metrics_path: PathBuf,
    gate_path: PathBuf,
    gate: Option<ProcFile>,
    metrics: Option<ProcFile>,
}

#[derive(Debug)]
pub struct AmdSmuCollector {
    root: PathBuf,
    cards: Vec<CardReader>,
    discovered: bool,
}

impl AmdSmuCollector {
    pub fn new() -> Self {
        Self::with_root(PathBuf::from("/sys/class/drm"))
    }

    pub fn with_root(root: PathBuf) -> Self {
        Self {
            root,
            cards: Vec::new(),
            discovered: false,
        }
    }

    fn reopen_metrics(card: &mut CardReader) -> io::Result<()> {
        card.metrics = Some(ProcFile::open(&card.metrics_path)?);
        Ok(())
    }

    fn sample_card(card: &mut CardReader) -> Result<AmdSmuCard, io::ErrorKind> {
        if card.gate.as_ref().is_some_and(ProcFile::needs_reopen) {
            card.gate = Some(ProcFile::open(&card.gate_path).map_err(|error| error.kind())?);
        }
        if let Some(gate) = card.gate.as_mut() {
            let state = gate.read_all().map_err(|error| error.kind())?;
            if state.trim_ascii() == b"suspended" {
                return Ok(AmdSmuCard {
                    card: card.index,
                    suspended: true,
                    decoded: AmdSmuStatus::Suspended,
                    metrics: GpuMetrics::default(),
                });
            }
        }
        if card.metrics.as_ref().is_none_or(ProcFile::needs_reopen) {
            Self::reopen_metrics(card).map_err(|error| error.kind())?;
        }
        let metrics_file = card.metrics.as_mut().expect("opened above");
        let bytes = metrics_file.read_all().map_err(|error| error.kind())?;
        let mut metrics = GpuMetrics::default();
        let decoded = decode(bytes, &mut metrics);
        if let Some(header) = metrics.header
            && matches!(header.format, 2 | 3)
        {
            card.gate = None;
        }
        Ok(AmdSmuCard {
            card: card.index,
            suspended: false,
            decoded: AmdSmuStatus::Decoded(decoded),
            metrics,
        })
    }
}

impl Default for AmdSmuCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl Collector for AmdSmuCollector {
    fn id(&self) -> CollectorId {
        CollectorId::AmdSmu
    }

    fn cadence(&self) -> Cadence {
        Cadence::Every(1)
    }

    fn discover(&mut self, _env: &Env) -> Health {
        self.close();
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                self.discovered = true;
                return Health::Absent(Absent::NoPermission);
            }
            Err(_) => {
                self.discovered = true;
                return Health::Absent(Absent::NoHardware);
            }
        };
        let mut candidates = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name();
                let name = name.to_str()?;
                let digits = name.strip_prefix("card")?;
                if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                    return None;
                }
                let index = digits.parse::<u8>().ok()?;
                Some((index, entry.path()))
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|(index, _)| *index);
        let mut denied = 0;
        let mut seen_gpu_metrics = 0;
        for (index, card_path) in candidates {
            if self.cards.len() >= MAX_CARDS {
                break;
            }
            let metrics_path = card_path.join("device/gpu_metrics");
            if !metrics_path.exists() {
                continue;
            }
            seen_gpu_metrics += 1;
            let gate_path = card_path.join("device/power/runtime_status");
            let mut gate = match ProcFile::open(&gate_path) {
                Ok(reader) => Some(reader),
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                    denied += 1;
                    continue;
                }
                Err(_) => continue,
            };
            let initial_state = match gate.as_mut().expect("gate opened").read_all() {
                Ok(state) => state,
                Err(_) => continue,
            };
            let suspended = initial_state.trim_ascii() == b"suspended";
            let metrics = if suspended {
                None
            } else {
                match ProcFile::open(&metrics_path) {
                    Ok(reader) => Some(reader),
                    Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                        denied += 1;
                        continue;
                    }
                    Err(_) => continue,
                }
            };
            // Active cards have their gate checked above before gpu_metrics is opened;
            // the held gate is sampled once on each subsequent tick.
            self.cards.push(CardReader {
                index,
                metrics_path,
                gate_path,
                gate,
                metrics,
            });
        }
        self.discovered = true;
        if !self.cards.is_empty() {
            Health::Ok
        } else if seen_gpu_metrics > 0 && denied == seen_gpu_metrics {
            Health::Absent(Absent::NoPermission)
        } else {
            Health::Absent(Absent::NoHardware)
        }
    }

    fn sample(&mut self, _ctx: &TickCtx, frame: &mut MonitorFrame) -> Result<(), CollectError> {
        if !self.discovered {
            return Err(CollectError::Gone);
        }
        let mut failures = 0;
        let result_cards = &mut frame.parts.amd_smu.cards;
        result_cards.clear();
        for card in &mut self.cards {
            match Self::sample_card(card) {
                Ok(result) => result_cards.push(result),
                Err(kind) => {
                    failures += 1;
                    result_cards.push(AmdSmuCard {
                        card: card.index,
                        suspended: false,
                        decoded: AmdSmuStatus::Error(kind),
                        metrics: GpuMetrics::default(),
                    });
                }
            }
        }
        if !self.cards.is_empty() && failures == self.cards.len() {
            Err(CollectError::Gone)
        } else {
            Ok(())
        }
    }

    fn close(&mut self) {
        self.cards.clear();
        self.discovered = false;
    }
}

#[cfg(test)]
mod tests {
    use super::AmdSmuCollector;
    use crate::collect::model::amd_smu::AmdSmuStatus;
    use crate::collect::{
        Collector, TickCtx,
        env::Env,
        frame::MonitorFrame,
        health::{Absent, Health},
    };
    use std::fs;
    use std::path::{Path, PathBuf};

    fn v3() -> [u8; 264] {
        let mut bytes = [0_u8; 264];
        bytes[..4].copy_from_slice(&[8, 1, 3, 0]);
        bytes
    }

    fn put(path: &Path, bytes: &[u8]) {
        if path.exists() {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(path)
                .unwrap();
            use std::io::Write;
            file.write_all(bytes).unwrap();
        } else {
            fs::write(path, bytes).unwrap();
        }
    }

    fn card(root: &Path, id: usize, status: &[u8], metrics: &[u8]) {
        let device = root.join(format!("card{id}")).join("device");
        fs::create_dir_all(device.join("power")).unwrap();
        put(&device.join("power/runtime_status"), status);
        put(&device.join("gpu_metrics"), metrics);
    }

    fn v13() -> [u8; 120] {
        let mut bytes = [0_u8; 120];
        bytes[..4].copy_from_slice(&[120, 0, 1, 3]);
        bytes[4..6].copy_from_slice(&65_u16.to_le_bytes());
        bytes[22..24].copy_from_slice(&180_u16.to_le_bytes());
        bytes
    }

    #[test]
    fn suspended_dgpu_is_not_opened_until_active_and_cards_are_filtered() {
        let dir = tempfile::tempdir().unwrap();
        card(dir.path(), 0, b"suspended\n", &v13());
        fs::create_dir_all(dir.path().join("card0-DP-1/device")).unwrap();
        fs::create_dir_all(dir.path().join("renderD128/device")).unwrap();
        let mut collector = AmdSmuCollector::with_root(PathBuf::from(dir.path()));
        assert_eq!(collector.discover(&Env::default()), Health::Ok);
        fs::remove_file(dir.path().join("card0/device/gpu_metrics")).unwrap();
        let mut frame = MonitorFrame::default();
        assert!(collector.sample(&TickCtx::default(), &mut frame).is_ok());
        assert_eq!(frame.parts.amd_smu.cards.len(), 1);
        assert!(frame.parts.amd_smu.cards[0].suspended);
        assert_eq!(
            frame.parts.amd_smu.cards[0].decoded,
            AmdSmuStatus::Suspended
        );
        put(
            &dir.path().join("card0/device/power/runtime_status"),
            b"active\n",
        );
        put(&dir.path().join("card0/device/gpu_metrics"), &v13());
        assert!(collector.sample(&TickCtx::default(), &mut frame).is_ok());
        assert_eq!(
            frame.parts.amd_smu.cards[0].metrics.temperature_edge,
            Some(6500)
        );
    }

    #[test]
    fn apu_drops_runtime_gate_and_no_hardware_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        card(dir.path(), 1, b"active\n", &v3());
        let mut collector = AmdSmuCollector::with_root(PathBuf::from(dir.path()));
        assert_eq!(collector.discover(&Env::default()), Health::Ok);
        let gate = dir.path().join("card1/device/power/runtime_status");
        let mut frame = MonitorFrame::default();
        assert!(collector.sample(&TickCtx::default(), &mut frame).is_ok());
        assert!(collector.cards[0].gate.is_none());
        put(&gate, b"suspended\n");
        assert!(collector.sample(&TickCtx::default(), &mut frame).is_ok());
        assert!(!frame.parts.amd_smu.cards[0].suspended);
        let empty = tempfile::tempdir().unwrap();
        let mut none = AmdSmuCollector::with_root(PathBuf::from(empty.path()));
        assert_eq!(
            none.discover(&Env::default()),
            Health::Absent(Absent::NoHardware)
        );
    }

    #[test]
    fn card_cap_and_per_card_failure_do_not_hide_healthy_cards() {
        let dir = tempfile::tempdir().unwrap();
        for id in 0..9 {
            card(dir.path(), id, b"active\n", &v3());
        }
        let metrics_path = dir.path().join("card0/device/gpu_metrics");
        fs::remove_file(&metrics_path).unwrap();
        fs::create_dir(&metrics_path).unwrap();
        let mut collector = AmdSmuCollector::with_root(PathBuf::from(dir.path()));
        assert_eq!(collector.discover(&Env::default()), Health::Ok);
        let mut frame = MonitorFrame::default();
        assert!(collector.sample(&TickCtx::default(), &mut frame).is_ok());
        assert_eq!(frame.parts.amd_smu.cards.len(), 8);
        assert!(matches!(
            frame.parts.amd_smu.cards[0].decoded,
            AmdSmuStatus::Error(_)
        ));
        assert!(matches!(
            frame.parts.amd_smu.cards[1].decoded,
            AmdSmuStatus::Decoded(_)
        ));
        assert!(matches!(
            frame.parts.amd_smu.cards[0].decoded,
            AmdSmuStatus::Error(std::io::ErrorKind::IsADirectory)
        ));
    }
}
