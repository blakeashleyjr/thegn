//! Heap-free cpufreq policy and power controls.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SysToken {
    bytes: [u8; 47],
    len: u8,
}
impl SysToken {
    pub fn new(bytes: &[u8]) -> Option<Self> {
        if bytes.is_empty() || bytes.len() > 47 || !bytes.is_ascii() {
            return None;
        }
        let mut token = Self {
            bytes: [0; 47],
            len: bytes.len() as u8,
        };
        token.bytes[..bytes.len()].copy_from_slice(bytes);
        Some(token)
    }
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or("")
    }
}
impl Default for SysToken {
    fn default() -> Self {
        Self {
            bytes: [0; 47],
            len: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenList {
    tokens: [SysToken; 8],
    len: u8,
}
impl TokenList {
    pub fn push(&mut self, token: SysToken) -> bool {
        if usize::from(self.len) == self.tokens.len() {
            return false;
        }
        self.tokens[usize::from(self.len)] = token;
        self.len += 1;
        true
    }
    pub fn len(&self) -> usize {
        usize::from(self.len)
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn get(&self, index: usize) -> Option<SysToken> {
        self.tokens
            .get(index)
            .copied()
            .filter(|_| index < self.len())
    }
    pub fn iter(&self) -> impl Iterator<Item = SysToken> + '_ {
        self.tokens[..self.len()].iter().copied()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CpuSet {
    words: [u64; 8],
    pub truncated: bool,
}
impl CpuSet {
    pub fn insert(&mut self, cpu: u32) {
        if cpu >= 512 {
            self.truncated = true;
        } else {
            self.words[(cpu / 64) as usize] |= 1_u64 << (cpu % 64);
        }
    }
    pub const fn contains(&self, cpu: u32) -> bool {
        cpu < 512 && self.words[(cpu / 64) as usize] & (1_u64 << (cpu % 64)) != 0
    }
    pub fn first(&self) -> Option<u32> {
        self.iter().next()
    }
    pub fn len(&self) -> usize {
        self.words
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum()
    }
    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|word| *word == 0)
    }
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.words.iter().enumerate().flat_map(|(word_i, word)| {
            (0..64).filter_map(move |bit| {
                (word & (1_u64 << bit) != 0).then_some((word_i * 64 + bit) as u32)
            })
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreqSource {
    Avg,
    Scaling,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GovernorSummary {
    Common(SysToken),
    Mixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CpuPolicy {
    pub policy: u16,
    pub cpus: CpuSet,
    pub cur_khz: Option<u64>,
    pub cur_source: Option<FreqSource>,
    pub hw_min_khz: Option<u64>,
    pub hw_max_khz: Option<u64>,
    pub scaling_min_khz: Option<u64>,
    pub scaling_max_khz: Option<u64>,
    pub governor: Option<SysToken>,
    pub epp: Option<SysToken>,
    pub driver: Option<SysToken>,
    pub boost: Option<bool>,
    pub prefcore_rank: Option<u32>,
    pub highest_perf: Option<u32>,
}
impl CpuPolicy {
    pub fn capped(&self) -> bool {
        matches!((self.scaling_max_khz, self.hw_max_khz), (Some(s), Some(h)) if s < h)
            || matches!((self.scaling_min_khz, self.hw_min_khz), (Some(s), Some(h)) if s > h)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PowerPolicy {
    pub boost: Option<bool>,
    pub amd_pstate_status: Option<SysToken>,
    pub amd_prefcore: Option<bool>,
    pub platform_profile: Option<SysToken>,
    pub platform_profile_choices: TokenList,
    pub epp_choices: TokenList,
}

#[cfg(test)]
mod tests {
    use super::{CpuPolicy, CpuSet, SysToken, TokenList};
    #[test]
    fn token_and_cpu_set_are_bounded() {
        assert!(SysToken::new(b"balanced").is_some());
        assert!(SysToken::new(&[b'a'; 48]).is_none());
        assert!(SysToken::new("é".as_bytes()).is_none());
        let mut cpus = CpuSet::default();
        cpus.insert(511);
        cpus.insert(512);
        assert!(cpus.contains(511));
        assert!(cpus.truncated);
        assert_eq!(cpus.len(), 1);
        let mut tokens = TokenList::default();
        for _ in 0..8 {
            assert!(tokens.push(SysToken::new(b"x").unwrap()));
        }
        assert!(!tokens.push(SysToken::new(b"x").unwrap()));
    }
    #[test]
    fn capped_detects_only_limits_inside_hardware_range() {
        let policy = CpuPolicy {
            hw_min_khz: Some(1),
            hw_max_khz: Some(10),
            scaling_min_khz: Some(2),
            scaling_max_khz: Some(9),
            ..CpuPolicy::default()
        };
        assert!(policy.capped());
        let uncapped = CpuPolicy {
            hw_min_khz: Some(1),
            hw_max_khz: Some(10),
            scaling_min_khz: Some(1),
            scaling_max_khz: Some(10),
            ..CpuPolicy::default()
        };
        assert!(!uncapped.capped());
    }
}
