//! Separate model artifact store: immutable revision + SHA-256, no executable
//! extraction, with a larger explicit size budget than managed-tool downloads.
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

pub const REVISION: &str = "5359861c739e955e79d9a303bcbc70fb988958b1";
const TEMP_PREFIX: &str = ".thegn-voice-model-";
const MARKER_TEMP_PREFIX: &str = ".thegn-voice-marker-";
const STALE_TEMP_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct Model {
    pub name: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
}
pub const MODELS: &[Model] = &[
    Model {
        name: "tiny",
        bytes: 77691713,
        sha256: "be07e048e1e599ad46341c8d2a135645097a538221678b7acdd1b1919c6e1b21",
    },
    Model {
        name: "tiny.en",
        bytes: 77704715,
        sha256: "921e4cf8686fdd993dcd081a5da5b6c365bfde1162e72b08d75ac75289920b1f",
    },
    Model {
        name: "base",
        bytes: 147951465,
        sha256: "60ed5bc3dd14eea856493d334349b405782ddcaf0028d4b5df4088345fba2efe",
    },
    Model {
        name: "base.en",
        bytes: 147964211,
        sha256: "a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002",
    },
    Model {
        name: "small",
        bytes: 487601967,
        sha256: "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b",
    },
    Model {
        name: "small.en",
        bytes: 487614201,
        sha256: "c6138d6d58ecc8322097e0f987c32f1be8bb0a18532a3f88f734d1bbf9c41e5d",
    },
];
pub fn model(name: &str) -> Result<&'static Model> {
    MODELS
        .iter()
        .find(|m| m.name == name)
        .context("unknown model; use voice model list")
}
pub fn store_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| thegn_core::util::home().join(".local/share"))
        .join("thegn/voice/models")
}
pub fn path_in(dir: &Path, name: &str) -> Result<PathBuf> {
    model(name)?;
    Ok(dir.join(format!("ggml-{name}.bin")))
}
pub fn resolve(cfg: &thegn_core::config::VoiceConfig) -> Result<PathBuf> {
    if let Some(path) = &cfg.model_path {
        return Ok(PathBuf::from(thegn_core::util::expand_tilde(path)));
    }
    path_in(&store_dir(), &cfg.model)
}
pub fn verify(path: &Path, artifact: &Model, cancel: &AtomicBool) -> Result<()> {
    verify_with(path, artifact, || hash_file(path, cancel))
}

fn verify_with(path: &Path, artifact: &Model, hash: impl FnOnce() -> Result<String>) -> Result<()> {
    let metadata = std::fs::metadata(path)?;
    if metadata.len() != artifact.bytes {
        bail!("model size mismatch; retry voice model pull");
    }
    if read_marker(path).is_some_and(|marker| marker_matches(&marker, &metadata, artifact)) {
        return Ok(());
    }
    if hash()? != artifact.sha256 {
        bail!("model SHA-256 mismatch; retry voice model pull");
    }
    if let Err(error) = write_marker(path, artifact, &metadata) {
        tracing::debug!(%error, "voice model marker not written");
    }
    Ok(())
}

/// Remove a catalogued model, its verification marker and abandoned downloads.
pub fn remove(dir: &Path, name: &str) -> Result<()> {
    sweep_temps(dir)?;
    let path = path_in(dir, name)?;
    remove_if_present(&path)?;
    remove_if_present(&marker_path(&path))
}

fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn hash_file(path: &Path, cancel: &AtomicBool) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        if cancel.load(Ordering::Acquire) {
            bail!("model verification cancelled");
        }
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct VerifiedMarker {
    bytes: u64,
    modified_secs: u64,
    modified_nanos: u32,
    device: Option<u64>,
    inode: Option<u64>,
    changed_secs: Option<i64>,
    changed_nanos: Option<i64>,
    sha256: String,
}

fn marker_path(path: &Path) -> PathBuf {
    path.with_extension("verified.json")
}

fn marker_metadata(metadata: &std::fs::Metadata) -> Option<(u64, u32)> {
    let modified = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    Some((modified.as_secs(), modified.subsec_nanos()))
}

fn read_marker(path: &Path) -> Option<VerifiedMarker> {
    serde_json::from_slice(&std::fs::read(marker_path(path)).ok()?).ok()
}

fn marker_matches(marker: &VerifiedMarker, metadata: &std::fs::Metadata, artifact: &Model) -> bool {
    let (device, inode, changed_secs, changed_nanos) =
        crate::plugin::platform::voice_model_identity_metadata(metadata);
    marker_metadata(metadata).is_some_and(|(secs, nanos)| {
        marker.bytes == metadata.len()
            && marker.modified_secs == secs
            && marker.modified_nanos == nanos
            && marker.device == device
            && marker.inode == inode
            && marker.changed_secs == changed_secs
            && marker.changed_nanos == changed_nanos
            && marker.sha256 == artifact.sha256
    })
}

fn write_marker(path: &Path, artifact: &Model, metadata: &std::fs::Metadata) -> Result<()> {
    let Some((modified_secs, modified_nanos)) = marker_metadata(metadata) else {
        bail!("model modification time unavailable");
    };
    let (device, inode, changed_secs, changed_nanos) =
        crate::plugin::platform::voice_model_identity_metadata(metadata);
    let marker = VerifiedMarker {
        bytes: metadata.len(),
        modified_secs,
        modified_nanos,
        device,
        inode,
        changed_secs,
        changed_nanos,
        sha256: artifact.sha256.into(),
    };
    let parent = path.parent().context("model path has no parent")?;
    std::fs::create_dir_all(parent)?;
    let mut temp = tempfile::Builder::new()
        .prefix(MARKER_TEMP_PREFIX)
        .tempfile_in(parent)?;
    serde_json::to_writer(&mut temp, &marker)?;
    temp.as_file().sync_all()?;
    temp.persist(marker_path(path))
        .map_err(|error| error.error)?;
    Ok(())
}

/// Remove abandoned downloads owned by this store before a new pull or rm.
pub fn sweep_temps(dir: &Path) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX)
            && entry.file_type()?.is_file()
            && entry
                .metadata()?
                .modified()?
                .elapsed()
                .is_ok_and(|age| age >= STALE_TEMP_AGE)
        {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn begin_pull(dir: &Path) -> Result<tempfile::NamedTempFile> {
    std::fs::create_dir_all(dir)?;
    sweep_temps(dir)?;
    Ok(tempfile::Builder::new()
        .prefix(TEMP_PREFIX)
        .tempfile_in(dir)?)
}
/// Install from a stream; used by the network path and corrupt/interrupted fixtures.
pub fn install(
    dir: &Path,
    artifact: &Model,
    mut reader: impl Read,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, u64),
) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let mut temp = tempfile::Builder::new()
        .prefix(TEMP_PREFIX)
        .tempfile_in(dir)?;
    let mut buf = [0u8; 64 * 1024];
    let mut total = 0;
    loop {
        if cancel.load(Ordering::Acquire) {
            bail!("model download cancelled");
        }
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > artifact.bytes {
            bail!("model download exceeds pinned size");
        }
        temp.write_all(&buf[..n])?;
        progress(total, artifact.bytes);
    }
    let path = dir.join(format!("ggml-{}.bin", artifact.name));
    finish_install(temp, &path, artifact, cancel)
}

fn finish_install(
    temp: tempfile::NamedTempFile,
    path: &Path,
    artifact: &Model,
    cancel: &AtomicBool,
) -> Result<PathBuf> {
    if temp.as_file().metadata()?.len() != artifact.bytes
        || hash_file(temp.path(), cancel)? != artifact.sha256
    {
        bail!("model verification failed; retry voice model pull");
    }
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|e| e.error)?;
    if let Err(error) = write_marker(path, artifact, &std::fs::metadata(path)?) {
        tracing::debug!(%error, "voice model marker not written");
    }
    Ok(path.to_path_buf())
}

/// Async network streaming allows cancellation even while the peer is stalled.
pub fn pull(
    dir: &Path,
    artifact: &Model,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, u64),
) -> Result<PathBuf> {
    let mut temp = begin_pull(dir)?;
    crate::voice_runtime::run(async {
        let operation = async {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(1800))
                .connect_timeout(std::time::Duration::from_secs(15))
                .https_only(true)
                .build()?;
            let url = format!(
                "https://huggingface.co/ggerganov/whisper.cpp/resolve/{REVISION}/ggml-{}.bin",
                artifact.name
            );
            let mut response = client.get(url).send().await?.error_for_status()?;
            if response
                .content_length()
                .is_some_and(|n| n != artifact.bytes)
            {
                bail!("model download size does not match manifest");
            }
            let mut total = 0;
            while let Some(chunk) = response.chunk().await? {
                total += chunk.len() as u64;
                if total > artifact.bytes {
                    bail!("model download exceeds pinned size");
                }
                temp.write_all(&chunk)?;
                progress(total, artifact.bytes);
            }
            Ok::<_, anyhow::Error>(())
        };
        tokio::pin!(operation);
        loop {
            if cancel.load(Ordering::Acquire) {
                bail!("model download cancelled");
            }
            tokio::select! {
                result = &mut operation => return result,
                _ = tokio::signal::ctrl_c() => {
                    cancel.store(true, Ordering::Release);
                    bail!("model download cancelled")
                },
                _ = tokio::time::sleep(std::time::Duration::from_millis(25)) => {}
            }
        }
    })??;
    let path = path_in(dir, artifact.name)?;
    finish_install(temp, &path, artifact, cancel)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn installation_is_atomic_and_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let cancelled = AtomicBool::new(false);
        let m = Model {
            name: "fixture",
            bytes: 3,
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        };
        let path = install(dir.path(), &m, &b"abc"[..], &cancelled, |_, _| {}).unwrap();
        for data in [&b"bad"[..], &b"ab"[..], &b"abcd"[..]] {
            assert!(install(dir.path(), &m, data, &cancelled, |_, _| {}).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), b"abc");
        }
        cancelled.store(true, Ordering::Release);
        assert!(install(dir.path(), &m, &b"abc"[..], &cancelled, |_, _| {}).is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }
    #[test]
    fn catalog_rejects_path_traversal() {
        assert!(path_in(Path::new("/tmp"), "../model").is_err());
    }
    #[test]
    fn interrupted_and_midstream_cancelled_installs_leave_no_artifact() {
        struct Interrupted(bool);
        impl Read for Interrupted {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.0 {
                    return Err(std::io::Error::from(std::io::ErrorKind::ConnectionReset));
                }
                self.0 = true;
                buf[..2].copy_from_slice(b"ab");
                Ok(2)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let cancelled = AtomicBool::new(false);
        let m = Model {
            name: "fixture",
            bytes: 3,
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        };
        let model_path = install(dir.path(), &m, &b"abc"[..], &cancelled, |_, _| {}).unwrap();
        assert!(install(dir.path(), &m, Interrupted(false), &cancelled, |_, _| {}).is_err());
        assert_eq!(std::fs::read(&model_path).unwrap(), b"abc");
        assert!(
            install(dir.path(), &m, &b"abc"[..], &cancelled, |_, _| cancelled
                .store(true, Ordering::Release))
            .is_err()
        );
        assert_eq!(std::fs::read(&model_path).unwrap(), b"abc");
    }

    #[test]
    fn unchanged_models_skip_hash_and_changed_models_are_reverified() {
        let dir = tempfile::tempdir().unwrap();
        let artifact = Model {
            name: "fixture",
            bytes: 3,
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        };
        let path = install(
            dir.path(),
            &artifact,
            &b"abc"[..],
            &AtomicBool::new(false),
            |_, _| {},
        )
        .unwrap();
        let hashes = std::cell::Cell::new(0);
        verify_with(&path, &artifact, || {
            hashes.set(hashes.get() + 1);
            Ok(artifact.sha256.into())
        })
        .unwrap();
        assert_eq!(hashes.get(), 0, "unchanged marker must skip SHA-256");
        let metadata = std::fs::metadata(&path).unwrap();
        let (modified_secs, modified_nanos) = marker_metadata(&metadata).unwrap();
        let stale_marker = VerifiedMarker {
            bytes: metadata.len(),
            modified_secs: modified_secs.saturating_add(1),
            modified_nanos,
            sha256: artifact.sha256.into(),
        };
        std::fs::write(
            marker_path(&path),
            serde_json::to_vec(&stale_marker).unwrap(),
        )
        .unwrap();
        std::fs::write(&path, b"xyz").unwrap();
        let result = verify_with(&path, &artifact, || {
            hashes.set(hashes.get() + 1);
            Ok("bad".into())
        });
        assert!(result.is_err(), "changed file must be hashed and refused");
        assert_eq!(hashes.get(), 1);
    }

    #[test]
    fn pull_preparation_removes_only_stale_download_temps() {
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join(format!("{TEMP_PREFIX}leftover"));
        std::fs::write(&stale, b"partial download").unwrap();
        let old = std::time::SystemTime::now() - STALE_TEMP_AGE - std::time::Duration::from_secs(1);
        std::fs::File::open(&stale)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
        let fresh = dir.path().join(format!("{TEMP_PREFIX}in-flight"));
        std::fs::write(&fresh, b"active download").unwrap();
        let marker_temp = dir.path().join(format!("{MARKER_TEMP_PREFIX}in-flight"));
        std::fs::write(&marker_temp, b"active marker").unwrap();
        let download = begin_pull(dir.path()).unwrap();
        assert!(!stale.exists(), "pull preparation removes stale downloads");
        assert!(
            fresh.exists(),
            "pull preparation preserves another active download"
        );
        assert!(
            marker_temp.exists(),
            "pull preparation preserves marker writes"
        );
        drop(download);
    }

    #[test]
    fn verified_marker_rejects_same_size_same_mtime_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let artifact = Model {
            name: "fixture",
            bytes: 3,
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        };
        let path = install(
            dir.path(),
            &artifact,
            &b"abc"[..],
            &AtomicBool::new(false),
            |_, _| {},
        )
        .unwrap();
        let before = std::fs::metadata(&path).unwrap();
        let (device, inode, changed_secs, changed_nanos) =
            crate::plugin::platform::voice_model_identity_metadata(&before);
        if device.is_none() || inode.is_none() || changed_secs.is_none() || changed_nanos.is_none()
        {
            return;
        }
        let modified = before.modified().unwrap();
        let replacement = dir.path().join("replacement");
        std::fs::write(&replacement, b"xyz").unwrap();
        std::fs::File::open(&replacement)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        let after = std::fs::metadata(&path).unwrap();
        assert_eq!(before.len(), after.len());
        assert_eq!(marker_metadata(&before), marker_metadata(&after));
        assert!(!marker_matches(
            &read_marker(&path).unwrap(),
            &after,
            &artifact
        ));
    }

    #[test]
    fn verification_succeeds_when_marker_cannot_be_written() {
        let dir = tempfile::tempdir().unwrap();
        let artifact = Model {
            name: "fixture",
            bytes: 3,
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        };
        let path = dir.path().join("ggml-fixture.bin");
        std::fs::write(&path, b"abc").unwrap();
        std::fs::create_dir(marker_path(&path)).unwrap();
        assert!(verify(&path, &artifact, &AtomicBool::new(false)).is_ok());
    }

    #[test]
    fn remove_deletes_model_marker_and_stale_temp_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ggml-tiny.bin"), b"model").unwrap();
        std::fs::write(dir.path().join("ggml-tiny.verified.json"), b"marker").unwrap();
        let stale = dir.path().join(format!("{TEMP_PREFIX}stale"));
        std::fs::write(&stale, b"partial").unwrap();
        let old = std::time::SystemTime::now() - STALE_TEMP_AGE - std::time::Duration::from_secs(1);
        std::fs::File::open(&stale)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
        remove(dir.path(), "tiny").unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        remove(dir.path(), "tiny").unwrap();
    }
}
