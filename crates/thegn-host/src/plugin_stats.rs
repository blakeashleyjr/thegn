use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::thread::JoinHandle;

pub(crate) const MAX_ROWS: usize = 64;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PluginCounters {
    pub plugin: String,
    pub bridge: thegn_svc::plugin::BridgeStats,
    pub in_flight: usize,
    pub rejected: u64,
    pub timed_out: u64,
    pub lost: u64,
    pub queue_drops: u64,
    pub coalesced: u64,
}

impl From<&PluginCounters> for thegn_core::plugin_status::PluginStatusRow {
    fn from(row: &PluginCounters) -> Self {
        Self {
            plugin: row.plugin.clone(),
            in_flight: row.in_flight as u64,
            rejected: row.rejected,
            timed_out: row.timed_out,
            lost: row.lost,
            queue_drops: row.queue_drops,
            coalesced: row.coalesced,
            bridge: thegn_core::plugin_status::BridgeCounts {
                calls: row.bridge.calls,
                timed_out: row.bridge.timed_out,
                cancelled: row.bridge.cancelled,
                late_replies: row.bridge.late_replies,
                malformed: row.bridge.malformed,
                closed_wakeups: row.bridge.closed_wakeups,
            },
        }
    }
}

/// Overlay each live bridge's counters onto the published rows (one bridge
/// per provider plugin, keyed by plugin id). A plugin whose bridge counters are
/// all zero adds no row; rows stay sorted by id and capped at [`MAX_ROWS`].
pub(crate) fn merge_bridge(
    rows: Vec<PluginCounters>,
    bridge: &BTreeMap<String, thegn_svc::plugin::BridgeStats>,
) -> Vec<PluginCounters> {
    let mut rows: BTreeMap<String, PluginCounters> = rows
        .into_iter()
        .map(|row| (row.plugin.clone(), row))
        .collect();
    for (plugin, stats) in bridge {
        let has_displayed_counts = stats.timed_out > 0
            || stats.cancelled > 0
            || stats.late_replies > 0
            || stats.malformed > 0
            || stats.closed_wakeups > 0;
        if has_displayed_counts {
            rows.entry(plugin.clone())
                .or_insert_with(|| PluginCounters {
                    plugin: plugin.clone(),
                    ..Default::default()
                })
                .bridge = *stats;
        }
    }
    rows.into_values().take(MAX_ROWS).collect()
}

#[derive(Debug, Default)]
pub(crate) struct Totals(BTreeMap<String, PluginCounters>);

impl Totals {
    pub(crate) fn add_dispatch(
        &mut self,
        plugin: &str,
        rejected: usize,
        timed_out: usize,
        unavailable: usize,
        abandoned: usize,
        undelivered: usize,
    ) {
        let row = self
            .0
            .entry(plugin.to_owned())
            .or_insert_with(|| PluginCounters {
                plugin: plugin.to_owned(),
                ..Default::default()
            });
        row.rejected = row.rejected.saturating_add(rejected as u64);
        row.timed_out = row.timed_out.saturating_add(timed_out as u64);
        row.lost = row
            .lost
            .saturating_add(unavailable as u64)
            .saturating_add(abandoned as u64)
            .saturating_add(undelivered as u64);
    }

    pub(crate) fn add_inbox(
        &mut self,
        plugin: &str,
        coalesced: usize,
        dropped: usize,
        rejected: usize,
    ) {
        let row = self
            .0
            .entry(plugin.to_owned())
            .or_insert_with(|| PluginCounters {
                plugin: plugin.to_owned(),
                ..Default::default()
            });
        row.coalesced = row.coalesced.saturating_add(coalesced as u64);
        row.queue_drops = row
            .queue_drops
            .saturating_add(dropped as u64)
            .saturating_add(rejected as u64);
    }

    pub(crate) fn retain(&mut self, keep: impl Fn(&str) -> bool) {
        self.0.retain(|plugin, _| keep(plugin));
    }

    /// Rows to publish: plugins with any nonzero total or in-flight count,
    /// sorted by id and capped at [`MAX_ROWS`].
    pub(crate) fn rows(&self, in_flight: &BTreeMap<String, usize>) -> Vec<PluginCounters> {
        let mut rows = BTreeMap::new();
        for (plugin, totals) in &self.0 {
            rows.insert(plugin.clone(), totals.clone());
        }
        for (plugin, count) in in_flight {
            rows.entry(plugin.clone())
                .and_modify(|row: &mut PluginCounters| row.in_flight = *count)
                .or_insert_with(|| PluginCounters {
                    plugin: plugin.clone(),
                    in_flight: *count,
                    ..Default::default()
                });
        }
        rows.into_values()
            .filter(|row| {
                row.in_flight > 0
                    || row.rejected > 0
                    || row.timed_out > 0
                    || row.lost > 0
                    || row.queue_drops > 0
            })
            .take(MAX_ROWS)
            .collect()
    }
}

static ROWS: LazyLock<Mutex<Arc<Vec<PluginCounters>>>> =
    LazyLock::new(|| Mutex::new(Arc::new(Vec::new())));
static WRITER: OnceLock<Sender<Arc<Vec<PluginCounters>>>> = OnceLock::new();

#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn publish(rows: Vec<PluginCounters>) -> bool {
    let mut published = ROWS.lock().unwrap_or_else(|error| error.into_inner());
    if **published == rows {
        false
    } else {
        *published = Arc::new(rows);
        if let Some(tx) = WRITER.get()
            && let Err(error) = tx.send(Arc::clone(&published))
        {
            tracing::debug!(target: "thegn::plugin", %error, "plugin status snapshot send failed");
        }
        true
    }
}

pub(crate) fn snapshot() -> Arc<Vec<PluginCounters>> {
    Arc::clone(&ROWS.lock().unwrap_or_else(|error| error.into_inner()))
}

pub(crate) fn install_status_writer(state_dir: PathBuf) -> bool {
    let mut installed = false;
    let tx = WRITER.get_or_init(|| {
        let (tx, handle) = spawn_status_writer(state_dir);
        drop(handle);
        installed = true;
        tx
    });
    if installed && let Err(error) = tx.send(snapshot()) {
        tracing::debug!(target: "thegn::plugin", %error, "plugin status snapshot send failed");
    }
    installed
}

fn spawn_status_writer(state_dir: PathBuf) -> (Sender<Arc<Vec<PluginCounters>>>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let handle = thegn_core::thread_name::spawn_named("thegn-plugstat", move || {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
        while let Ok(mut rows) = rx.recv() {
            while let Ok(newer) = rx.try_recv() {
                rows = newer;
            }
            if let Err(error) = write_status_file(
                &state_dir,
                &rows,
                std::process::id(),
                thegn_core::util::now_ms(),
            ) {
                tracing::debug!(target: "thegn::plugin", %error, "plugin status snapshot not written");
            }
        }
    });
    (tx, handle)
}

fn write_status_file(
    state_dir: &Path,
    rows: &[PluginCounters],
    pid: u32,
    now_ms: i64,
) -> std::io::Result<()> {
    let snapshot = thegn_core::plugin_status::PluginStatusSnapshot {
        schema: thegn_core::plugin_status::STATUS_SCHEMA,
        pid,
        published_at_ms: now_ms,
        rows: rows.iter().map(Into::into).collect(),
    };
    let bytes = thegn_core::plugin_status::encode(&snapshot)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    crate::daemon::control_publish::prepare_run_dir(state_dir)?;
    thegn_core::fsperm::write_owner_only_atomic(
        &crate::daemon::control_publish::run_dir(state_dir)
            .join(thegn_core::plugin_status::STATUS_FILE),
        &bytes,
    )
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_ROWS, PluginCounters, Totals, merge_bridge, publish, snapshot, spawn_status_writer,
        write_status_file,
    };
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use thegn_core::plugin_status::{BridgeCounts, PluginStatusRow, STATUS_FILE, decode};
    use thegn_svc::plugin::BridgeStats;

    #[test]
    fn totals_accumulate_across_flushes_and_never_drain() {
        let mut totals = Totals::default();
        totals.add_dispatch("a", 1, 2, 3, 4, 5);
        totals.add_dispatch("a", 1, 2, 3, 4, 5);
        totals.add_inbox("a", 7, 1, 2);
        let row = &totals.rows(&BTreeMap::new())[0];
        assert_eq!(row.rejected, 2);
        assert_eq!(row.timed_out, 4);
        assert_eq!(row.lost, 24);
        assert_eq!(row.queue_drops, 3);
        assert_eq!(row.coalesced, 7);
        assert_eq!(totals.rows(&BTreeMap::new())[0].lost, 24);
    }

    #[test]
    fn rows_omit_all_zero_plugins_sort_by_id_and_cap_at_max_rows() {
        let mut totals = Totals::default();
        totals.add_dispatch("z", 0, 0, 0, 0, 0);
        totals.add_inbox("coalesced-only", 1, 0, 0);
        assert!(totals.rows(&BTreeMap::new()).is_empty());
        totals.add_dispatch("b", 1, 0, 0, 0, 0);
        totals.add_dispatch("a", 1, 0, 0, 0, 0);
        assert_eq!(
            totals
                .rows(&BTreeMap::new())
                .iter()
                .map(|row| row.plugin.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );

        let mut many = Totals::default();
        for index in 0..(MAX_ROWS + 10) {
            many.add_dispatch(&format!("plugin-{index:03}"), 1, 0, 0, 0, 0);
        }
        let rows = many.rows(&BTreeMap::new());
        assert_eq!(rows.len(), MAX_ROWS);
        assert_eq!(rows[0].plugin, "plugin-000");
        assert_eq!(rows[MAX_ROWS - 1].plugin, "plugin-063");
    }

    #[test]
    fn rows_carry_in_flight_for_a_plugin_with_no_totals() {
        let totals = Totals::default();
        let mut active = BTreeMap::new();
        active.insert("p".to_owned(), 2);
        assert_eq!(totals.rows(&active)[0].in_flight, 2);
        active.insert("p".to_owned(), 0);
        assert!(totals.rows(&active).is_empty());
    }

    #[test]
    fn retain_drops_uninstalled_plugins() {
        let mut totals = Totals::default();
        totals.add_dispatch("a", 1, 0, 0, 0, 0);
        totals.add_dispatch("b", 1, 0, 0, 0, 0);
        totals.retain(|plugin| plugin == "a");
        assert_eq!(totals.rows(&BTreeMap::new())[0].plugin, "a");
    }

    #[test]
    fn publish_reports_change_only_when_rows_differ() {
        let _guard = super::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let plugin = format!("pub-test-{}", std::process::id());
        let row = PluginCounters {
            plugin: plugin.clone(),
            rejected: 1,
            ..Default::default()
        };
        assert!(publish(vec![row.clone()]));
        assert!(!publish(vec![row.clone()]));
        let changed = PluginCounters {
            rejected: 2,
            ..row.clone()
        };
        assert!(publish(vec![changed.clone()]));
        assert!(snapshot().iter().any(|published| published == &changed));
        publish(Vec::new());
    }

    #[test]
    fn merge_bridge_overlays_counters_and_skips_all_zero_bridges() {
        let rows = vec![PluginCounters {
            plugin: "m".into(),
            rejected: 1,
            ..Default::default()
        }];
        let bridge = BTreeMap::from([
            ("a".into(), BridgeStats::default()),
            (
                "b".into(),
                BridgeStats {
                    timed_out: 2,
                    ..Default::default()
                },
            ),
            (
                "c".into(),
                BridgeStats {
                    timed_out: 1,
                    ..Default::default()
                },
            ),
            (
                "z".into(),
                BridgeStats {
                    calls: 1,
                    ..Default::default()
                },
            ),
        ]);

        let merged = merge_bridge(rows, &bridge);
        assert_eq!(
            merged
                .iter()
                .map(|row| row.plugin.as_str())
                .collect::<Vec<_>>(),
            ["b", "c", "m"]
        );
        assert_eq!(merged[0].bridge.timed_out, 2);
        assert!(!merged.iter().any(|row| row.plugin == "a"));
        assert!(!merged.iter().any(|row| row.plugin == "z"));
    }

    #[test]
    fn merge_bridge_caps_at_max_rows() {
        let bridge = (0..(MAX_ROWS + 5))
            .map(|index| {
                (
                    format!("plugin-{index:03}"),
                    BridgeStats {
                        timed_out: 1,
                        ..Default::default()
                    },
                )
            })
            .collect();

        let merged = merge_bridge(Vec::new(), &bridge);
        assert_eq!(merged.len(), MAX_ROWS);
        assert_eq!(merged[0].plugin, "plugin-000");
        assert_eq!(merged[MAX_ROWS - 1].plugin, "plugin-063");
    }

    #[test]
    fn status_row_conversion_copies_every_counter() {
        let counters = PluginCounters {
            plugin: "counter-test".to_owned(),
            in_flight: 1,
            rejected: 2,
            timed_out: 3,
            lost: 4,
            queue_drops: 5,
            coalesced: 6,
            bridge: BridgeStats {
                calls: 7,
                timed_out: 8,
                cancelled: 9,
                late_replies: 10,
                malformed: 11,
                closed_wakeups: 12,
            },
        };
        let status = PluginStatusRow::from(&counters);
        assert_eq!(status.plugin, "counter-test");
        assert_eq!(status.in_flight, 1);
        assert_eq!(status.rejected, 2);
        assert_eq!(status.timed_out, 3);
        assert_eq!(status.lost, 4);
        assert_eq!(status.queue_drops, 5);
        assert_eq!(status.coalesced, 6);
        assert_eq!(
            status.bridge,
            BridgeCounts {
                calls: 7,
                timed_out: 8,
                cancelled: 9,
                late_replies: 10,
                malformed: 11,
                closed_wakeups: 12,
            }
        );
    }

    #[test]
    fn status_writer_writes_the_newest_rows_and_exits_when_the_sender_drops() {
        let dir = tempfile::tempdir_in("/tmp").expect("private status tempdir");
        let (tx, handle) = spawn_status_writer(dir.path().to_path_buf());
        for rejected in [1, 4, 9] {
            tx.send(Arc::new(vec![PluginCounters {
                plugin: "newest".to_owned(),
                rejected,
                ..Default::default()
            }]))
            .expect("send status rows");
        }
        drop(tx);
        handle.join().expect("writer thread");

        let bytes =
            std::fs::read(dir.path().join("run").join(STATUS_FILE)).expect("status snapshot file");
        let decoded = decode(&bytes).expect("decode status snapshot");
        assert_eq!(decoded.pid, std::process::id());
        assert_eq!(
            decoded.rows,
            vec![PluginStatusRow {
                plugin: "newest".to_owned(),
                rejected: 9,
                ..Default::default()
            }]
        );
    }

    #[test]
    fn status_file_is_replaced_not_appended() {
        let dir = tempfile::tempdir_in("/tmp").expect("private status tempdir");
        let first = PluginCounters {
            plugin: "first".to_owned(),
            rejected: 1,
            ..Default::default()
        };
        let second = PluginCounters {
            plugin: "second".to_owned(),
            rejected: 2,
            ..Default::default()
        };
        write_status_file(dir.path(), std::slice::from_ref(&first), 7, 10)
            .expect("write initial snapshot");
        write_status_file(dir.path(), std::slice::from_ref(&second), 8, 20)
            .expect("replace snapshot");
        let bytes =
            std::fs::read(dir.path().join("run").join(STATUS_FILE)).expect("status snapshot file");
        let decoded = decode(&bytes).expect("decode status snapshot");
        assert_eq!(decoded.pid, 8);
        assert_eq!(decoded.published_at_ms, 20);
        assert_eq!(decoded.rows, vec![PluginStatusRow::from(&second)]);
    }

    #[test]
    fn publish_without_an_installed_writer_still_only_updates_memory() {
        let _guard = super::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir_in("/tmp").expect("private status tempdir");
        let file = dir.path().join("run").join(STATUS_FILE);
        let row = PluginCounters {
            plugin: format!("status-memory-{}", std::process::id()),
            rejected: 1,
            ..Default::default()
        };
        assert!(publish(vec![row.clone()]));
        assert!(!publish(vec![row.clone()]));
        assert!(snapshot().iter().any(|published| published == &row));
        assert!(!file.exists());
        publish(Vec::new());
    }

    #[test]
    fn status_writer_failure_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir_in("/tmp").expect("private status tempdir");
        let state_file = dir.path().join("state-file");
        std::fs::write(&state_file, b"not a directory").expect("create file state path");
        let result = write_status_file(&state_file, &[], 1, 2);
        assert!(result.is_err());
    }
}
