//! Daily, best-effort check for a newer stable GitHub release.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

const RELEASE_URL: &str = "https://api.github.com/repos/blakeashleyjr/thegn/releases/latest";
const CACHE_TTL_SECS: u64 = 24 * 60 * 60;

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
struct Cache {
    checked_at: i64,
    latest: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Notice {
    pub version: String,
    pub command: String,
}

#[derive(Default)]
struct NoticeState {
    notice: Option<Notice>,
    dismissed: bool,
}

fn state() -> &'static Mutex<NoticeState> {
    static STATE: OnceLock<Mutex<NoticeState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(NoticeState::default()))
}

trait ReleaseSource {
    fn latest(&self) -> Result<String, ()>;
}

struct GithubRelease;

impl ReleaseSource for GithubRelease {
    fn latest(&self) -> Result<String, ()> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(5))
            .user_agent(concat!("thegn/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| ())?;
        let response = client.get(RELEASE_URL).send().map_err(|_| ())?;
        if !response.status().is_success() {
            return Err(());
        }
        #[derive(serde::Deserialize)]
        struct Release {
            tag_name: String,
        }
        let release: Release = response.json().map_err(|_| ())?;
        Ok(release.tag_name.trim_start_matches('v').to_owned())
    }
}

fn cache_path() -> PathBuf {
    thegn_core::util::xdg_state_home()
        .join("thegn")
        .join("update-check.json")
}

fn read_cache(path: &Path) -> Option<Cache> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn write_cache(path: &Path, cache: &Cache) {
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(bytes) = serde_json::to_vec(cache) else { return };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(tmp, path);
    }
}

fn cache_fresh(cache: &Cache, now: i64) -> bool {
    now >= cache.checked_at && now.saturating_sub(cache.checked_at) < CACHE_TTL_SECS as i64
}

fn latest_version<S: ReleaseSource>(source: &S, path: &Path, now: i64) -> Option<String> {
    let cached = read_cache(path);
    if let Some(cache) = cached.as_ref().filter(|cache| cache_fresh(cache, now)) {
        return Some(cache.latest.clone());
    }
    match source.latest() {
        Ok(latest) => {
            write_cache(
                path,
                &Cache {
                    checked_at: now,
                    latest: latest.clone(),
                },
            );
            Some(latest)
        }
        Err(()) => cached.map(|cache| cache.latest),
    }
}

/// Compare numeric stable / alpha versions in the form used by thegn releases.
fn is_newer(latest: &str, running: &str) -> bool {
    fn parse(value: &str) -> Option<(u64, u64, u64, Option<u64>)> {
        let (base, pre) = value
            .split_once('-')
            .map_or((value, None), |(a, b)| (a, Some(b)));
        let mut parts = base.split('.').map(str::parse::<u64>);
        let major = parts.next()?.ok()?;
        let minor = parts.next()?.ok()?;
        let patch = parts.next()?.ok()?;
        if parts.next().is_some() {
            return None;
        }
        let alpha = match pre {
            None => None,
            Some(label) => Some(label.strip_prefix("alpha.")?.parse().ok()?),
        };
        Some((major, minor, patch, alpha))
    }
    match (parse(latest), parse(running)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

pub(crate) fn install_command(executable: &Path) -> String {
    let path = executable.to_string_lossy();
    if path.starts_with("/nix/store/") {
        "nix profile upgrade thegn".into()
    } else if path.starts_with("/opt/homebrew/")
        || path.starts_with("/usr/local/Cellar/")
        || path.starts_with("/usr/local/opt/")
    {
        "brew upgrade thegn".into()
    } else {
        "just update stable (or download from https://github.com/blakeashleyjr/thegn/releases)"
            .into()
    }
}

fn update_from<S: ReleaseSource>(
    source: &S,
    path: &Path,
    running: &str,
    now: i64,
) -> Option<Notice> {
    let latest = latest_version(source, path, now)?;
    is_newer(&latest, running).then(|| Notice {
        version: latest,
        command: std::env::current_exe()
            .ok()
            .map(|path| install_command(&path))
            .unwrap_or_else(|| "download from the release page".into()),
    })
}

pub(crate) fn current() -> Option<Notice> {
    let guard = state().lock().unwrap_or_else(|e| e.into_inner());
    (!guard.dismissed).then(|| guard.notice.clone()).flatten()
}

pub(crate) fn dismiss() {
    let mut guard = state().lock().unwrap_or_else(|e| e.into_inner());
    guard.dismissed = true;
}

pub(crate) fn cached_notice() -> Option<Notice> {
    if crate::channel_state::current() == thegn_core::channel::Channel::Dev {
        return None;
    }
    let cache = read_cache(&cache_path())?;
    is_newer(&cache.latest, env!("CARGO_PKG_VERSION")).then(|| Notice {
        version: cache.latest,
        command: std::env::current_exe()
            .ok()
            .map(|path| install_command(&path))
            .unwrap_or_else(|| "download from the release page".into()),
    })
}

pub(crate) fn check_now() -> Option<Notice> {
    update_from(
        &GithubRelease,
        &cache_path(),
        env!("CARGO_PKG_VERSION"),
        thegn_core::util::now(),
    )
}

/// Run on a detached OS thread: client construction, disk access, and HTTP are
/// all outside startup and the render loop. The five-second timeout bounds it.
pub(crate) fn spawn(
    enabled: bool,
    tx: tokio::sync::mpsc::UnboundedSender<crate::hydrate::RefreshKind>,
    waker: termwiz::terminal::TerminalWaker,
) {
    if !enabled || crate::channel_state::current() == thegn_core::channel::Channel::Dev {
        return;
    }
    std::thread::spawn(move || {
        let notice = update_from(
            &GithubRelease,
            &cache_path(),
            env!("CARGO_PKG_VERSION"),
            thegn_core::util::now(),
        );
        let mut guard = state().lock().unwrap_or_else(|e| e.into_inner());
        guard.notice = notice;
        drop(guard);
        if tx.send(crate::hydrate::RefreshKind::UpdateNotice).is_ok() {
            let _ = waker.wake();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fake(AtomicUsize, Result<String, ()>);
    impl ReleaseSource for Fake {
        fn latest(&self) -> Result<String, ()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            self.1.clone()
        }
    }

    #[test]
    fn compares_alpha_numbers_and_stable_releases() {
        assert!(is_newer("0.1.0-alpha.10", "0.1.0-alpha.9"));
        assert!(!is_newer("0.1.0-alpha.9", "0.1.0-alpha.10"));
        assert!(is_newer("0.1.0", "0.1.0-alpha.99"));
        assert!(!is_newer("0.1.0-alpha.2", "0.1.0"));
    }

    #[test]
    fn cache_ttl_skips_fetch_until_a_day_has_passed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update.json");
        write_cache(
            &path,
            &Cache {
                checked_at: 100,
                latest: "1.2.3".into(),
            },
        );
        let source = Fake(AtomicUsize::new(0), Ok("9.9.9".into()));
        assert_eq!(
            latest_version(&source, &path, 100 + CACHE_TTL_SECS as i64 - 1).as_deref(),
            Some("1.2.3")
        );
        assert_eq!(source.0.load(Ordering::SeqCst), 0);
        assert_eq!(
            latest_version(&source, &path, 100 + CACHE_TTL_SECS as i64).as_deref(),
            Some("9.9.9")
        );
        assert_eq!(source.0.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn detects_brew_nix_and_manual_installs() {
        assert_eq!(
            install_command(Path::new("/opt/homebrew/bin/thegn")),
            "brew upgrade thegn"
        );
        assert_eq!(
            install_command(Path::new("/nix/store/abc-thegn/bin/thegn")),
            "nix profile upgrade thegn"
        );
        assert!(
            install_command(Path::new("/home/user/bin/thegn"))
                .starts_with("just update stable")
        );
    }
}
