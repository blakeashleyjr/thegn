//! Loop-side cache and async result fence for user-opened palette DB reads.

use std::collections::{HashMap, VecDeque};

use crate::palette::PaletteItem;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum PaletteKind {
    Main,
    Account,
    Bundle,
    Identity,
    WorkspaceSwitch,
    MoveToFolder,
    WorkspacePickerSeed,
    LayoutNames,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct PaletteKey {
    pub(crate) kind: PaletteKind,
    pub(crate) context: String,
}

impl PaletteKey {
    pub(crate) fn cache_id(&self) -> String {
        format!("{:?}:{}", self.kind, self.context)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PaletteRows {
    Items(Vec<PaletteItem>),
    MoveToFolder {
        items: Vec<PaletteItem>,
        repo_path: String,
    },
    PickerSeed(Vec<String>, std::collections::HashSet<String>),
    LayoutNames(Vec<String>),
}

#[derive(Debug)]
pub(crate) struct PaletteDbResult {
    pub(crate) key: PaletteKey,
    pub(crate) generation: u64,
    pub(crate) rows: Option<PaletteRows>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaletteOpenPlan {
    pub(crate) generation: u64,
    pub(crate) rows: PaletteRows,
    pub(crate) load_requested: bool,
}

#[derive(Debug, Default)]
pub(crate) struct PaletteDbCache {
    entries: HashMap<PaletteKey, PaletteRows>,
    move_folder_lru: VecDeque<PaletteKey>,
    generation: u64,
    current: Option<(PaletteKey, u64)>,
}

impl PaletteDbCache {
    pub(crate) fn contains(&self, key: &PaletteKey) -> bool {
        self.entries.contains_key(key)
    }

    pub(crate) fn begin(&mut self, key: PaletteKey, fallback: PaletteRows) -> (u64, PaletteRows) {
        let plan = self.plan_open(key, fallback);
        if plan.load_requested {
            (plan.generation, plan.rows)
        } else {
            self.current = None;
            (plan.generation, plan.rows)
        }
    }

    pub(crate) fn plan_open(&mut self, key: PaletteKey, fallback: PaletteRows) -> PaletteOpenPlan {
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let rows = self.entries.get(&key).cloned().unwrap_or(fallback);
        if key.kind == PaletteKind::MoveToFolder {
            self.move_folder_lru.retain(|entry| entry != &key);
            self.move_folder_lru.push_back(key.clone());
        }
        self.current = Some((key, generation));
        PaletteOpenPlan {
            generation,
            rows,
            load_requested: true,
        }
    }

    pub(crate) fn apply(&mut self, result: PaletteDbResult) -> Option<PaletteRows> {
        if let Some(rows) = result.rows.clone() {
            if result.key.kind != PaletteKind::MoveToFolder {
                self.entries
                    .retain(|key, _| key.kind != result.key.kind || key == &result.key);
            }
            self.entries.insert(result.key.clone(), rows);
        }
        if result.key.kind == PaletteKind::MoveToFolder {
            self.move_folder_lru.retain(|entry| entry != &result.key);
            self.move_folder_lru.push_back(result.key.clone());
            while self.move_folder_lru.len() > 8 {
                if let Some(oldest) = self.move_folder_lru.pop_front() {
                    self.entries.remove(&oldest);
                }
            }
        }
        match (&self.current, result.rows) {
            (Some((key, generation)), Some(rows))
                if key == &result.key && *generation == result.generation =>
            {
                Some(rows)
            }
            _ => None,
        }
    }

    pub(crate) fn is_current(&self, key: &PaletteKey, generation: u64) -> bool {
        self.current.as_ref() == Some(&(key.clone(), generation))
    }
}

pub(crate) fn spawn_load(
    key: PaletteKey,
    generation: u64,
    tx: &tokio::sync::mpsc::UnboundedSender<PaletteDbResult>,
    waker: &termwiz::terminal::TerminalWaker,
    build: impl FnOnce(&thegn_core::db::Db) -> PaletteRows + Send + 'static,
) {
    let tx = tx.clone();
    let wake = waker.clone();
    let fallback_tx = tx.clone();
    let fallback_wake = wake.clone();
    let fallback_key = key.clone();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    crate::db_task::persist(move |db| {
        let rows = build(db);
        if let Err(error) = tx.send(PaletteDbResult {
            key,
            generation,
            rows: Some(rows),
        }) {
            tracing::debug!(target: "thegn::db", %error, "palette DB result receiver is gone");
        }
        if let Err(error) = wake.wake() {
            tracing::debug!(target: "thegn::db", %error, "palette DB result wake failed");
        }
        if done_tx.send(()).is_err() {
            tracing::debug!(target: "thegn::db", "palette DB completion receiver is gone");
        }
    });
    tokio::spawn(async move {
        if done_rx.await.is_err() {
            if let Err(error) = fallback_tx.send(dropped_job_result(fallback_key, generation)) {
                tracing::debug!(target: "thegn::db", %error, "palette DB fallback receiver is gone");
            }
            if let Err(error) = fallback_wake.wake() {
                tracing::debug!(target: "thegn::db", %error, "palette DB fallback wake failed");
            }
        }
    });
}

fn dropped_job_result(key: PaletteKey, generation: u64) -> PaletteDbResult {
    PaletteDbResult {
        key,
        generation,
        rows: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(kind: PaletteKind, context: &str) -> PaletteKey {
        PaletteKey {
            kind,
            context: context.to_owned(),
        }
    }

    #[test]
    fn cache_hit_opens_with_cached_rows_and_pending() {
        let mut cache = PaletteDbCache::default();
        let address = key(PaletteKind::Main, "session");
        let rows = PaletteRows::Items(vec![PaletteItem::new("cached", "Cached")]);
        cache.entries.insert(address.clone(), rows.clone());
        let plan = cache.plan_open(address.clone(), PaletteRows::Items(Vec::new()));
        assert_eq!(plan.rows, rows);
        assert!(plan.load_requested);
        let mut session = crate::search_everywhere::PaletteSession::new(vec![PaletteItem::new(
            "cached", "Cached",
        )]);
        session.mark_db_pending(address, plan.generation);
        assert!(session.db_pending);
    }

    #[test]
    fn stale_generation_result_only_refreshes_cache() {
        let mut cache = PaletteDbCache::default();
        let address = key(PaletteKind::Main, "session");
        let (generation, _) = cache.begin(address.clone(), PaletteRows::Items(Vec::new()));
        let result = PaletteDbResult {
            key: address.clone(),
            generation: generation.wrapping_sub(1),
            rows: Some(PaletteRows::Items(vec![PaletteItem::new("x", "X")])),
        };
        assert!(cache.apply(result).is_none());
        assert!(cache.entries.contains_key(&address));
    }

    #[test]
    fn move_to_folder_cache_is_bounded_lru() {
        let mut cache = PaletteDbCache::default();
        for i in 0..10 {
            let address = key(PaletteKind::MoveToFolder, &i.to_string());
            let (generation, _) = cache.begin(address.clone(), PaletteRows::Items(Vec::new()));
            cache.apply(PaletteDbResult {
                key: address,
                generation,
                rows: Some(PaletteRows::Items(Vec::new())),
            });
        }
        assert_eq!(cache.move_folder_lru.len(), 8);
        assert!(
            !cache
                .entries
                .contains_key(&key(PaletteKind::MoveToFolder, "0"))
        );
        assert!(
            cache
                .entries
                .contains_key(&key(PaletteKind::MoveToFolder, "9"))
        );
    }

    #[test]
    fn dropped_job_fallback_is_a_fenced_pending_clear_result() {
        let mut cache = PaletteDbCache::default();
        let address = key(PaletteKind::Main, "session");
        let (generation, _) = cache.begin(address.clone(), PaletteRows::Items(Vec::new()));
        let fallback = dropped_job_result(address.clone(), generation);
        assert!(cache.is_current(&fallback.key, fallback.generation));
        assert!(fallback.rows.is_none());
    }
}
