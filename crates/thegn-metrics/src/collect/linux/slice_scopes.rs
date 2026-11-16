//! Per-scope CPU and memory inside `thegn.slice`, for the CPU attribution
//! popup.
//!
//! thegn names the transient scope of its heaviest background jobs
//! (`thegn-<kind>-<slug>-<pid>-<seq>.scope`, see
//! `thegn_core::sandbox_cpucap::scope_unit_name`); everything else it wraps is
//! an anonymous `run-*.scope`. One pass is one `read_dir` of the slice plus two
//! small reads (`cpu.stat`, `memory.current`) per scope. It runs only inside the
//! popup-gated process pass, never on an idle tick.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::cgroup::cgroup2_root;
use super::cgroup_slice::discover_slice;
use crate::procs::{MAX_SCOPES, ScopeTotals, UNNAMED_SCOPES};

/// Bounds one pass's directory walk, whatever the slice holds.
const MAX_ENTRIES: usize = 512;

/// How long a failed slice lookup is trusted before it is retried (the slice
/// appears with the first scope that joins it).
const RETRY_AFTER: Duration = Duration::from_secs(30);

#[derive(Debug)]
enum Locate {
    /// Never read (tests that must not touch the host's cgroups).
    Disabled,
    /// Find the slice under this cgroup2 mount from `/proc/self/cgroup`.
    Discover {
        root: PathBuf,
        retry_at: Option<Instant>,
    },
    /// The slice directory, found once and cached.
    Found(PathBuf),
}

#[derive(Debug)]
pub(crate) struct SliceScopes {
    locate: Locate,
    /// `(unit, usage_usec)` from the previous pass, sorted by unit.
    previous: Vec<(String, u64)>,
    previous_at: Option<Instant>,
}

impl SliceScopes {
    /// Reads the live host: cgroup2 at `/sys/fs/cgroup`.
    pub(crate) fn system() -> Self {
        Self::with(Locate::Discover {
            root: PathBuf::from("/sys/fs/cgroup"),
            retry_at: None,
        })
    }

    /// Reads nothing; every pass yields an empty list.
    pub(crate) fn disabled() -> Self {
        Self::with(Locate::Disabled)
    }

    /// Reads the slice at a fixed path (fixtures).
    #[cfg(test)]
    pub(crate) fn at(slice: PathBuf) -> Self {
        Self::with(Locate::Found(slice))
    }

    fn with(locate: Locate) -> Self {
        Self {
            locate,
            previous: Vec::new(),
            previous_at: None,
        }
    }

    /// Forget CPU baselines (the popup closed; the next pass re-primes).
    pub(crate) fn reset(&mut self) {
        self.previous.clear();
        self.previous_at = None;
    }

    fn slice_path(&mut self, now: Instant) -> Option<PathBuf> {
        let root = match &self.locate {
            Locate::Disabled => return None,
            Locate::Found(path) => return Some(path.clone()),
            Locate::Discover { retry_at, .. } if retry_at.is_some_and(|at| now < at) => {
                return None;
            }
            Locate::Discover { root, .. } => root.clone(),
        };
        let found = cgroup2_root(&root).and_then(|root| {
            let membership = std::fs::read("/proc/self/cgroup").ok()?;
            discover_slice(&root, &membership).filter(|path| path.is_dir())
        });
        self.locate = match &found {
            Some(path) => Locate::Found(path.clone()),
            None => Locate::Discover {
                root,
                retry_at: now.checked_add(RETRY_AFTER),
            },
        };
        found
    }

    /// One pass. CPU is the usage delta since the previous pass over the wall
    /// time between them, so the first pass reports `cpu_pct: None`.
    pub(crate) fn sample(&mut self, now: Instant) -> Vec<ScopeTotals> {
        let Some(slice) = self.slice_path(now) else {
            self.reset();
            return Vec::new();
        };
        let wall = self
            .previous_at
            .map(|at| now.saturating_duration_since(at))
            .filter(|wall| !wall.is_zero());
        let readings = read_scopes(&slice);
        let mut named: Vec<ScopeTotals> = Vec::new();
        let mut unnamed = ScopeTotals {
            unit: UNNAMED_SCOPES.to_string(),
            cpu_pct: None,
            rss: None,
        };
        let mut any_unnamed = false;
        for reading in &readings {
            let cpu = wall.and_then(|wall| {
                let index = self
                    .previous
                    .binary_search_by(|(unit, _)| unit.as_str().cmp(reading.unit.as_str()))
                    .ok()?;
                let before = self.previous[index].1;
                let usage = reading.usage_usec?;
                let delta = usage.checked_sub(before)?;
                Some((delta as f64 / wall.as_micros().max(1) as f64 * 100.0) as f32)
            });
            if reading.named && named.len() < MAX_SCOPES - 1 {
                named.push(ScopeTotals {
                    unit: reading.unit.clone(),
                    cpu_pct: cpu,
                    rss: reading.rss,
                });
            } else {
                any_unnamed = true;
                if let Some(cpu) = cpu {
                    unnamed.cpu_pct = Some(unnamed.cpu_pct.unwrap_or(0.0) + cpu);
                }
                if let Some(rss) = reading.rss {
                    unnamed.rss = Some(unnamed.rss.unwrap_or(0).saturating_add(rss));
                }
            }
        }
        self.previous.clear();
        self.previous.extend(
            readings
                .into_iter()
                .filter_map(|reading| Some((reading.unit, reading.usage_usec?))),
        );
        self.previous.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        self.previous_at = Some(now);
        named.sort_unstable_by(|a, b| a.unit.cmp(&b.unit));
        if any_unnamed {
            named.push(unnamed);
        }
        named
    }
}

struct Reading {
    unit: String,
    named: bool,
    usage_usec: Option<u64>,
    rss: Option<u64>,
}

/// One `read_dir` of the slice: every `thegn-*.scope` and `run-*.scope`,
/// sorted so named scopes come first (and in a stable order), then anonymous.
fn read_scopes(slice: &Path) -> Vec<Reading> {
    let Ok(dir) = std::fs::read_dir(slice) else {
        return Vec::new();
    };
    let mut out: Vec<Reading> = Vec::new();
    for entry in dir.flatten().take(MAX_ENTRIES) {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(unit) = name.strip_suffix(".scope") else {
            continue;
        };
        let named = unit.starts_with("thegn-");
        if !named && !unit.starts_with("run-") {
            continue;
        }
        let path = entry.path();
        out.push(Reading {
            unit: unit.to_string(),
            named,
            usage_usec: std::fs::read_to_string(path.join("cpu.stat"))
                .ok()
                .and_then(|text| usage_usec(&text)),
            rss: std::fs::read_to_string(path.join("memory.current"))
                .ok()
                .and_then(|text| text.trim().parse().ok()),
        });
    }
    out.sort_unstable_by(|a, b| b.named.cmp(&a.named).then_with(|| a.unit.cmp(&b.unit)));
    out
}

/// `usage_usec` from a cgroup v2 `cpu.stat`.
fn usage_usec(cpu_stat: &str) -> Option<u64> {
    cpu_stat.lines().find_map(|line| {
        let (key, value) = line.split_once(' ')?;
        if key == "usage_usec" {
            value.trim().parse().ok()
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{MAX_SCOPES, SliceScopes, UNNAMED_SCOPES, usage_usec};
    use std::path::Path;
    use std::time::{Duration, Instant};

    fn scope(slice: &Path, name: &str, usage: u64, mem: u64) {
        let dir = slice.join(name);
        std::fs::create_dir_all(&dir).expect("scope dir");
        std::fs::write(
            dir.join("cpu.stat"),
            format!("usage_usec {usage}\nuser_usec 1\nsystem_usec 2\n"),
        )
        .expect("cpu.stat");
        std::fs::write(dir.join("memory.current"), format!("{mem}\n")).expect("memory.current");
    }

    fn close(a: Option<f32>, b: f32) -> bool {
        a.is_some_and(|a| (a - b).abs() < 0.01)
    }

    #[test]
    fn parses_usage_usec() {
        assert_eq!(usage_usec("usage_usec 42\nuser_usec 1\n"), Some(42));
        assert_eq!(usage_usec("user_usec 1\nusage_usec 7"), Some(7));
        assert_eq!(usage_usec("user_usec 1\n"), None);
        assert_eq!(usage_usec("usage_usec x\n"), None);
    }

    #[test]
    fn fixture_slice_gives_named_scopes_an_unnamed_sum_and_cpu_deltas() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let slice = tmp.path().join("thegn.slice");
        scope(&slice, "thegn-gate-main-10-0.scope", 1_000_000, 100);
        scope(&slice, "thegn-agent-feat-10-1.scope", 0, 200);
        scope(&slice, "run-u1.scope", 0, 10);
        scope(&slice, "run-u2.scope", 500, 20);
        // Not a scope, and not one of ours: ignored.
        std::fs::create_dir_all(slice.join("nested.slice")).expect("slice");
        scope(&slice, "app-other.scope", 0, 999);
        std::fs::write(slice.join("cpu.stat"), "usage_usec 1\n").expect("file");

        let mut reader = SliceScopes::at(slice.clone());
        let t0 = Instant::now();
        let first = reader.sample(t0);
        let units: Vec<&str> = first.iter().map(|s| s.unit.as_str()).collect();
        assert_eq!(
            units,
            [
                "thegn-agent-feat-10-1",
                "thegn-gate-main-10-0",
                UNNAMED_SCOPES
            ]
        );
        assert!(
            first.iter().all(|s| s.cpu_pct.is_none()),
            "first pass primes"
        );
        assert_eq!(first[2].rss, Some(30));
        assert_eq!(first[0].rss, Some(200));

        // One second later: gate burned 2 s of CPU (200%), agent 0.5 s, the
        // anonymous scopes 0.25 s between them, and one anonymous scope exited
        // while a new one appeared (no baseline, so it adds no CPU).
        scope(&slice, "thegn-gate-main-10-0.scope", 3_000_000, 100);
        scope(&slice, "thegn-agent-feat-10-1.scope", 500_000, 200);
        scope(&slice, "run-u1.scope", 250_000, 10);
        std::fs::remove_dir_all(slice.join("run-u2.scope")).expect("rm");
        scope(&slice, "run-u3.scope", 9_000_000, 5);
        let second = reader.sample(t0 + Duration::from_secs(1));
        assert!(close(second[1].cpu_pct, 200.0), "{second:?}");
        assert!(close(second[0].cpu_pct, 50.0), "{second:?}");
        assert_eq!(second[2].unit, UNNAMED_SCOPES);
        assert!(close(second[2].cpu_pct, 25.0), "{second:?}");
        assert_eq!(second[2].rss, Some(15));

        // A reset drops the baseline.
        reader.reset();
        assert!(
            reader
                .sample(t0 + Duration::from_secs(2))
                .iter()
                .all(|s| s.cpu_pct.is_none())
        );
    }

    #[test]
    fn named_scopes_are_capped_and_overflow_into_the_unnamed_sum() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let slice = tmp.path().join("thegn.slice");
        for i in 0..(MAX_SCOPES + 5) {
            scope(&slice, &format!("thegn-gate-w-1-{i}.scope"), 0, 1);
        }
        let out = SliceScopes::at(slice).sample(Instant::now());
        assert_eq!(out.len(), MAX_SCOPES);
        let last = out.last().expect("unnamed");
        assert_eq!(last.unit, UNNAMED_SCOPES);
        assert_eq!(last.rss, Some(6));
    }

    #[test]
    fn missing_slice_or_disabled_reader_is_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(
            SliceScopes::at(tmp.path().join("absent"))
                .sample(Instant::now())
                .is_empty()
        );
        assert!(SliceScopes::disabled().sample(Instant::now()).is_empty());
        // A root that is not a cgroup2 mount never resolves, and the miss is
        // remembered until the retry window passes.
        let mut reader = SliceScopes::with(super::Locate::Discover {
            root: tmp.path().to_path_buf(),
            retry_at: None,
        });
        let now = Instant::now();
        assert!(reader.sample(now).is_empty());
        assert!(matches!(
            reader.locate,
            super::Locate::Discover {
                retry_at: Some(_),
                ..
            }
        ));
        assert!(reader.sample(now).is_empty());
    }
}
