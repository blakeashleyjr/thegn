use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock, Mutex};

pub(crate) const MAX_ROWS: usize = 64;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PluginCounters {
    pub plugin: String,
    pub in_flight: usize,
    pub rejected: u64,
    pub timed_out: u64,
    pub lost: u64,
    pub queue_drops: u64,
    pub coalesced: u64,
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

#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn publish(rows: Vec<PluginCounters>) -> bool {
    let mut published = ROWS.lock().unwrap_or_else(|error| error.into_inner());
    if **published == rows {
        false
    } else {
        *published = Arc::new(rows);
        true
    }
}

pub(crate) fn snapshot() -> Arc<Vec<PluginCounters>> {
    Arc::clone(&ROWS.lock().unwrap_or_else(|error| error.into_inner()))
}

#[cfg(test)]
mod tests {
    use super::{MAX_ROWS, PluginCounters, Totals, publish, snapshot};
    use std::collections::BTreeMap;

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
}
