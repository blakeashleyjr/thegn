use super::{ContainerStatsSampler, MAX_CONTAINER_SCOPES_PER_TICK, ResolvedContainerScope};
use crate::collect::linux::cgroup::{CgroupFs, DeviceFilter};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Default)]
struct FakeFs {
    files: HashMap<PathBuf, Vec<u8>>,
    reads: usize,
    paths: Vec<PathBuf>,
}

impl CgroupFs for FakeFs {
    fn read(&mut self, path: &Path, buf: &mut Vec<u8>) -> io::Result<usize> {
        self.reads += 1;
        self.paths.push(path.to_path_buf());
        let bytes = self
            .files
            .get(path)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        buf.clear();
        buf.extend_from_slice(bytes);
        Ok(bytes.len())
    }
}

fn scope(id: impl Into<String>, path: impl Into<PathBuf>) -> ResolvedContainerScope {
    ResolvedContainerScope::new(id, path).unwrap()
}

fn root() -> PathBuf {
    PathBuf::from("cgroup-root")
}

fn set_group(fs: &mut FakeFs, path: &Path, usage: u64, read: u64, write: u64, peak: u64) {
    let files = [
        (
            "cpu.stat",
            format!("usage_usec {usage}\nuser_usec 0\nsystem_usec 0\n"),
        ),
        ("memory.current", "4096\n".to_owned()),
        ("memory.peak", format!("{peak}\n")),
        ("memory.stat", "anon 1\n".to_owned()),
        ("memory.events", "low 0\n".to_owned()),
        ("memory.swap.current", "0\n".to_owned()),
        ("pids.current", "3\n".to_owned()),
        (
            "cpu.pressure",
            "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\n".to_owned(),
        ),
        (
            "memory.pressure",
            "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\n".to_owned(),
        ),
        (
            "io.pressure",
            "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\n".to_owned(),
        ),
        (
            "io.stat",
            format!("8:0 rbytes={read} wbytes={write} rios=1 wios=1\n"),
        ),
    ];
    for (name, value) in files {
        fs.files.insert(path.join(name), value.into_bytes());
    }
}

fn filter() -> DeviceFilter {
    let temp = tempfile::tempdir().unwrap();
    let disk = temp.path().join("nvme0n1");
    std::fs::create_dir_all(disk.join("device")).unwrap();
    std::fs::write(disk.join("dev"), b"8:0\n").unwrap();
    DeviceFilter::from_sys_block(temp.path())
}

#[test]
fn rates_prime_then_derive_cpu_and_io_and_counter_rollback_is_a_gap() {
    let path = PathBuf::from("group");
    let mut fs = FakeFs::default();
    set_group(&mut fs, &root().join(&path), 1_000_000, 100, 200, 4096);
    let mut sampler = ContainerStatsSampler::new(fs, filter(), root());
    let scopes = [scope("container-a", path.clone())];
    let start = Instant::now();
    let first = sampler.sample(&scopes, start).to_vec();
    assert_eq!(first.len(), 1);
    assert!(std::sync::Arc::ptr_eq(&first[0].id, &scopes[0].id));
    assert!(
        sampler
            .fs
            .paths
            .contains(&root().join(&path).join("cpu.stat"))
    );
    assert_eq!(first[0].cpu_pct, None);
    assert_eq!(first[0].io_read_bps, None);
    assert!(!first[0].gap);

    set_group(
        &mut sampler.fs,
        &root().join(&path),
        1_250_000,
        1100,
        2200,
        4096,
    );
    let second = sampler.sample(&scopes, start + Duration::from_secs(1));
    assert_eq!(second[0].cpu_pct, Some(25.0));
    assert_eq!(second[0].io_read_bps, Some(1000.0));
    assert_eq!(second[0].io_write_bps, Some(2000.0));
    assert_eq!(second[0].mem_current, Some(4096));
    assert_eq!(second[0].pids, Some(3));

    set_group(&mut sampler.fs, &root().join(&path), 5, 1, 1, 2);
    let rollback = sampler.sample(&scopes, start + Duration::from_secs(2));
    assert!(rollback[0].gap);
    assert_eq!(rollback[0].cpu_pct, None);
    assert_eq!(rollback[0].io_read_bps, None);
    assert_eq!(rollback[0].io_write_bps, None);
    assert_eq!(sampler.previous["container-a"].cpu_pct, 25.0);
}

#[test]
fn missing_scope_drops_previous_sample_and_reappearance_primes_again() {
    let path = PathBuf::from("group");
    let mut fs = FakeFs::default();
    set_group(&mut fs, &root().join(&path), 1_000_000, 100, 200, 4096);
    let mut sampler = ContainerStatsSampler::new(fs, filter(), root());
    let scopes = [scope("container-a", path.clone())];
    let start = Instant::now();
    assert_eq!(sampler.sample(&scopes, start).len(), 1);
    sampler.fs.files.clear();
    assert!(
        sampler
            .sample(&scopes, start + Duration::from_secs(1))
            .is_empty()
    );
    assert_eq!(sampler.previous.len(), 0);
    set_group(
        &mut sampler.fs,
        &root().join(&path),
        1_500_000,
        500,
        600,
        4096,
    );
    let restored = sampler.sample(&scopes, start + Duration::from_secs(2));
    assert_eq!(restored[0].cpu_pct, None);
    assert!(!restored[0].gap);
}

#[test]
fn caps_reads_at_64_and_round_robins_every_scope_within_three_ticks() {
    let mut fs = FakeFs::default();
    let scopes: Vec<ResolvedContainerScope> = (0..70)
        .map(|index| {
            let path = PathBuf::from(format!("group-{index}"));
            set_group(&mut fs, &root().join(&path), 1_000_000, 100, 200, 4096);
            scope(format!("container-{index}"), path)
        })
        .collect();
    let mut sampler = ContainerStatsSampler::new(fs, filter(), root());
    let start = Instant::now();
    let first: Vec<String> = sampler
        .sample(&scopes, start)
        .iter()
        .map(|row| row.id.to_string())
        .collect();
    assert_eq!(first.len(), MAX_CONTAINER_SCOPES_PER_TICK);
    assert!(sampler.fs.reads <= MAX_CONTAINER_SCOPES_PER_TICK * 11);
    // Two first-tick rotating scopes become busy before their next sample.
    for index in 56..58 {
        set_group(
            &mut sampler.fs,
            &root().join(format!("group-{index}")),
            2_000_000,
            100,
            200,
            4096,
        );
    }
    let second = sampler
        .sample(&scopes, start + Duration::from_secs(1))
        .to_vec();
    assert_eq!(second.len(), MAX_CONTAINER_SCOPES_PER_TICK);
    assert!(
        second
            .iter()
            .any(|row| &*row.id == "container-56" && row.cpu_pct == Some(100.0))
    );
    assert!(
        second
            .iter()
            .any(|row| &*row.id == "container-57" && row.cpu_pct == Some(100.0))
    );
    let third = sampler.sample(&scopes, start + Duration::from_secs(2));
    let observed: std::collections::HashSet<&str> = first
        .iter()
        .map(String::as_str)
        .chain(second.iter().map(|row| row.id.as_ref()))
        .chain(third.iter().map(|row| row.id.as_ref()))
        .collect();
    assert_eq!(observed.len(), scopes.len());
    assert!(sampler.fs.paths.iter().all(|path| path.starts_with(root())));
}

#[test]
fn mixed_cpu_scopes_all_get_sampled_despite_priority_churn() {
    let count = 120;
    let mut fs = FakeFs::default();
    let scopes: Vec<ResolvedContainerScope> = (0..count)
        .map(|index| {
            let path = PathBuf::from(format!("mixed-{index}"));
            set_group(&mut fs, &root().join(&path), 1_000_000, 100, 200, 4096);
            scope(format!("mixed-container-{index}"), path)
        })
        .collect();
    let mut sampler = ContainerStatsSampler::new(fs, filter(), root());
    let start = Instant::now();
    let warm_scopes = &scopes[..64];
    sampler.sample(warm_scopes, start);
    for index in 56..64 {
        set_group(
            &mut sampler.fs,
            &root().join(format!("mixed-{index}")),
            2_000_000,
            100,
            200,
            4096,
        );
    }
    sampler.sample(warm_scopes, start + Duration::from_secs(1));
    assert!(
        sampler
            .previous
            .get(&scopes[56].id)
            .is_some_and(|previous| previous.cpu_pct > 0.0)
    );
    let mut observed_cold = std::collections::HashSet::new();
    let max_ticks = (count - 56).div_ceil(8) + 1;
    let mut usage = [1_000_000_u64; 24];
    usage[..8].fill(2_000_000);
    for tick in 0..max_ticks {
        let hot_start = (tick * 8) % usage.len();
        for offset in 0..8 {
            let hot_index = (hot_start + offset) % usage.len();
            usage[hot_index] = usage[hot_index].saturating_add(1_000_000);
        }
        for index in 56..80 {
            set_group(
                &mut sampler.fs,
                &root().join(format!("mixed-{index}")),
                usage[index - 56],
                100,
                200,
                4096,
            );
        }
        let rows = sampler.sample(&scopes, start + Duration::from_secs(tick as u64 + 2));
        observed_cold.extend(
            rows.iter()
                .filter(|row| {
                    row.id.starts_with("mixed-container-")
                        && row.id[16..].parse::<usize>().is_ok_and(|index| index >= 80)
                })
                .map(|row| row.id.to_string()),
        );
        assert!(sampler.fs.reads <= (tick + 3) * MAX_CONTAINER_SCOPES_PER_TICK * 11);
    }
    assert_eq!(observed_cold.len(), 40);
}

#[test]
fn buffer_and_previous_table_reuse_capacity_over_steady_ticks() {
    let mut fs = FakeFs::default();
    let scopes: Vec<ResolvedContainerScope> = (0..64)
        .map(|index| {
            let path = PathBuf::from(format!("group-{index}"));
            set_group(&mut fs, &root().join(&path), 1_000_000, 100, 200, 4096);
            scope(format!("container-{index}"), path)
        })
        .collect();
    let mut sampler = ContainerStatsSampler::new(fs, filter(), root());
    let start = Instant::now();
    for tick in 0..1000 {
        let rows = sampler.sample(&scopes, start + Duration::from_secs(tick));
        assert_eq!(rows.len(), 64);
    }
    let buffer_capacity = sampler.buffer_capacity();
    let previous_capacity = sampler.previous_capacity();
    let row_capacity = sampler.rows.capacity();
    let prioritized_capacity = sampler.prioritized.capacity();
    let selected_capacity = sampler.selected.capacity();
    let priority_capacity = sampler.priority.capacity();
    for tick in 1000..2000 {
        let rows = sampler.sample(&scopes, start + Duration::from_secs(tick));
        assert_eq!(rows.len(), 64);
    }
    assert_eq!(sampler.buffer_capacity(), buffer_capacity);
    assert_eq!(sampler.previous_capacity(), previous_capacity);
    assert_eq!(sampler.rows.capacity(), row_capacity);
    assert_eq!(sampler.prioritized.capacity(), prioritized_capacity);
    assert_eq!(sampler.selected.capacity(), selected_capacity);
    assert_eq!(sampler.priority.capacity(), priority_capacity);
    assert_eq!(sampler.fs.reads, 64 * 11 * 2000);
}
