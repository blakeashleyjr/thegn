//! Off-loop orchestration for notification sounds.
//!
//! The compositor only resolves a pure [`SoundRef`] and does a bounded
//! `try_send`. Pack inspection, provider probing, filesystem checks, and child
//! processes all happen on the named utility worker.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use termwiz::terminal::TerminalWaker;
use thegn_core::config::SoundConfig;
use thegn_core::notification_route::SoundEmit;
use thegn_core::notification_sound::SoundRef;
use thegn_core::seam::ProbeReport;

pub(crate) const QUEUE_DEPTH: usize = 32;

#[derive(Debug)]
enum SoundJob {
    File { path: PathBuf, volume: f32 },
    Command(String),
}

struct PlaybackSnapshot {
    provider: Option<Box<dyn crate::platform::sound::SoundPlayer>>,
    pack: Option<PathBuf>,
    entries: BTreeMap<String, PathBuf>,
    pack_entry_count: usize,
    files: BTreeMap<String, PathBuf>,
    provider_report: ProbeReport,
    fallback: Option<String>,
}

impl PlaybackSnapshot {
    fn empty() -> Self {
        Self {
            provider: None,
            pack: None,
            entries: BTreeMap::new(),
            pack_entry_count: 0,
            files: BTreeMap::new(),
            provider_report: ProbeReport::new(
                "sound",
                "none",
                thegn_core::seam::Availability::Unavailable("sound snapshot pending".into()),
            ),
            fallback: Some("sound snapshot is not loaded yet".into()),
        }
    }
}

/// Immutable-at-use-time provider and pack state shared by producers and the
/// worker. Replacing the `Arc` under the short mutex never makes a producer
/// inspect the filesystem.
pub(crate) struct SoundRuntime {
    snapshot: Mutex<Arc<PlaybackSnapshot>>,
    queue: Mutex<Option<std::sync::mpsc::SyncSender<SoundJob>>>,
    dropped: AtomicU64,
    fallback_bell: AtomicBool,
    waker: TerminalWaker,
}

impl SoundRuntime {
    pub(crate) fn new(waker: TerminalWaker) -> Arc<Self> {
        Arc::new(Self {
            snapshot: Mutex::new(Arc::new(PlaybackSnapshot::empty())),
            queue: Mutex::new(None),
            dropped: AtomicU64::new(0),
            fallback_bell: AtomicBool::new(false),
            waker,
        })
    }

    /// Build a fresh snapshot off the compositor loop, then swap it atomically.
    pub(crate) fn reload(self: &Arc<Self>, cfg: SoundConfig) {
        let runtime = Arc::clone(self);
        std::thread::Builder::new()
            .name("notify-sound-config".into())
            .spawn(move || {
                crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
                let snapshot = Arc::new(build_snapshot(&cfg));
                *runtime.snapshot.lock().unwrap() = snapshot;
            })
            // best-effort: a failed config worker leaves the last snapshot in place
            .ok();
    }

    pub(crate) fn enqueue(self: &Arc<Self>, emit: &SoundEmit) {
        let job = match emit {
            SoundEmit::File { sound_ref, volume } => {
                let snapshot = self.snapshot.lock().unwrap().clone();
                let Some(path) = resolve(sound_ref, &snapshot) else {
                    self.request_fallback("sound reference was not found in the configured pack");
                    return;
                };
                SoundJob::File {
                    path,
                    volume: *volume,
                }
            }
            SoundEmit::Command(command) => SoundJob::Command(command.clone()),
            SoundEmit::Bell => return,
        };

        let Some(tx) = self.ensure_worker() else {
            self.request_fallback("could not start notify-sound worker");
            return;
        };
        match tx.try_send(job) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::warn!(
                    target: "thegn::notify_sound",
                    dropped_total = dropped,
                    "sound queue full — dropped an audio job"
                );
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                self.request_fallback("notify-sound worker exited");
            }
        }
    }

    fn ensure_worker(self: &Arc<Self>) -> Option<std::sync::mpsc::SyncSender<SoundJob>> {
        let mut queue = self.queue.lock().unwrap();
        if let Some(tx) = queue.as_ref() {
            return Some(tx.clone());
        }
        let (tx, rx) = std::sync::mpsc::sync_channel(QUEUE_DEPTH);
        let runtime = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("notify-sound".into())
            .spawn(move || {
                crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
                while let Ok(job) = rx.recv() {
                    match job {
                        SoundJob::File { path, volume } => {
                            let snapshot = runtime.snapshot.lock().unwrap().clone();
                            let Some(provider) = snapshot.provider.as_ref() else {
                                runtime.request_fallback("no audio provider");
                                continue;
                            };
                            if !supported_format(&path, &provider.caps().formats) {
                                runtime.request_fallback("sound file format is unsupported");
                                continue;
                            }
                            if let Err(error) = provider.play(&path, volume) {
                                tracing::debug!(target: "thegn::notify_sound", %error, "audio provider failed");
                                runtime.request_fallback("audio provider failed");
                            }
                        }
                        SoundJob::Command(command) => {
                            run_command(&command);
                        }
                    }
                }
            });
        spawned.ok()?;
        *queue = Some(tx.clone());
        Some(tx)
    }

    fn request_fallback(&self, reason: &str) {
        tracing::debug!(target: "thegn::notify_sound", reason, "falling back to terminal bell");
        request_fallback_raw(&self.fallback_bell, &self.waker, reason);
    }

    pub(crate) fn take_fallback_bell(&self) -> bool {
        self.fallback_bell.swap(false, Ordering::Relaxed)
    }

    pub(crate) fn report(cfg: &SoundConfig) -> serde_json::Value {
        let snapshot = build_snapshot(cfg);
        serde_json::json!({
            "provider": snapshot.provider_report,
            "pack": snapshot.pack.as_ref().map(|p| p.display().to_string()),
            "pack_entries": snapshot.pack_entry_count,
            "fallback": snapshot.fallback,
        })
    }
}

fn request_fallback_raw(flag: &AtomicBool, waker: &TerminalWaker, reason: &str) {
    tracing::debug!(target: "thegn::notify_sound", reason, "sound degraded to terminal bell");
    flag.store(true, Ordering::Relaxed);
    let _ = waker.wake(); // best-effort: a fallback cue must not fail its producer
}

fn build_snapshot(cfg: &SoundConfig) -> PlaybackSnapshot {
    let provider = crate::platform::sound::provider();
    let provider_report = provider
        .as_ref()
        .map_or_else(crate::platform::sound::probe, |p| p.probe());
    let pack = (!cfg.pack.trim().is_empty())
        .then(|| PathBuf::from(thegn_core::util::expand_tilde(cfg.pack.trim())));
    let mut entries = BTreeMap::new();
    let mut pack_entry_count = 0;
    let mut files = BTreeMap::new();
    let mut fallback = None;
    if let Some(dir) = &pack {
        match std::fs::read_dir(dir) {
            Ok(read_dir) => {
                for item in read_dir.flatten() {
                    let path = item.path();
                    if !path.is_file() {
                        continue;
                    }
                    pack_entry_count += 1;
                    let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
                        continue;
                    };
                    entries
                        .entry(file_name.to_string())
                        .or_insert_with(|| path.clone());
                    if let Some(stem) = path.file_stem().and_then(|n| n.to_str()) {
                        entries.entry(stem.to_string()).or_insert(path);
                    }
                }
            }
            Err(error) => fallback = Some(format!("sound pack unavailable: {error}")),
        }
    }
    if provider.is_none() {
        fallback = Some("no supported audio player found; terminal bell is used".into());
    }
    for raw in cfg
        .per_kind
        .values()
        .chain(std::iter::once(&cfg.chime_file))
    {
        let Ok(SoundRef::File(path)) = SoundRef::parse(raw) else {
            continue;
        };
        let expanded = PathBuf::from(thegn_core::util::expand_tilde(&path));
        if expanded.is_file() {
            files.insert(path, expanded);
        } else {
            fallback.get_or_insert_with(|| "a configured sound file is missing".into());
        }
    }
    PlaybackSnapshot {
        provider,
        pack,
        entries,
        pack_entry_count,
        files,
        provider_report,
        fallback,
    }
}

fn resolve(sound_ref: &SoundRef, snapshot: &PlaybackSnapshot) -> Option<PathBuf> {
    match sound_ref {
        SoundRef::Off | SoundRef::Bell => None,
        SoundRef::Pack(name) => snapshot.entries.get(name).cloned(),
        SoundRef::File(path) => snapshot.files.get(path).cloned(),
    }
}

fn supported_format(path: &Path, formats: &[&str]) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    formats.iter().any(|f| f.eq_ignore_ascii_case(ext))
}

#[expect(clippy::disallowed_methods)]
fn run_command(command: &str) {
    let _ = std::process::Command::new("sh")
        .args(["-c", command])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status(); // best-effort: a configured command is an advisory sound
}

pub(crate) fn emit(runtime: &Arc<SoundRuntime>, emit: &SoundEmit) {
    runtime.enqueue(emit);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_snapshot_indexes_filename_and_stem() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("attention.wav"), b"RIFF").unwrap();
        let cfg = SoundConfig {
            pack: dir.path().display().to_string(),
            ..SoundConfig::default()
        };
        let snapshot = build_snapshot(&cfg);
        assert!(snapshot.entries.contains_key("attention.wav"));
        assert!(snapshot.entries.contains_key("attention"));
        assert_eq!(snapshot.entries.len(), 2);
        assert_eq!(snapshot.pack_entry_count, 1);
    }

    #[test]
    fn sound_mode_only_needs_a_worker_for_file_or_command_playback() {
        let bell = SoundConfig::default();
        assert!(!needs_worker(&bell));
        let command = SoundConfig {
            mode: thegn_core::config::SoundMode::Command,
            command: "true".into(),
            ..bell.clone()
        };
        assert!(needs_worker(&command));
    }

    fn needs_worker(cfg: &SoundConfig) -> bool {
        cfg.mode == thegn_core::config::SoundMode::Command
            || !cfg.chime_file.trim().is_empty()
            || cfg.per_kind.values().any(|value| {
                matches!(
                    SoundRef::parse(value),
                    Ok(SoundRef::File(_) | SoundRef::Pack(_))
                )
            })
    }
}
