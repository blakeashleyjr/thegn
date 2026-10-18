//! Loop-owned bridge from the existing stats ticker and monitor frames into
//! the bounded multi-resolution monitor history.

use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(test)]
use thegn_core::series_store::SeriesId;
use thegn_core::series_store::{CAP_A, CAP_B, Class, SeriesKey, SeriesStore};
use thegn_metrics::StatsSnapshot;
use thegn_metrics::collect::{CollectorId, frame::MonitorFrame};

use crate::telemetry::Metric;

pub(crate) const FAMILY_MASTHEAD: u8 = 1;
pub(crate) const FAMILY_PSI: u8 = 2;
pub(crate) const FAMILY_CPU_STAT: u8 = 3;
const TOTAL_INSTANCE: u16 = u16::MAX;
const CORE_ID_WORDS: usize = (u16::MAX as usize + 1).div_ceil(64);

const _: () = assert!(Metric::ALL.len() + 6 + 1 <= CAP_A);
const _: () = assert!(CAP_B <= u16::MAX as usize + 1);

#[derive(Debug, Clone, Copy, Default)]
enum CachedId {
    #[default]
    Unseen,
    Ready(SeriesId),
    Refused,
}

#[derive(Debug, Clone, Copy, Default)]
struct CoreId {
    id: u16,
    series: CachedId,
}

/// History for the lifetime of the host loop, independent of the overlay.
#[derive(Debug)]
pub(crate) struct MonitorHistory {
    store: SeriesStore,
    masthead_ids: [CachedId; Metric::ALL.len()],
    psi_ids: [CachedId; 6],
    cpu_total_id: CachedId,
    core_ids: [CoreId; CAP_B],
    core_len: usize,
    refused_core_ids: [u64; CORE_ID_WORDS],
    frame_owned_second: [Option<u64>; Metric::ALL.len()],
    refused_interns: u64,
    rejected_pushes: u64,
    intern_scans: u64,
}

impl MonitorHistory {
    pub(crate) fn new() -> Self {
        Self {
            store: SeriesStore::new(),
            masthead_ids: [CachedId::Unseen; Metric::ALL.len()],
            psi_ids: [CachedId::Unseen; 6],
            cpu_total_id: CachedId::Unseen,
            core_ids: [CoreId::default(); CAP_B],
            core_len: 0,
            refused_core_ids: [0; CORE_ID_WORDS],
            frame_owned_second: [None; Metric::ALL.len()],
            refused_interns: 0,
            rejected_pushes: 0,
            intern_scans: 0,
        }
    }

    pub(crate) fn ingest_masthead(&mut self, snap: &StatsSnapshot, at_ms: u64) {
        let second = at_ms / 1_000;
        for metric in Metric::ALL {
            let index = metric.index();
            if self.frame_owned_second[index].is_some_and(|owned| owned >= second) {
                continue;
            }
            let value = metric.read(snap);
            if !value.is_finite() && matches!(self.masthead_ids[index], CachedId::Unseen) {
                continue;
            }
            let key = SeriesKey {
                family: FAMILY_MASTHEAD,
                metric: index as u16,
                instance: 0,
            };
            let id = self.resolve_masthead(index, key);
            self.push(id, at_ms, value);
        }
    }

    pub(crate) fn ingest_frame(&mut self, frame: &MonitorFrame, wall_ms: u64) {
        let second = wall_ms / 1_000;
        if frame.sampled.contains(CollectorId::Loadavg) {
            let index = Metric::Load.index();
            if let Some(load) = frame.parts.loadavg.value {
                let key = SeriesKey {
                    family: FAMILY_MASTHEAD,
                    metric: index as u16,
                    instance: 0,
                };
                let id = self.resolve_masthead(index, key);
                self.push(id, wall_ms, load.load1 as f32 / 100.0);
                self.frame_owned_second[index] = Some(second);
            } else if !matches!(self.masthead_ids[index], CachedId::Unseen) {
                self.push(self.masthead_ids[index], wall_ms, f32::NAN);
            }
        }

        if frame.sampled.contains(CollectorId::Psi) {
            let resources = [
                &frame.parts.psi.cpu,
                &frame.parts.psi.memory,
                &frame.parts.psi.io,
            ];
            for (resource, part) in resources.into_iter().enumerate() {
                let lines = [part.file.some, part.file.full];
                for (kind, line) in lines.into_iter().enumerate() {
                    let slot = resource * 2 + kind;
                    let key = SeriesKey {
                        family: FAMILY_PSI,
                        metric: slot as u16,
                        instance: 0,
                    };
                    let value = if part.present {
                        line.map_or(f32::NAN, |line| line.avg10 as f32 / 100.0)
                    } else {
                        f32::NAN
                    };
                    if value.is_finite() || !matches!(self.psi_ids[slot], CachedId::Unseen) {
                        let id = self.resolve_psi(slot, key);
                        self.push(id, wall_ms, value);
                    }
                }
            }
        }

        if frame.sampled.contains(CollectorId::CpuStat) {
            let key = SeriesKey {
                family: FAMILY_CPU_STAT,
                metric: 0,
                instance: TOTAL_INSTANCE,
            };
            let total = frame
                .parts
                .cpu_stat
                .total
                .map_or(f32::NAN, |share| f32::from(share.busy) / 10.0);
            if total.is_finite() || !matches!(self.cpu_total_id, CachedId::Unseen) {
                let total_id = self.resolve_cpu_total(key);
                self.push(total_id, wall_ms, total);
            }

            for core in &frame.parts.cpu_stat.cores {
                let value = if core.online {
                    core.share
                        .map_or(f32::NAN, |share| f32::from(share.busy) / 10.0)
                } else {
                    f32::NAN
                };
                if value.is_finite() || !matches!(self.cached_core(core.id), CachedId::Unseen) {
                    let id = self.resolve_core(core.id);
                    self.push(id, wall_ms, value);
                }
            }
        }
    }

    fn resolve_masthead(&mut self, index: usize, key: SeriesKey) -> CachedId {
        Self::resolve(
            &mut self.store,
            &mut self.intern_scans,
            &mut self.refused_interns,
            &mut self.masthead_ids[index],
            key,
            Class::A,
        )
    }

    fn resolve_psi(&mut self, index: usize, key: SeriesKey) -> CachedId {
        Self::resolve(
            &mut self.store,
            &mut self.intern_scans,
            &mut self.refused_interns,
            &mut self.psi_ids[index],
            key,
            Class::A,
        )
    }

    fn resolve_cpu_total(&mut self, key: SeriesKey) -> CachedId {
        Self::resolve(
            &mut self.store,
            &mut self.intern_scans,
            &mut self.refused_interns,
            &mut self.cpu_total_id,
            key,
            Class::A,
        )
    }

    fn resolve(
        store: &mut SeriesStore,
        scans: &mut u64,
        refused: &mut u64,
        cache: &mut CachedId,
        key: SeriesKey,
        class: Class,
    ) -> CachedId {
        if matches!(cache, CachedId::Unseen) {
            *scans = scans.saturating_add(1);
            *cache = match store.intern(key, class) {
                Some(id) => CachedId::Ready(id),
                None => {
                    *refused = refused.saturating_add(1);
                    CachedId::Refused
                }
            };
        }
        *cache
    }

    fn resolve_core(&mut self, core_id: u16) -> CachedId {
        let cached = self.cached_core(core_id);
        if !matches!(cached, CachedId::Unseen) {
            return cached;
        }
        if core_id == TOTAL_INSTANCE {
            let word = usize::from(core_id) / 64;
            let bit = 1_u64 << (core_id % 64);
            if self.refused_core_ids[word] & bit == 0 {
                self.refused_core_ids[word] |= bit;
                self.refused_interns = self.refused_interns.saturating_add(1);
            }
            return CachedId::Refused;
        }
        if self.core_len == CAP_B {
            self.refused_core_ids[usize::from(core_id) / 64] |= 1_u64 << (core_id % 64);
            self.refused_interns = self.refused_interns.saturating_add(1);
            return CachedId::Refused;
        }
        let key = SeriesKey {
            family: FAMILY_CPU_STAT,
            metric: 0,
            instance: core_id,
        };
        let id = Self::resolve(
            &mut self.store,
            &mut self.intern_scans,
            &mut self.refused_interns,
            &mut self.core_ids[self.core_len].series,
            key,
            Class::B,
        );
        self.core_ids[self.core_len].id = core_id;
        self.core_len += 1;
        id
    }

    fn cached_core(&self, core_id: u16) -> CachedId {
        if self.refused_core_ids[usize::from(core_id) / 64] & (1_u64 << (core_id % 64)) != 0 {
            return CachedId::Refused;
        }
        self.core_ids[..self.core_len]
            .iter()
            .find(|entry| entry.id == core_id)
            .map_or(CachedId::Unseen, |entry| entry.series)
    }

    fn push(&mut self, id: CachedId, at_ms: u64, value: f32) {
        if let CachedId::Ready(id) = id
            && !self.store.push(id, at_ms, value)
        {
            self.rejected_pushes = self.rejected_pushes.saturating_add(1);
        }
    }

    #[cfg(test)]
    fn id_for(&self, key: SeriesKey) -> Option<SeriesId> {
        self.store.id_of(key)
    }

    #[cfg(test)]
    fn store(&self) -> &SeriesStore {
        &self.store
    }
}

pub(crate) fn wall_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            duration
                .as_secs()
                .saturating_mul(1_000)
                .saturating_add(u64::from(duration.subsec_millis()))
        })
}

#[cfg(test)]
mod history_tests;
