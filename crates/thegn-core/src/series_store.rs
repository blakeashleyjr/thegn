//! Bounded, allocation-stable time series history for the system monitor.
//!
//! T0 retains one value per second, T1 retains five-second mean/max pairs, and
//! T2 retains minute mean/max pairs. Callers provide timestamps; this module
//! does not read a clock or perform I/O.

use crate::series_window::Window;

pub const T0_LEN: usize = 600;
pub const T1_LEN: usize = 720;
pub const T2_LEN: usize = 1440;
pub const C_LEN: usize = 120;
pub const T0_MS: u64 = 1_000;
pub const T1_MS: u64 = 5_000;
pub const T2_MS: u64 = 60_000;
pub const CAP_A: usize = 96;
pub const CAP_B: usize = 400;
pub const CAP_C: usize = 512;
pub const HISTORY_BUDGET: usize = 8 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    A,
    B,
    C,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    T0,
    T1,
    T2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SeriesKey {
    pub family: u8,
    pub metric: u16,
    pub instance: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SeriesId {
    slot: usize,
    generation: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SeriesBufRow {
    pub at_ms: u64,
    pub mean: f32,
    pub max: f32,
}

#[derive(Debug, Default)]
pub struct SeriesBuf {
    pub rows: Vec<SeriesBufRow>,
}

#[derive(Debug, Clone, Copy, Default)]
struct Acc {
    bucket: u64,
    next_bucket: Option<u64>,
    sum: f64,
    n: u32,
    max: f32,
    active: bool,
}

#[derive(Debug)]
struct Series {
    key: SeriesKey,
    class: Class,
    last_push_ms: u64,
    first_push_ms: u64,
    revision: u64,
    open_second: Option<u64>,
    open_value: f32,
    t0: Box<[f32]>,
    t1_mean: Option<Box<[f32]>>,
    t1_max: Option<Box<[f32]>>,
    t2_mean: Option<Box<[f32]>>,
    t2_max: Option<Box<[f32]>>,
    t1_acc: Acc,
    t2_acc: Acc,
}

impl Series {
    fn new(key: SeriesKey, class: Class) -> Self {
        let (t1, t2) = if class == Class::A {
            (true, true)
        } else {
            (class == Class::B, false)
        };
        Self {
            key,
            class,
            last_push_ms: 0,
            first_push_ms: 0,
            revision: 0,
            open_second: None,
            open_value: f32::NAN,
            t0: vec![f32::NAN; if class == Class::C { C_LEN } else { T0_LEN }].into_boxed_slice(),
            t1_mean: t1.then(|| vec![f32::NAN; T1_LEN].into_boxed_slice()),
            t1_max: t1.then(|| vec![f32::NAN; T1_LEN].into_boxed_slice()),
            t2_mean: t2.then(|| vec![f32::NAN; T2_LEN].into_boxed_slice()),
            t2_max: t2.then(|| vec![f32::NAN; T2_LEN].into_boxed_slice()),
            t1_acc: Acc::default(),
            t2_acc: Acc::default(),
        }
    }
    const fn bytes(class: Class) -> usize {
        size_of::<Self>()
            + match class {
                Class::A => T0_LEN * 4 + (T1_LEN + T2_LEN) * 8,
                Class::B => T0_LEN * 4 + T1_LEN * 8,
                Class::C => C_LEN * 4,
            }
    }
    fn push(&mut self, at_ms: u64, value: f32) -> bool {
        let second = at_ms / T0_MS;
        if self.open_second.is_some_and(|s| second < s) {
            return false;
        }
        if self.open_second.is_none() {
            self.first_push_ms = at_ms;
        }
        if let Some(open) = self.open_second
            && second > open
        {
            self.close_second(open, self.open_value);
            let fill_start = (open + 1).max(second.saturating_sub(self.t0.len() as u64));
            for skipped in fill_start..second {
                let index = skipped as usize % self.t0.len();
                self.t0[index] = f32::NAN;
            }
        }
        self.open_second = Some(second);
        self.open_value = if value.is_finite() { value } else { f32::NAN };
        self.t0[second as usize % self.t0.len()] = self.open_value;
        self.last_push_ms = at_ms;
        self.revision = self.revision.wrapping_add(1);
        true
    }
    fn close_second(&mut self, second: u64, value: f32) {
        let Some(mean) = &mut self.t1_mean else {
            return;
        };
        update_acc(
            &mut self.t1_acc,
            second / 5,
            value,
            mean,
            self.t1_max.as_mut().expect("paired T1 rings"),
            second % 5 == 4,
        );
        if let Some(mean2) = &mut self.t2_mean {
            update_acc(
                &mut self.t2_acc,
                second / 60,
                value,
                mean2,
                self.t2_max.as_mut().expect("paired T2 rings"),
                second % 60 == 59,
            );
        }
    }

    fn copy_recent_t0(source: &[f32], source_len: usize, dest: &mut [f32], end_second: u64) {
        let count = source_len.min(dest.len());
        let start = end_second.saturating_sub(count as u64 - 1);
        for second in start..=end_second {
            let destination_index = second as usize % dest.len();
            let source_index = second as usize % source_len;
            dest[destination_index] = source[source_index];
        }
    }
}

fn update_acc(
    acc: &mut Acc,
    bucket: u64,
    value: f32,
    means: &mut [f32],
    maxima: &mut [f32],
    finish: bool,
) {
    if acc.active && acc.bucket != bucket {
        commit_acc(*acc, means, maxima);
        acc.active = false;
    }
    if let Some(next_bucket) = acc.next_bucket
        && bucket > next_bucket
    {
        let missing = bucket.saturating_sub(next_bucket).min(means.len() as u64);
        for offset in 0..missing {
            let idx = (next_bucket + offset) as usize % means.len();
            means[idx] = f32::NAN;
            maxima[idx] = f32::NAN;
        }
    }
    if !acc.active {
        acc.bucket = bucket;
        acc.sum = 0.0;
        acc.n = 0;
        acc.max = 0.0;
        acc.active = true;
    }
    if value.is_finite() {
        acc.sum += f64::from(value);
        acc.n += 1;
        acc.max = if acc.n == 1 {
            value
        } else {
            acc.max.max(value)
        };
    }
    if finish {
        commit_acc(*acc, means, maxima);
        acc.active = false;
    }
    acc.next_bucket = Some(bucket.saturating_add(1));
}

fn commit_acc(acc: Acc, means: &mut [f32], maxima: &mut [f32]) {
    let idx = acc.bucket as usize % means.len();
    if acc.n == 0 {
        means[idx] = f32::NAN;
        maxima[idx] = f32::NAN;
    } else {
        means[idx] = (acc.sum / f64::from(acc.n)) as f32;
        maxima[idx] = acc.max;
    }
}

pub const fn horizon_secs(class: Class) -> u32 {
    match class {
        Class::A => 86_400,
        Class::B => 3_600,
        Class::C => 120,
    }
}
pub const fn class_bytes(class: Class) -> usize {
    Series::bytes(class)
}
pub const WORST_CASE_BYTES: usize =
    CAP_A * class_bytes(Class::A) + CAP_B * class_bytes(Class::B) + CAP_C * class_bytes(Class::C);
const _: () = assert!(WORST_CASE_BYTES <= HISTORY_BUDGET);

pub const fn resolve_window(class: Class, window: Window) -> u32 {
    match window.secs() {
        Some(s) => {
            if s < horizon_secs(class) {
                s
            } else {
                horizon_secs(class)
            }
        }
        None => horizon_secs(class),
    }
}
pub const fn tier_for(class: Class, window_secs: u32) -> Tier {
    match class {
        Class::A => {
            if window_secs <= 600 {
                Tier::T0
            } else if window_secs <= 3_600 {
                Tier::T1
            } else {
                Tier::T2
            }
        }
        Class::B => {
            if window_secs <= 600 {
                Tier::T0
            } else {
                Tier::T1
            }
        }
        Class::C => Tier::T0,
    }
}

#[derive(Debug, Default)]
pub struct SeriesStore {
    slots: Vec<Option<Series>>,
    generations: Vec<u32>,
}
impl SeriesStore {
    pub fn new() -> Self {
        Self::default()
    }
    fn cap(class: Class) -> usize {
        match class {
            Class::A => CAP_A,
            Class::B => CAP_B,
            Class::C => CAP_C,
        }
    }
    fn count(&self, class: Class) -> usize {
        self.slots
            .iter()
            .flatten()
            .filter(|s| s.class == class)
            .count()
    }
    fn find(&self, key: SeriesKey) -> Option<usize> {
        self.slots
            .iter()
            .position(|s| s.as_ref().is_some_and(|r| r.key == key))
    }
    fn id(&self, slot: usize) -> SeriesId {
        SeriesId {
            slot,
            generation: self.generations[slot],
        }
    }
    pub fn intern(&mut self, key: SeriesKey, class: Class) -> Option<SeriesId> {
        if let Some(slot) = self.find(key) {
            return Some(self.id(slot));
        }
        if self.count(class) >= Self::cap(class) && class != Class::C {
            return None;
        }
        if self.count(class) >= Self::cap(class) && class == Class::C {
            let slot = self
                .slots
                .iter()
                .enumerate()
                .filter_map(|(i, s)| {
                    s.as_ref()
                        .filter(|x| x.class == Class::C)
                        .map(|x| (i, x.last_push_ms))
                })
                .min_by_key(|x| x.1)?
                .0;
            self.slots[slot] = None;
            self.generations[slot] = self.generations[slot].wrapping_add(1);
        }
        let slot = self
            .slots
            .iter()
            .position(Option::is_none)
            .unwrap_or_else(|| {
                self.slots.push(None);
                self.generations.push(0);
                self.slots.len() - 1
            });
        self.slots[slot] = Some(Series::new(key, class));
        Some(self.id(slot))
    }
    pub fn id_of(&self, key: SeriesKey) -> Option<SeriesId> {
        self.find(key).map(|slot| self.id(slot))
    }
    fn get(&self, id: SeriesId) -> Option<&Series> {
        self.generations
            .get(id.slot)
            .filter(|g| **g == id.generation)
            .and_then(|_| self.slots.get(id.slot)?.as_ref())
    }
    fn get_mut(&mut self, id: SeriesId) -> Option<&mut Series> {
        if self.generations.get(id.slot).copied() != Some(id.generation) {
            return None;
        }
        self.slots.get_mut(id.slot)?.as_mut()
    }
    pub fn push(&mut self, id: SeriesId, at_ms: u64, value: f32) -> bool {
        self.get_mut(id).is_some_and(|s| s.push(at_ms, value))
    }
    pub fn evict_stale(&mut self, now_ms: u64) {
        for slot in 0..self.slots.len() {
            if self.slots[slot].as_ref().is_some_and(|s| {
                s.class == Class::C && now_ms.saturating_sub(s.last_push_ms) > 120_000
            }) {
                self.slots[slot] = None;
                self.generations[slot] = self.generations[slot].wrapping_add(1);
            }
        }
    }
    pub fn select(&mut self, key: SeriesKey, selected: bool) -> Option<SeriesId> {
        let slot = self.find(key)?;
        let s = self.slots[slot].as_ref()?;
        if selected && s.class == Class::C && self.count(Class::B) >= CAP_B {
            return None;
        }
        if selected && s.class == Class::C {
            let mut promoted = Series::new(key, Class::B);
            promoted.last_push_ms = s.last_push_ms;
            promoted.first_push_ms = s.first_push_ms;
            promoted.revision = s.revision;
            promoted.open_second = s.open_second;
            promoted.open_value = s.open_value;
            if let Some(end_second) = s.open_second {
                Series::copy_recent_t0(&s.t0, C_LEN, &mut promoted.t0, end_second);
                let start = end_second.saturating_sub((C_LEN - 1) as u64);
                for second in start..end_second {
                    promoted.close_second(second, promoted.t0[second as usize % T0_LEN]);
                }
            }
            self.slots[slot] = Some(promoted);
        } else if !selected && s.class == Class::B {
            let mut demoted = Series::new(key, Class::C);
            demoted.last_push_ms = s.last_push_ms;
            demoted.first_push_ms = s.first_push_ms;
            demoted.revision = s.revision;
            demoted.open_second = s.open_second;
            demoted.open_value = s.open_value;
            if let Some(end_second) = s.open_second {
                Series::copy_recent_t0(&s.t0, T0_LEN, &mut demoted.t0, end_second);
            }
            let evicted = if self.count(Class::C) >= CAP_C {
                self.slots
                    .iter()
                    .enumerate()
                    .filter(|(candidate, _)| *candidate != slot)
                    .filter_map(|(candidate, entry)| {
                        entry
                            .as_ref()
                            .filter(|series| series.class == Class::C)
                            .map(|series| (candidate, series.last_push_ms))
                    })
                    .min_by_key(|entry| entry.1)
                    .map(|entry| entry.0)
            } else {
                None
            };
            if let Some(evicted) = evicted {
                self.slots[evicted] = None;
                self.generations[evicted] = self.generations[evicted].wrapping_add(1);
            }
            self.slots[slot] = Some(demoted);
        }
        Some(self.id(slot))
    }
    pub fn read(
        &self,
        id: SeriesId,
        tier: Tier,
        from_ms: u64,
        to_ms: u64,
        out: &mut SeriesBuf,
    ) -> bool {
        let Some(s) = self.get(id) else {
            return false;
        };
        out.rows.clear();
        let (mean, max, period, len): (&[f32], &[f32], u64, usize) = match tier {
            Tier::T0 => (&s.t0, &s.t0, T0_MS, s.t0.len()),
            Tier::T1 => {
                let Some(m) = &s.t1_mean else {
                    return false;
                };
                (
                    m,
                    s.t1_max.as_deref().expect("paired T1 rings"),
                    T1_MS,
                    m.len(),
                )
            }
            Tier::T2 => {
                let Some(m) = &s.t2_mean else {
                    return false;
                };
                (
                    m,
                    s.t2_max.as_deref().expect("paired T2 rings"),
                    T2_MS,
                    m.len(),
                )
            }
        };
        if to_ms < from_ms || s.open_second.is_none() {
            return true;
        }
        let first = (from_ms / period).max(s.first_push_ms / period);
        let last_committed = match tier {
            Tier::T0 => s.open_second,
            Tier::T1 => {
                if s.t1_acc.active {
                    s.t1_acc.bucket.checked_sub(1)
                } else {
                    s.t1_acc.next_bucket.and_then(|next| next.checked_sub(1))
                }
            }
            Tier::T2 => {
                if s.t2_acc.active {
                    s.t2_acc.bucket.checked_sub(1)
                } else {
                    s.t2_acc.next_bucket.and_then(|next| next.checked_sub(1))
                }
            }
        };
        let Some(last_committed) = last_committed else {
            return true;
        };
        let last = (to_ms / period).min(last_committed);
        if last < first {
            return true;
        }
        let start = last.saturating_sub(len as u64 - 1).max(first);
        for bucket in start..=last {
            let idx = bucket as usize % len;
            let value = mean[idx];
            out.rows.push(SeriesBufRow {
                at_ms: bucket * period,
                mean: value,
                max: if tier == Tier::T0 { value } else { max[idx] },
            });
        }
        true
    }
    pub fn covered_secs(&self, id: SeriesId, now_ms: u64) -> Option<u32> {
        self.get(id).map(|s| {
            if s.open_second.is_none() {
                0
            } else {
                (now_ms.saturating_sub(s.first_push_ms) / 1_000 + 1)
                    .min(u64::from(horizon_secs(s.class))) as u32
            }
        })
    }
    pub fn revision(&self, id: SeriesId) -> Option<u64> {
        self.get(id).map(|s| s.revision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(n: u16) -> SeriesKey {
        SeriesKey {
            family: 1,
            metric: n,
            instance: 0,
        }
    }
    #[test]
    fn basic_push_gaps_and_stale_order() {
        let mut store = SeriesStore::new();
        let id = store.intern(key(1), Class::A).unwrap();
        assert!(store.push(id, 0, 1.0));
        assert!(store.push(id, 17_000, 3.0));
        let mut out = SeriesBuf::default();
        assert!(store.read(id, Tier::T0, 0, 17_000, &mut out));
        assert!(out.rows[1..17].iter().all(|r| r.mean.is_nan()));
        assert!(store.push(id, 18_000, 4.0));
        assert!(store.read(id, Tier::T1, 0, 18_000, &mut out));
        assert!(
            out.rows[1..3]
                .iter()
                .all(|r| r.mean.is_nan() && r.max.is_nan())
        );
        let rev = store.revision(id).unwrap();
        assert!(!store.push(id, 16_000, 4.0));
        assert_eq!(store.revision(id), Some(rev));
    }
    #[test]
    fn caps_lru_and_stale_ids() {
        let mut store = SeriesStore::new();
        for i in 0..CAP_A {
            assert!(store.intern(key(i as u16), Class::A).is_some());
        }
        assert!(store.intern(key(900), Class::A).is_none());
        let mut c = SeriesStore::new();
        let first = c.intern(key(0), Class::C).unwrap();
        let mut middle = None;
        for i in 1..CAP_C {
            let id = c.intern(key(i as u16), Class::C).unwrap();
            if i == 300 {
                middle = Some(id);
            } else {
                assert!(c.push(id, 1_000, i as f32));
            }
        }
        assert!(c.push(first, 2_000, 1.0));
        let middle = middle.unwrap();
        let new = c.intern(key(900), Class::C).unwrap();
        assert!(!c.push(middle, 3_000, 4.0));
        assert!(c.push(first, 3_000, 4.0));
        // The live series still rejects out-of-order pushes.
        assert!(!c.push(first, 1_000, 4.0));
        let mut out = SeriesBuf::default();
        assert!(!c.read(middle, Tier::T0, 0, 1_000, &mut out));
        assert!(c.read(first, Tier::T0, 0, 3_000, &mut out));
        assert!(c.push(new, 1_000, 4.0));
        c.evict_stale(122_001);
        assert!(!c.push(new, 123_000, 1.0));
    }
    #[test]
    fn tiers_windows_and_promote_demote() {
        assert_eq!(tier_for(Class::A, 600), Tier::T0);
        assert_eq!(tier_for(Class::A, 601), Tier::T1);
        assert_eq!(tier_for(Class::A, 3_601), Tier::T2);
        assert_eq!(tier_for(Class::B, 86_400), Tier::T1);
        assert_eq!(resolve_window(Class::A, Window::EVERYTHING), 86_400);
        assert_eq!(resolve_window(Class::C, Window::from_secs(300)), 120);
        for rung in crate::series_window::WindowLadder::DEFAULT_KEYS {
            assert!(Window::parse(rung).is_some());
        }
        assert_eq!(
            Window::parse(crate::series_window::WindowLadder::DEFAULT_KEYS[8])
                .and_then(Window::secs),
            Some(86_400)
        );
        let mut store = SeriesStore::new();
        let id = store.intern(key(1), Class::C).unwrap();
        for second in 0..5 {
            assert!(store.push(id, second * 1_000, second as f32));
        }
        let id = store.select(key(1), true).unwrap();
        let mut out = SeriesBuf::default();
        for second in 5..=10 {
            assert!(store.push(id, second * 1_000, second as f32));
        }
        assert!(store.read(id, Tier::T1, 0, 10_000, &mut out));
        assert_eq!(
            out.rows.iter().find(|r| r.at_ms == 5_000).unwrap().mean,
            7.0
        );
        store.select(key(1), false).unwrap();
        assert_eq!(store.get(id).unwrap().t0.len(), C_LEN);
        assert!(store.read(id, Tier::T0, 0, 10_000, &mut out));
        assert_eq!(out.rows.len(), 11);
        for (second, row) in out.rows.iter().enumerate() {
            assert_eq!(row.mean, second as f32);
        }

        let mut full_b = SeriesStore::new();
        for metric in 0..CAP_B {
            assert!(full_b.intern(key(metric as u16), Class::B).is_some());
        }
        let low = full_b.intern(key(1_000), Class::C).unwrap();
        assert!(full_b.select(key(1_000), true).is_none());
        assert!(full_b.push(low, 1_000, 5.0));
    }

    #[test]
    fn thirty_minute_gap_preserves_t2_history_and_fills_missing_minutes() {
        let mut store = SeriesStore::new();
        let id = store.intern(key(1), Class::A).unwrap();
        for second in 0..120_u64 {
            assert!(store.push(id, second * 1_000, second as f32));
        }
        assert!(store.push(id, 30 * 60_000, 9_000.0));
        assert!(store.push(id, 30 * 60_000 + 1_000, 9_001.0));
        let mut out = SeriesBuf::default();
        assert!(store.read(id, Tier::T2, 0, 30 * 60_000, &mut out));
        assert_eq!(out.rows[0].mean, 29.5);
        assert_eq!(out.rows[1].mean, 89.5);
        assert!(out.rows[2..30].iter().all(|row| row.mean.is_nan()));
    }

    #[test]
    fn t1_reads_stop_before_the_open_bucket_after_wrap() {
        let mut store = SeriesStore::new();
        let id = store.intern(key(2), Class::B).unwrap();
        for second in 0..3_700_u64 {
            assert!(store.push(id, second * 1_000, second as f32));
        }
        let mut out = SeriesBuf::default();
        assert!(store.read(id, Tier::T1, 0, 3_700_000, &mut out));
        assert_eq!(out.rows.len(), T1_LEN);
        assert_eq!(out.rows.last().unwrap().at_ms, 3_690_000);
        assert!(out.rows.iter().all(|row| row.at_ms < 3_700_000));
    }

    #[test]
    fn coverage_ids_missing_tiers_and_empty_ranges() {
        let mut store = SeriesStore::new();
        let id = store.intern(key(3), Class::C).unwrap();
        assert_eq!(store.covered_secs(id, 1_000_000), Some(0));
        assert_eq!(store.id_of(key(3)), Some(id));
        assert_eq!(store.id_of(key(4)), None);
        assert_eq!(store.select(key(4), true), None);
        let mut out = SeriesBuf::default();
        assert!(!store.read(id, Tier::T1, 0, 10_000, &mut out));
        assert!(!store.read(id, Tier::T2, 0, 10_000, &mut out));
        assert!(store.push(id, 10_000, 1.0));
        assert_eq!(store.covered_secs(id, 15_000), Some(6));
        assert!(store.read(id, Tier::T0, 10_000, 9_000, &mut out));
        assert!(out.rows.is_empty());
    }

    #[test]
    fn demoting_at_c_cap_evicts_one_c_series_and_keeps_recent_values() {
        let mut store = SeriesStore::new();
        for metric in 0..CAP_C {
            let id = store.intern(key(metric as u16), Class::C).unwrap();
            assert!(store.push(id, 1_000, metric as f32));
        }
        let evicted_id = store.id_of(key(0)).unwrap();
        let chosen = store.intern(key(1_000), Class::B).unwrap();
        for second in 0..130_u64 {
            assert!(store.push(chosen, second * 1_000, second as f32));
        }
        let demoted = store.select(key(1_000), false).unwrap();
        assert!(!store.push(evicted_id, 2_000, 2.0));
        let mut out = SeriesBuf::default();
        assert!(store.read(demoted, Tier::T0, 0, 129_000, &mut out));
        assert_eq!(out.rows.len(), C_LEN);
        assert_eq!(out.rows.first().unwrap().mean, 10.0);
        assert_eq!(out.rows.last().unwrap().mean, 129.0);
    }

    #[test]
    fn seeded_tier_means_and_maxima_match_latest_per_second_model() {
        let mut store = SeriesStore::new();
        let id = store.intern(key(7), Class::A).unwrap();
        let mut model = vec![f32::NAN; 3_882];
        let mut random = 0x9e37_79b9_u32;
        let mut gap = 0_u64;
        for second in 0..3_700_u64 {
            if second == 1_800 {
                gap = 180;
            }
            random ^= random << 13;
            random ^= random >> 17;
            random ^= random << 5;
            let actual_second = second + gap;
            if second == 0 || !random.is_multiple_of(7) {
                let pushes = 1 + (random % 3) as usize;
                for push_index in 0..pushes {
                    random ^= random << 13;
                    random ^= random >> 17;
                    random ^= random << 5;
                    let value = if random.is_multiple_of(11) {
                        f32::NAN
                    } else {
                        (random % 10_000) as f32 / 37.0
                    };
                    let at_ms = actual_second * 1_000 + push_index as u64 * 200;
                    assert!(store.push(id, at_ms, value));
                    model[actual_second as usize] =
                        if value.is_finite() { value } else { f32::NAN };
                }
            }
        }
        assert!(store.push(id, 3_880_000, 1.0));
        assert!(store.push(id, 3_881_000, 2.0));
        let mut out = SeriesBuf::default();
        assert!(store.read(id, Tier::T1, 0, 3_880_000, &mut out));
        assert_eq!(out.rows.len(), T1_LEN);
        for row in &out.rows {
            let bucket = (row.at_ms / T1_MS) as usize;
            let values = &model[bucket * 5..bucket * 5 + 5];
            let finite: Vec<f32> = values.iter().copied().filter(|v| v.is_finite()).collect();
            if finite.is_empty() {
                assert!(row.mean.is_nan() && row.max.is_nan());
            } else {
                let expected_mean =
                    finite.iter().map(|v| f64::from(*v)).sum::<f64>() / finite.len() as f64;
                let expected_max = finite.iter().copied().fold(f32::NEG_INFINITY, f32::max);
                assert!(
                    (f64::from(row.mean) - expected_mean).abs()
                        <= expected_mean.abs().max(1.0) * 1e-4
                );
                assert_eq!(row.max, expected_max);
            }
        }
        assert!(store.read(id, Tier::T2, 0, 3_880_000, &mut out));
        for row in &out.rows {
            let bucket = (row.at_ms / T2_MS) as usize;
            let values = &model[bucket * 60..bucket * 60 + 60];
            let finite: Vec<f32> = values.iter().copied().filter(|v| v.is_finite()).collect();
            if finite.is_empty() {
                assert!(row.mean.is_nan() && row.max.is_nan());
            } else {
                let expected_mean =
                    finite.iter().map(|v| f64::from(*v)).sum::<f64>() / finite.len() as f64;
                let expected_max = finite.iter().copied().fold(f32::NEG_INFINITY, f32::max);
                assert!(
                    (f64::from(row.mean) - expected_mean).abs()
                        <= expected_mean.abs().max(1.0) * 1e-4
                );
                assert_eq!(row.max, expected_max);
            }
        }
    }

    #[test]
    fn all_resolves_to_the_widest_default_rung() {
        let ladder = crate::series_window::WindowLadder::default_ladder();
        assert_eq!(
            resolve_window(Class::A, Window::EVERYTHING),
            ladder.widest_bounded_secs()
        );
        assert_eq!(ladder.widest_bounded_secs(), 86_400);
        assert_eq!(
            ladder.windows().last().and_then(|window| window.secs()),
            Some(86_400)
        );
    }
}
