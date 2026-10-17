//! Bounded stable ids for metric instances.
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InstanceId(pub u16);
impl InstanceId {
    pub const OTHER: Self = Self(u16::MAX);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Family {
    Core,
    Iface,
    BlockDev,
    Sensor,
    Gpu,
    Container,
}
impl Family {
    pub const fn cap(self) -> usize {
        match self {
            Self::Core => 512,
            Self::Iface => 32,
            Self::BlockDev => 64,
            Self::Sensor => 128,
            Self::Gpu => 8,
            Self::Container => 64,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interned {
    Id(InstanceId),
    Other,
}

#[derive(Debug, Clone)]
struct Entry {
    id: InstanceId,
    live: bool,
    retired_at: u64,
}

#[derive(Debug)]
pub struct Interner {
    family: Family,
    entries: HashMap<String, Entry>,
    by_id: Vec<Option<String>>,
    clock: u64,
    truncated: bool,
}
impl Interner {
    pub fn new(family: Family) -> Self {
        Self {
            family,
            entries: HashMap::new(),
            by_id: Vec::new(),
            clock: 0,
            truncated: false,
        }
    }
    pub fn intern(&mut self, key: &str) -> Interned {
        if let Some(entry) = self.entries.get_mut(key) {
            entry.live = true;
            return Interned::Id(entry.id);
        }
        let live = self.live();
        let id = if live < self.family.cap() {
            let Ok(id) = u16::try_from(self.by_id.len()) else {
                self.truncated = true;
                return Interned::Other;
            };
            if id == InstanceId::OTHER.0 {
                self.truncated = true;
                return Interned::Other;
            }
            InstanceId(id)
        } else {
            let oldest = self
                .entries
                .iter()
                .filter(|(_, entry)| !entry.live)
                .min_by_key(|(_, entry)| entry.retired_at)
                .map(|(key, entry)| (key.clone(), entry.id));
            if let Some((old_key, id)) = oldest {
                self.entries.remove(&old_key);
                id
            } else {
                self.truncated = true;
                return Interned::Other;
            }
        };
        if usize::from(id.0) == self.by_id.len() {
            self.by_id.push(Some(key.to_owned()));
        } else if let Some(slot) = self.by_id.get_mut(usize::from(id.0)) {
            *slot = Some(key.to_owned());
        }
        self.entries.insert(
            key.to_owned(),
            Entry {
                id,
                live: true,
                retired_at: 0,
            },
        );
        Interned::Id(id)
    }
    pub fn retire(&mut self, id: InstanceId) {
        self.clock = self.clock.saturating_add(1);
        if let Some(entry) = self.entries.values_mut().find(|entry| entry.id == id) {
            entry.live = false;
            entry.retired_at = self.clock;
        }
    }
    pub fn truncated(&self) -> bool {
        self.truncated
    }
    pub fn live(&self) -> usize {
        self.entries.values().filter(|entry| entry.live).count()
    }
    pub fn key(&self, id: InstanceId) -> Option<&str> {
        self.by_id.get(usize::from(id.0)).and_then(Option::as_deref)
    }
}

#[cfg(test)]
mod tests {
    use super::{Family, InstanceId, Interned, Interner};
    #[test]
    fn ids_are_dense_stable_and_capacity_is_bounded() {
        let mut interner = Interner::new(Family::Gpu);
        let ids: Vec<_> = (0..8)
            .map(|n| interner.intern(&format!("gpu{n}")))
            .collect();
        assert_eq!(ids[0], Interned::Id(InstanceId(0)));
        assert_eq!(interner.intern("gpu0"), ids[0]);
        assert_eq!(interner.intern("overflow"), Interned::Other);
        assert!(interner.truncated());
    }
    #[test]
    fn retired_key_resumes_and_dormant_slot_is_reused_at_capacity() {
        let mut interner = Interner::new(Family::Gpu);
        let first = interner.intern("a");
        let Interned::Id(first_id) = first else {
            panic!()
        };
        interner.intern("b");
        interner.retire(first_id);
        assert_eq!(interner.intern("a"), first);
        interner.retire(first_id);
        for key in ["c", "d", "e", "f", "g", "h", "i"] {
            assert!(matches!(interner.intern(key), Interned::Id(_)));
        }
        assert_eq!(interner.live(), Family::Gpu.cap());
        assert_eq!(interner.intern("new"), Interned::Id(first_id));
        assert_eq!(interner.key(first_id), Some("new"));
        assert_eq!(interner.live(), Family::Gpu.cap());
    }

    #[test]
    fn cap_reuses_the_oldest_retired_id_first() {
        let mut interner = Interner::new(Family::Gpu);
        let Interned::Id(a) = interner.intern("a") else {
            panic!()
        };
        let Interned::Id(b) = interner.intern("b") else {
            panic!()
        };
        interner.retire(a);
        interner.retire(b);
        for key in ["c", "d", "e", "f", "g", "h", "i", "j"] {
            assert!(matches!(interner.intern(key), Interned::Id(_)));
        }
        assert_eq!(interner.live(), Family::Gpu.cap());
        assert_eq!(interner.intern("k"), Interned::Id(a));
        assert_eq!(interner.intern("l"), Interned::Id(b));
    }
}
