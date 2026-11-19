//! Admission before provider access or ledger retirement. The legacy ledger
//! has no environment/account binding, so one provider kind must have one
//! unambiguous account reference and lifetime policy for automatic cleanup.
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use thegn_core::config::EnvProviderConfig;

pub(crate) const STALL_WARN_AFTER: Duration = Duration::from_secs(15 * 60);

/// Admission gate for the in-process lifetime of one provider reaper pass.
///
/// The process-shared [`ReaperLease`] below protects provider-kind ownership
/// across processes; this gate keeps a slow provider call from allowing the
/// hydration ticker to pile up passes in this process.
pub(crate) struct ReaperGate {
    last: Mutex<Option<Instant>>,
    started: Mutex<Option<Instant>>,
    in_flight: AtomicBool,
    stall_warned: AtomicBool,
}

impl ReaperGate {
    pub(crate) const fn new() -> Self {
        Self {
            last: Mutex::new(None),
            started: Mutex::new(None),
            in_flight: AtomicBool::new(false),
            stall_warned: AtomicBool::new(false),
        }
    }

    pub(crate) fn try_begin(&'static self, interval: Duration) -> Option<PassToken> {
        self.try_begin_at(Instant::now(), interval)
    }

    pub(crate) fn try_begin_at(
        &'static self,
        now: Instant,
        interval: Duration,
    ) -> Option<PassToken> {
        if self
            .in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return None;
        }

        let mut last = self
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if last.is_some_and(|previous| now < previous || now.duration_since(previous) < interval) {
            self.in_flight.store(false, Ordering::Release);
            return None;
        }
        *last = Some(now);
        drop(last);

        let mut started = self
            .started
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *started = Some(now);
        drop(started);
        self.stall_warned.store(false, Ordering::Release);
        Some(PassToken { gate: self })
    }

    pub(crate) fn in_flight(&self) -> bool {
        self.in_flight.load(Ordering::Acquire)
    }

    /// Return the live pass duration once it exceeds `limit`, at most once per
    /// pass. This latch lets every hydration tick check cheaply without
    /// repeating the warning while a provider call remains hung.
    pub(crate) fn stalled_for(&self, now: Instant, limit: Duration) -> Option<Duration> {
        if !self.in_flight() {
            return None;
        }
        let started = self
            .started
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let elapsed = started
            .and_then(|started| now.checked_duration_since(started))
            .filter(|elapsed| *elapsed > limit)?;
        if !self.stall_warned.swap(true, Ordering::AcqRel) {
            Some(elapsed)
        } else {
            None
        }
    }
}

pub(crate) struct PassToken {
    gate: &'static ReaperGate,
}

impl Drop for PassToken {
    fn drop(&mut self) {
        let mut started = self
            .gate
            .started
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *started = None;
        drop(started);
        self.gate.in_flight.store(false, Ordering::Release);
    }
}

/// Spawn a reaper pass on its named background thread, retaining the gate token
/// until the pass returns or unwinds.
pub(crate) fn spawn_pass(
    token: PassToken,
    thread_name: &'static str,
    pass: impl FnOnce() + Send + 'static,
) -> bool {
    thegn_core::thread_name::try_spawn_named(thread_name, move || {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
        let _token = token;
        pass();
    })
    .is_ok()
}

fn lock_dir() -> PathBuf {
    #[cfg(test)]
    {
        thread_local! {
            static DIR: tempfile::TempDir = tempfile::tempdir().expect("test reaper lock dir");
        }
        DIR.with(|dir| dir.path().join("locks"))
    }
    #[cfg(not(test))]
    {
        thegn_core::util::xdg_state_home()
            .join("thegn")
            .join("locks")
    }
}

fn ensure_dir(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    crate::platform::restrict_dir_owner_only(dir);
    Ok(())
}

/// A process-shared lease held for one provider kind's full reconcile pass.
pub(crate) struct ReaperLease {
    _file: File,
}

pub(crate) fn try_reaper_lease(kind: &str) -> Result<Option<ReaperLease>, String> {
    try_reaper_lease_in(&lock_dir(), kind)
}

fn try_reaper_lease_in(dir: &Path, kind: &str) -> Result<Option<ReaperLease>, String> {
    if kind.is_empty()
        || kind.len() > 32
        || !kind
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte))
    {
        return Err("invalid provider kind for reaper lock".into());
    }
    let fail = |error: std::io::Error| format!("cannot establish reaper lock: {error}");
    ensure_dir(dir).map_err(fail)?;
    let path = dir.join(format!("reaper-{kind}.lock"));
    let file = crate::platform::open_lock_file(&path).map_err(fail)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(ReaperLease { _file: file })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(fail(error)),
    }
}

pub(crate) fn with_unambiguous<'a, T>(
    envs: impl IntoIterator<Item = &'a EnvProviderConfig>,
    reconcile: impl FnOnce(&'a EnvProviderConfig) -> T,
) -> Result<Option<T>, &'static str> {
    let mut envs = envs.into_iter();
    let Some(first) = envs.next() else {
        return Ok(None);
    };
    if envs.any(|other| {
        other.provider.trim() != first.provider.trim()
            || other.api_base != first.api_base
            || other.api_key_env.trim() != first.api_key_env.trim()
            || other.max_lifetime_secs != first.max_lifetime_secs
    }) {
        return Err(
            "ambiguous environment account/lifetime policy; bind ownership or align configuration before automatic cleanup",
        );
    }
    Ok(Some(reconcile(first)))
}

/// Resources one reaper pass deliberately left in place (quarantined, refused, failed). Each line is logged as it happens
/// (`msg::warn`: the log and CLI keep it); the pass ends with ONE in-app Alert when the set of kept resources changed.
#[derive(Debug, Default)]
pub(crate) struct KeptReport {
    kept: Vec<(String, String)>, // (resource, message)
}

impl KeptReport {
    pub(crate) fn keep(&mut self, resource: &str, message: String) {
        thegn_core::msg::warn(&message);
        self.kept.push((resource.to_string(), message));
    }

    /// End of one provider kind's pass.
    pub(crate) fn publish(self, kind: &str) {
        let mut latch = kept_latch()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let notice = latch_kept_notice(&mut latch, kind, self.kept);
        drop(latch);
        if let Some(notice) = notice {
            deliver_notice(&mut LiveSink, kind, &notice);
        }
    }
}

/// A changed kept set: the alert text plus the sorted, deduplicated resource names.
struct KeptNotice {
    message: String,
    names: Vec<String>,
}

trait KeptSink {
    fn toast(&mut self, message: &str);
    /// True when a new notice was recorded (its toast then went through notification routing).
    fn record(&mut self, kind: &str, source_ref: &str, message: &str) -> bool;
}

struct LiveSink;

impl KeptSink for LiveSink {
    fn toast(&mut self, message: &str) {
        if let Some(state) = crate::notify::global() {
            state.emit_toast(message, thegn_core::notification::Priority::Alert);
        }
    }

    fn record(&mut self, kind: &str, source_ref: &str, message: &str) -> bool {
        let db = match thegn_core::db::Db::open() {
            Ok(db) => db,
            Err(error) => {
                tracing::debug!(target: "thegn::reaper", %error, "kept notice: db open failed");
                return false;
            }
        };
        match crate::automation_events::emit_once(
            &db,
            thegn_core::notification::NotificationKind::ResourceAlert.as_str(),
            source_ref,
            message,
            "",
        ) {
            Ok(inserted) => inserted,
            Err(error) => {
                tracing::debug!(target: "thegn::reaper", %error, kind, "kept notice: record failed");
                false
            }
        }
    }
}

fn deliver_notice(sink: &mut dyn KeptSink, kind: &str, notice: &KeptNotice) {
    let source_ref = kept_source_ref(kind, &notice.names);
    if !sink.record(kind, &source_ref, &notice.message) {
        // Not newly recorded (duplicate, no DB): routing raised no toast, so raise it directly.
        sink.toast(&notice.message);
    }
}

/// Stable (FNV-1a) identity of one kept set, independent of name order.
fn kept_source_ref(kind: &str, names: &[String]) -> String {
    let mut sorted: Vec<&str> = names.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.dedup();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for name in sorted {
        for byte in name.bytes().chain(std::iter::once(0u8)) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("reaper-kept:{kind}:{hash:016x}")
}

fn kept_latch() -> &'static std::sync::Mutex<std::collections::BTreeMap<String, Vec<String>>> {
    static LATCH: std::sync::OnceLock<
        std::sync::Mutex<std::collections::BTreeMap<String, Vec<String>>>,
    > = std::sync::OnceLock::new();
    LATCH.get_or_init(|| std::sync::Mutex::new(std::collections::BTreeMap::new()))
}

fn latch_kept_notice(
    latch: &mut std::collections::BTreeMap<String, Vec<String>>,
    kind: &str,
    mut kept: Vec<(String, String)>,
) -> Option<KeptNotice> {
    kept.sort();
    if kept.is_empty() {
        latch.remove(kind);
        return None;
    }

    let mut names = Vec::new();
    for (resource, _) in &kept {
        if names.last() != Some(resource) {
            names.push(resource.clone());
        }
    }
    if latch.get(kind) == Some(&names) {
        return None;
    }
    latch.insert(kind.to_string(), names.clone());
    let message = kept_alert_text(kind, names.len(), &kept[0].1);
    Some(KeptNotice { message, names })
}

#[cfg(test)]
fn latch_kept(
    latch: &mut std::collections::BTreeMap<String, Vec<String>>,
    kind: &str,
    kept: Vec<(String, String)>,
) -> Option<String> {
    latch_kept_notice(latch, kind, kept).map(|notice| notice.message)
}

fn kept_alert_text(kind: &str, count: usize, first_message: &str) -> String {
    if count == 1 {
        format!("{kind} reaper kept 1 resource for review: {first_message}")
    } else {
        format!(
            "{kind} reaper kept {count} resources for review: {first_message} (+{} more in the log)",
            count - 1
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeSink {
        toasts: usize,
        records: Vec<String>,
        inserted: bool,
    }

    impl KeptSink for FakeSink {
        fn toast(&mut self, _message: &str) {
            self.toasts += 1;
        }
        fn record(&mut self, _kind: &str, source_ref: &str, _message: &str) -> bool {
            self.records.push(source_ref.to_string());
            self.inserted
        }
    }

    fn pass(
        latch: &mut std::collections::BTreeMap<String, Vec<String>>,
        sink: &mut FakeSink,
        names: &[&str],
    ) {
        let kept = names
            .iter()
            .map(|n| (n.to_string(), format!("kept {n}")))
            .collect();
        if let Some(notice) = latch_kept_notice(latch, "fly", kept) {
            deliver_notice(sink, "fly", &notice);
        }
    }

    #[test]
    fn kept_source_ref_is_stable_and_order_independent() {
        let a = vec!["a".to_string(), "b".to_string()];
        let b = vec!["b".to_string(), "a".to_string()];
        assert_eq!(kept_source_ref("fly", &a), kept_source_ref("fly", &b));
        assert!(kept_source_ref("fly", &a).starts_with("reaper-kept:fly:"));
        assert_ne!(
            kept_source_ref("fly", &a),
            kept_source_ref("fly", &["a".to_string()])
        );
        assert_ne!(kept_source_ref("fly", &a), kept_source_ref("vps", &a));
        assert_ne!(
            kept_source_ref("fly", &["ab".to_string()]),
            kept_source_ref("fly", &["a".to_string(), "b".to_string()])
        );
    }

    #[test]
    fn publish_records_one_notice_per_changed_set() {
        let mut latch = std::collections::BTreeMap::new();
        let mut sink = FakeSink::default();
        pass(&mut latch, &mut sink, &["a"]);
        assert_eq!(sink.records.len(), 1);
        pass(&mut latch, &mut sink, &["a"]);
        assert_eq!(sink.records.len(), 1, "same set records nothing");
        pass(&mut latch, &mut sink, &["a", "b"]);
        assert_eq!(sink.records.len(), 2);
        assert_ne!(sink.records[0], sink.records[1]);
        pass(&mut latch, &mut sink, &[]);
        assert_eq!(sink.records.len(), 2, "cleared set records nothing");
        assert_eq!(sink.toasts, 2);
        let mut routed = FakeSink {
            inserted: true,
            ..FakeSink::default()
        };
        let mut fresh = std::collections::BTreeMap::new();
        pass(&mut fresh, &mut routed, &["a"]);
        assert_eq!(routed.records.len(), 1);
        assert_eq!(
            routed.toasts, 0,
            "a newly recorded notice toasts through routing only"
        );
    }

    #[test]
    fn ambiguity_precedes_inventory_deletion_and_ledger_retirement() {
        let first = EnvProviderConfig {
            provider: "fly".into(),
            api_key_env: "ACCOUNT_A".into(),
            max_lifetime_secs: 60,
            ..Default::default()
        };
        for field in ["lifetime", "credentials", "endpoint"] {
            let mut other = first.clone();
            match field {
                "lifetime" => other.max_lifetime_secs = 0,
                "credentials" => other.api_key_env = "ACCOUNT_B".into(),
                _ => other.api_base = "https://other.invalid".into(),
            }
            let mut actions = [0; 3];
            let result = with_unambiguous([&first, &other], |_| {
                actions[0] += 1; // inventory
                actions[1] += 1; // delete
                actions[2] += 1; // ledger retirement
            });
            assert!(result.is_err(), "{field}");
            assert_eq!(actions, [0; 3]);
        }
        let mut reconciliations = 0;
        with_unambiguous([&first, &first.clone()], |_| reconciliations += 1).unwrap();
        assert_eq!(reconciliations, 1, "identical policy reconciles once");
    }

    #[test]
    fn reaper_lease_is_single_flight_per_provider_kind() {
        let dir = tempfile::tempdir().unwrap();
        let first = try_reaper_lease_in(dir.path(), "hetzner").unwrap();
        assert!(first.is_some(), "first holder should acquire hetzner");

        let second = try_reaper_lease_in(dir.path(), "hetzner").unwrap();
        assert!(
            second.is_none(),
            "a second open file description must conflict"
        );
        let fly = try_reaper_lease_in(dir.path(), "fly").unwrap();
        assert!(
            fly.is_some(),
            "different provider kinds have separate locks"
        );

        drop(first);
        let after_release = try_reaper_lease_in(dir.path(), "hetzner").unwrap();
        assert!(
            after_release.is_some(),
            "dropping the holder releases its lock"
        );
    }

    #[test]
    fn reaper_gate_is_single_flight_and_interval_throttled() {
        static GATE: ReaperGate = ReaperGate::new();
        let start = Instant::now();
        let interval = Duration::from_secs(10);
        let token = GATE.try_begin_at(start, interval).expect("first pass");
        assert!(GATE.try_begin_at(start + interval, interval).is_none());
        drop(token);
        assert!(
            GATE.try_begin_at(start + interval - Duration::from_secs(1), interval)
                .is_none()
        );
        assert!(GATE.try_begin_at(start + interval, interval).is_some());
    }

    #[test]
    fn reaper_gate_releases_after_a_panic() {
        static GATE: ReaperGate = ReaperGate::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _token = GATE
                .try_begin_at(Instant::now(), Duration::from_secs(1))
                .expect("panic test pass");
            panic!("provider call panicked");
        }));
        assert!(result.is_err());
        assert!(!GATE.in_flight());
    }

    #[test]
    fn reaper_gate_stalled_for_table() {
        static GATE: ReaperGate = ReaperGate::new();
        let start = Instant::now();
        let limit = Duration::from_secs(10);
        assert_eq!(
            GATE.stalled_for(start + limit + Duration::from_secs(1), limit),
            None
        );
        let token = GATE
            .try_begin_at(start, Duration::from_secs(1))
            .expect("pass");
        assert_eq!(GATE.stalled_for(start, limit), None);
        assert_eq!(GATE.stalled_for(start + limit, limit), None);
        assert_eq!(
            GATE.stalled_for(start + limit + Duration::from_secs(1), limit),
            Some(Duration::from_secs(11))
        );
        assert_eq!(
            GATE.stalled_for(start + limit + Duration::from_secs(2), limit),
            None,
            "a pass warns about a stall only once"
        );
        drop(token);
        assert_eq!(
            GATE.stalled_for(start + limit + Duration::from_secs(2), limit),
            None
        );
    }

    #[test]
    fn reaper_lease_releases_when_a_pass_panics() {
        let dir = tempfile::tempdir().unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _lease = try_reaper_lease_in(dir.path(), "fly")
                .unwrap()
                .expect("panic test lease");
            panic!("provider call panicked");
        }));
        assert!(result.is_err());
        assert!(try_reaper_lease_in(dir.path(), "fly").unwrap().is_some());
    }

    #[test]
    fn reaper_lease_rejects_invalid_provider_kinds() {
        let dir = tempfile::tempdir().unwrap();
        for kind in ["../x", "", "a12345678901234567890123456789012"] {
            assert!(
                try_reaper_lease_in(dir.path(), kind).is_err(),
                "invalid kind should fail: {kind:?}"
            );
        }
    }

    #[test]
    fn reaper_lease_rejects_unusable_lock_directory() {
        let root = tempfile::tempdir().unwrap();
        let file_path = root.path().join("not-a-directory");
        std::fs::write(&file_path, b"file").unwrap();
        assert!(try_reaper_lease_in(&file_path, "fly").is_err());
    }

    #[test]
    fn kept_alert_is_silent_for_a_clean_pass() {
        let mut latch = std::collections::BTreeMap::new();
        assert_eq!(latch_kept(&mut latch, "hetzner", vec![]), None);
        assert!(!latch.contains_key("hetzner"));
    }

    #[test]
    fn kept_alert_fires_once_for_the_same_kept_set() {
        let mut latch = std::collections::BTreeMap::new();
        let first = vec![
            ("vm-b".to_string(), "msg b".to_string()),
            ("vm-a".to_string(), "msg a".to_string()),
        ];
        assert!(latch_kept(&mut latch, "hetzner", first).is_some());
        let second = vec![
            ("vm-a".to_string(), "changed a".to_string()),
            ("vm-b".to_string(), "changed b".to_string()),
        ];
        assert_eq!(latch_kept(&mut latch, "hetzner", second), None);
    }

    #[test]
    fn kept_alert_fires_again_when_the_kept_set_changes() {
        let mut latch = std::collections::BTreeMap::new();
        let first = vec![
            ("vm-a".to_string(), "msg a".to_string()),
            ("vm-b".to_string(), "msg b".to_string()),
        ];
        assert!(latch_kept(&mut latch, "hetzner", first.clone()).is_some());
        let mut changed = first;
        changed.push(("vm-c".to_string(), "msg c".to_string()));
        assert!(latch_kept(&mut latch, "hetzner", changed).is_some());
    }

    #[test]
    fn kept_alert_rearms_after_a_clean_pass() {
        let mut latch = std::collections::BTreeMap::new();
        let kept = vec![("vm-a".to_string(), "msg a".to_string())];
        assert!(latch_kept(&mut latch, "hetzner", kept.clone()).is_some());
        assert_eq!(latch_kept(&mut latch, "hetzner", vec![]), None);
        assert!(latch_kept(&mut latch, "hetzner", kept).is_some());
    }

    #[test]
    fn kept_alert_names_the_count_and_first_line() {
        let mut latch = std::collections::BTreeMap::new();
        let multiple = vec![
            ("vm-b".to_string(), "msg b".to_string()),
            ("vm-a".to_string(), "msg a".to_string()),
            ("vm-a".to_string(), "msg a2".to_string()),
        ];
        assert_eq!(
            latch_kept(&mut latch, "hetzner", multiple).as_deref(),
            Some("hetzner reaper kept 2 resources for review: msg a (+1 more in the log)")
        );
        assert_eq!(
            latch_kept(
                &mut latch,
                "digitalocean",
                vec![("vm-a".to_string(), "single line".to_string())]
            )
            .as_deref(),
            Some("digitalocean reaper kept 1 resource for review: single line")
        );
    }

    #[test]
    fn kept_alert_latches_per_kind() {
        let mut latch = std::collections::BTreeMap::new();
        let kept = vec![("vm-a".to_string(), "msg a".to_string())];
        assert!(latch_kept(&mut latch, "hetzner", kept.clone()).is_some());
        assert!(latch_kept(&mut latch, "fly", kept.clone()).is_some());
        assert_eq!(latch_kept(&mut latch, "hetzner", kept.clone()), None);
        assert_eq!(latch_kept(&mut latch, "fly", kept), None);
    }
}
