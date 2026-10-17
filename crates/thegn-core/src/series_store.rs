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
pub const LADDER_KEYS: [&str; 9] = ["30s", "1m", "5m", "10m", "30m", "1h", "6h", "12h", "24h"];

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
    fn clear(&mut self) {
        self.t0.fill(f32::NAN);
        if let Some(x) = &mut self.t1_mean {
            x.fill(f32::NAN);
        }
        if let Some(x) = &mut self.t1_max {
            x.fill(f32::NAN);
        }
        if let Some(x) = &mut self.t2_mean {
            x.fill(f32::NAN);
        }
        if let Some(x) = &mut self.t2_max {
            x.fill(f32::NAN);
        }
        self.t1_acc = Acc::default();
        self.t2_acc = Acc::default();
        self.open_second = None;
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
            let gap = second - open;
            if gap > 1_440 {
                self.clear();
            } else {
                self.close_second(open, self.open_value);
                let fill_start = (open + 1).max(second.saturating_sub(self.t0.len() as u64));
                for skipped in fill_start..second {
                    let index = skipped as usize % self.t0.len();
                    self.t0[index] = f32::NAN;
                }
                for skipped in (open + 1)..second {
                    self.close_second(skipped, f32::NAN);
                }
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
        let missing = bucket
            .saturating_sub(acc.bucket + 1)
            .min(means.len() as u64);
        for offset in 0..missing {
            let idx = (acc.bucket + 1 + offset) as usize % means.len();
            means[idx] = f32::NAN;
            maxima[idx] = f32::NAN;
        }
        *acc = Acc::default();
    }
    if !acc.active {
        acc.bucket = bucket;
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
        *acc = Acc::default();
    }
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
        if self.count(class) >= Self::cap(class) {
            return None;
        }
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
        let first = from_ms / period;
        let last = to_ms / period;
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
            ((now_ms.saturating_sub(s.first_push_ms) / 1_000 + 1)
                .min(u64::from(horizon_secs(s.class))) as u32)
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
        assert!(store.read(id, Tier::T1, 0, 17_000, &mut out));
        assert!(
            out.rows[1..3]
                .iter()
                .all(|r| r.mean.is_nan() && r.max.is_nan())
        );
        let rev = store.revision(id).unwrap();
        assert!(!store.push(id, 16_000, 4.0));
        assert_eq!(store.revision(id), Some(rev));
        assert_eq!(class_bytes(Class::A), 19_680 + size_of::<Series>());
        assert!(WORST_CASE_BYTES <= HISTORY_BUDGET);
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
        for i in 1..CAP_C {
            assert!(c.intern(key(i as u16), Class::C).is_some());
        }
        let new = c.intern(key(900), Class::C).unwrap();
        assert!(!c.push(first, 1_000, 4.0));
        let mut out = SeriesBuf::default();
        assert!(!c.read(first, Tier::T0, 0, 1_000, &mut out));
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
        for rung in LADDER_KEYS {
            assert!(Window::parse(rung).is_some());
        }
        assert_eq!(
            Window::parse(LADDER_KEYS[8]).and_then(Window::secs),
            Some(86_400)
        );
        let mut store = SeriesStore::new();
        let id = store.intern(key(1), Class::C).unwrap();
        for second in 0..7 {
            assert!(store.push(id, second * 1_000, second as f32));
        }
        let id = store.select(key(1), true).unwrap();
        let mut out = SeriesBuf::default();
        assert!(store.read(id, Tier::T1, 0, 10_000, &mut out));
        assert!(out.rows.iter().any(|r| r.mean.is_finite()));
        store.select(key(1), false).unwrap();
        assert_eq!(store.get(id).unwrap().t0.len(), C_LEN);

        let mut full_b = SeriesStore::new();
        for metric in 0..CAP_B {
            assert!(full_b.intern(key(metric as u16), Class::B).is_some());
        }
        let low = full_b.intern(key(1_000), Class::C).unwrap();
        assert!(full_b.select(key(1_000), true).is_none());
        assert!(full_b.push(low, 1_000, 5.0));
    }

    #[test]
    fn three_day_gap_is_bounded_and_resets_visible_history() {
        let mut store = SeriesStore::new();
        let id = store.intern(key(1), Class::A).unwrap();
        assert!(store.push(id, 0, 2.0));
        assert!(store.push(id, 3 * 86_400_000, 7.0));
        let mut out = SeriesBuf::default();
        assert!(store.read(
            id,
            Tier::T0,
            3 * 86_400_000 - 599_000,
            3 * 86_400_000,
            &mut out,
        ));
        assert_eq!(out.rows.len(), T0_LEN);
        assert!(out.rows[..T0_LEN - 1].iter().all(|row| row.mean.is_nan()));
        assert_eq!(out.rows[T0_LEN - 1].mean, 7.0);
    }

    #[test]
    fn seeded_tier_means_and_maxima_match_latest_per_second_model() {
        let mut store = SeriesStore::new();
        let id = store.intern(key(7), Class::A).unwrap();
        let mut model = vec![f32::NAN; 300];
        let mut random = 0x9e37_79b9_u32;
        for second in 0..300_u64 {
            random ^= random << 13;
            random ^= random >> 17;
            random ^= random << 5;
            let pushes = 1 + (random % 3) as usize;
            for push_index in 0..pushes {
                random ^= random << 13;
                random ^= random >> 17;
                random ^= random << 5;
                let value = if random % 11 == 0 {
                    f32::NAN
                } else {
                    (random % 10_000) as f32 / 37.0
                };
                let at_ms = second * 1_000 + push_index as u64 * 200;
                assert!(store.push(id, at_ms, value));
                model[second as usize] = if value.is_finite() { value } else { f32::NAN };
            }
        }
        assert!(store.push(id, 300_000, 1.0));
        let mut out = SeriesBuf::default();
        assert!(store.read(id, Tier::T1, 0, 300_000, &mut out));
        for (bucket, row) in out.rows.iter().take(60).enumerate() {
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
        assert!(store.read(id, Tier::T2, 0, 300_000, &mut out));
        for (bucket, row) in out.rows.iter().take(5).enumerate() {
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
}
