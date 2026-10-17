//! Shared Linux cpufreq policy and power-state reader.
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::procfile::ProcFile;
use crate::collect::frame::MonitorFrame;
use crate::collect::health::{Absent, Health};
use crate::collect::model::cpu_policy::{CpuPolicy, FreqSource, PowerPolicy};
use crate::collect::parse::cpufreq::{
    parse_cpu_list, parse_flag, parse_khz, parse_token, parse_token_list, parse_u32,
};
use crate::collect::schedule::Cadence;
use crate::collect::{CollectError, Collector, CollectorId, TickCtx, env::Env};

const FD_CAP: usize = 256;

trait AttrRead: Send {
    fn read(&mut self) -> io::Result<&[u8]>;
    fn needs_reopen(&self) -> bool {
        false
    }
}
impl AttrRead for ProcFile {
    fn read(&mut self) -> io::Result<&[u8]> {
        self.read_all()
    }
    fn needs_reopen(&self) -> bool {
        ProcFile::needs_reopen(self)
    }
}

struct Attr {
    path: Option<PathBuf>,
    reader: Option<Box<dyn AttrRead>>,
}
impl Attr {
    fn open(path: PathBuf, held: &mut usize, cap: usize) -> Self {
        if !path.exists() {
            return Self {
                path: None,
                reader: None,
            };
        }
        let (path, reader) = if *held < cap {
            match ProcFile::open(&path) {
                Ok(file) => {
                    *held += 1;
                    (Some(path), Some(Box::new(file) as Box<dyn AttrRead>))
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => (None, None),
                Err(_) => (Some(path), None),
            }
        } else {
            (Some(path), None)
        };
        Self { path, reader }
    }
    fn read(&mut self, held: &mut usize, cap: usize, reopen: bool) -> Option<Vec<u8>> {
        if reopen
            && self
                .reader
                .as_ref()
                .is_some_and(|reader| reader.needs_reopen())
        {
            self.reader = None;
            *held = (*held).saturating_sub(1);
        }
        if let Some(reader) = self.reader.as_mut() {
            return reader.read().ok().map(|bytes| bytes.to_vec());
        }
        let path = self.path.as_ref()?;
        if *held < cap
            && let Ok(file) = ProcFile::open(path)
        {
            self.reader = Some(Box::new(file));
            *held += 1;
            return self
                .reader
                .as_mut()
                .and_then(|reader| reader.read().ok())
                .map(|bytes| bytes.to_vec());
        }
        ProcFile::open(path)
            .ok()
            .and_then(|mut file| file.read_all().ok().map(|bytes| bytes.to_vec()))
    }
}
struct PolicyFiles {
    policy: CpuPolicy,
    cur: Attr,
    cur_fallback: Option<PathBuf>,
    scaling_min: Attr,
    scaling_max: Attr,
    governor: Attr,
    epp: Attr,
    boost: Attr,
    rank: Attr,
}
struct PowerFiles {
    boost: Option<Attr>,
    no_turbo: Option<Attr>,
    profile: Option<Attr>,
}

pub struct CpuFreqCollector {
    root: PathBuf,
    policies: Vec<PolicyFiles>,
    power_files: Option<PowerFiles>,
    power: PowerPolicy,
    held: usize,
    cap: usize,
    last_slow: Option<u64>,
}
impl CpuFreqCollector {
    pub fn new(root: PathBuf) -> Self {
        Self::with_cap(root, FD_CAP)
    }
    fn with_cap(root: PathBuf, cap: usize) -> Self {
        Self {
            root,
            policies: Vec::new(),
            power_files: None,
            power: PowerPolicy::default(),
            held: 0,
            cap,
            last_slow: None,
        }
    }
    pub fn held_count(&self) -> usize {
        self.held
    }
    fn sys(&self, path: &str) -> PathBuf {
        self.root.join("sys").join(path)
    }
    fn dynamic(&mut self, path: PathBuf) -> Attr {
        Attr::open(path, &mut self.held, self.cap)
    }
}

pub fn slow_due(last: Option<u64>, tick: u64) -> bool {
    last.is_none_or(|previous| tick.saturating_sub(previous) >= 5)
}

impl Collector for CpuFreqCollector {
    fn id(&self) -> CollectorId {
        CollectorId::CpuFreq
    }
    fn cadence(&self) -> Cadence {
        Cadence::Every(1)
    }
    fn discover(&mut self, _env: &Env) -> Health {
        self.close();
        self.policies.clear();
        self.power = PowerPolicy::default();
        self.last_slow = None;
        let base = self.sys("devices/system/cpu/cpufreq");
        let mut dirs = match fs::read_dir(&base) {
            Ok(entries) => entries
                .filter_map(Result::ok)
                .filter_map(|entry| {
                    let name = entry.file_name();
                    let text = name.to_str()?;
                    let number = text.strip_prefix("policy")?.parse::<u16>().ok()?;
                    Some((number, entry.path()))
                })
                .collect::<Vec<_>>(),
            Err(_) => Vec::new(),
        };
        dirs.sort_by_key(|(number, _)| *number);
        let mut first_epp_choices = None;
        let mut policy_dirs = Vec::with_capacity(dirs.len());
        for (number, path) in dirs {
            let mut policy = CpuPolicy {
                policy: number,
                ..CpuPolicy::default()
            };
            if let Some(bytes) = read_static(path.join("related_cpus")) {
                parse_cpu_list(&bytes, &mut policy.cpus);
            }
            policy.hw_min_khz = read_static(path.join("cpuinfo_min_freq"))
                .as_deref()
                .and_then(parse_khz);
            policy.hw_max_khz = read_static(path.join("cpuinfo_max_freq"))
                .as_deref()
                .and_then(parse_khz);
            policy.driver = read_static(path.join("scaling_driver"))
                .as_deref()
                .and_then(parse_token);
            policy.highest_perf = read_static(path.join("amd_pstate_highest_perf"))
                .as_deref()
                .and_then(parse_u32);
            if first_epp_choices.is_none() {
                first_epp_choices =
                    read_static(path.join("energy_performance_available_preferences"))
                        .map(|b| parse_token_list(&b));
            }
            policy_dirs.push((policy, path));
        }
        // Current frequency is sampled every tick, so reserve held descriptors for
        // every policy before allocating the slower attributes.
        let mut policy_attrs = Vec::with_capacity(policy_dirs.len());
        for (policy, path) in policy_dirs {
            let avg = path.join("cpuinfo_avg_freq");
            let scaling_cur = path.join("scaling_cur_freq");
            let (cur_path, source, cur_fallback) = if avg.exists() {
                (avg, FreqSource::Avg, Some(scaling_cur))
            } else {
                (scaling_cur, FreqSource::Scaling, None)
            };
            let cur = self.dynamic(cur_path);
            policy_attrs.push((policy, path, cur, source, cur_fallback));
        }
        for (policy, path, cur, source, cur_fallback) in policy_attrs {
            let scaling_min = self.dynamic(path.join("scaling_min_freq"));
            let scaling_max = self.dynamic(path.join("scaling_max_freq"));
            let governor = self.dynamic(path.join("scaling_governor"));
            let epp = self.dynamic(path.join("energy_performance_preference"));
            let boost = self.dynamic(path.join("boost"));
            let rank = self.dynamic(path.join("amd_pstate_prefcore_ranking"));
            let mut files = PolicyFiles {
                policy,
                cur,
                cur_fallback,
                scaling_min,
                scaling_max,
                governor,
                epp,
                boost,
                rank,
            };
            // Record the preferred source; sampling falls back to scaling frequency if avg fails.
            files.policy.cur_source = Some(source);
            self.policies.push(files);
        }
        let profile_path = self.sys("firmware/acpi/platform_profile");
        let profile_choices = read_static(self.sys("firmware/acpi/platform_profile_choices"))
            .map(|b| parse_token_list(&b))
            .unwrap_or_default();
        let status_static = read_static(self.sys("devices/system/cpu/amd_pstate/status"))
            .as_deref()
            .and_then(parse_token);
        let prefcore_static = read_static(self.sys("devices/system/cpu/amd_pstate/prefcore"))
            .as_deref()
            .and_then(parse_flag);
        let boost_path = base.join("boost");
        let no_turbo_path = self.sys("devices/system/cpu/intel_pstate/no_turbo");
        let profile = profile_path.exists().then(|| self.dynamic(profile_path));
        let boost = boost_path.exists().then(|| self.dynamic(boost_path));
        let no_turbo =
            (boost.is_none() && no_turbo_path.exists()).then(|| self.dynamic(no_turbo_path));
        self.power = PowerPolicy {
            amd_pstate_status: status_static,
            amd_prefcore: prefcore_static,
            platform_profile_choices: profile_choices,
            epp_choices: first_epp_choices.unwrap_or_default(),
            ..PowerPolicy::default()
        };
        self.power_files = Some(PowerFiles {
            boost,
            no_turbo,
            profile,
        });
        if self.policies.is_empty()
            && self
                .power_files
                .as_ref()
                .is_none_or(|files| files.profile.is_none())
        {
            Health::Absent(Absent::NoHardware)
        } else {
            Health::Ok
        }
    }
    fn sample(&mut self, ctx: &TickCtx, frame: &mut MonitorFrame) -> Result<(), CollectError> {
        let slow = slow_due(self.last_slow, ctx.tick);
        if slow {
            self.last_slow = Some(ctx.tick);
        }
        for files in &mut self.policies {
            let mut cur = files
                .cur
                .read(&mut self.held, self.cap, slow)
                .and_then(|bytes| parse_khz(&bytes));
            if cur.is_some() && files.cur_fallback.is_some() {
                files.policy.cur_source = Some(FreqSource::Avg);
            }
            if cur.is_none()
                && files.policy.cur_source == Some(FreqSource::Avg)
                && let Some(path) = files.cur_fallback.as_ref()
            {
                cur = ProcFile::open(path)
                    .ok()
                    .and_then(|mut file| file.read_all().ok().and_then(parse_khz));
                if cur.is_some() {
                    files.policy.cur_source = Some(FreqSource::Scaling);
                }
            }
            files.policy.cur_khz = cur;
            if slow {
                files.policy.scaling_min_khz = files
                    .scaling_min
                    .read(&mut self.held, self.cap, true)
                    .and_then(|bytes| parse_khz(&bytes));
                files.policy.scaling_max_khz = files
                    .scaling_max
                    .read(&mut self.held, self.cap, true)
                    .and_then(|bytes| parse_khz(&bytes));
                files.policy.governor = files
                    .governor
                    .read(&mut self.held, self.cap, true)
                    .and_then(|bytes| parse_token(&bytes));
                files.policy.epp = files
                    .epp
                    .read(&mut self.held, self.cap, true)
                    .and_then(|bytes| parse_token(&bytes));
                files.policy.boost = files
                    .boost
                    .read(&mut self.held, self.cap, true)
                    .and_then(|bytes| parse_flag(&bytes));
                files.policy.prefcore_rank = files
                    .rank
                    .read(&mut self.held, self.cap, true)
                    .and_then(|bytes| parse_u32(&bytes));
            }
        }
        if slow && let Some(power) = &mut self.power_files {
            let boost = power
                .boost
                .as_mut()
                .and_then(|attr| attr.read(&mut self.held, self.cap, true))
                .and_then(|bytes| parse_flag(&bytes));
            let no_turbo = power
                .no_turbo
                .as_mut()
                .and_then(|attr| attr.read(&mut self.held, self.cap, true))
                .and_then(|bytes| parse_flag(&bytes));
            self.power.boost = boost.or_else(|| no_turbo.map(|disabled| !disabled));
            self.power.platform_profile = power
                .profile
                .as_mut()
                .and_then(|attr| attr.read(&mut self.held, self.cap, true))
                .and_then(|bytes| parse_token(&bytes));
        }
        let part = &mut frame.parts.cpu_freq;
        part.policies.clear();
        part.policies
            .extend(self.policies.iter().map(|files| files.policy));
        part.power = self.power;
        frame.sampled.insert(CollectorId::CpuFreq);
        Ok(())
    }
    fn close(&mut self) {
        self.policies.clear();
        self.power_files = None;
        self.held = 0;
    }
}

fn read_static(path: impl AsRef<Path>) -> Option<Vec<u8>> {
    fs::read(path).ok()
}

#[cfg(test)]
mod tests {
    use super::{AttrRead, CpuFreqCollector, slow_due};
    use crate::collect::{
        Collector,
        env::Env,
        health::{Absent, Health},
    };
    use std::fs;
    use std::io;
    use tempfile::tempdir;
    fn policy(root: &std::path::Path, n: u16, cpus: &str, max: &str) {
        let dir = root
            .join("sys/devices/system/cpu/cpufreq")
            .join(format!("policy{n}"));
        fs::create_dir_all(&dir).unwrap();
        for (name, value) in [
            ("related_cpus", cpus),
            ("cpuinfo_min_freq", "400000"),
            ("cpuinfo_max_freq", max),
            ("cpuinfo_avg_freq", "1200000"),
            ("scaling_min_freq", "400000"),
            ("scaling_max_freq", max),
            ("scaling_governor", "schedutil"),
            ("scaling_driver", "amd-pstate-epp"),
            ("energy_performance_preference", "balance_performance"),
            (
                "energy_performance_available_preferences",
                "performance balance_performance powersave",
            ),
            ("boost", "1"),
            ("amd_pstate_prefcore_ranking", "120"),
        ] {
            fs::write(dir.join(name), value).unwrap();
        }
    }
    #[test]
    fn synthetic_policies_are_discovered_and_sampled() {
        let root = tempdir().unwrap();
        for number in 0..24_u16 {
            let high = (0..4).contains(&number) || (12..16).contains(&number);
            policy(
                root.path(),
                number,
                &number.to_string(),
                if high { "5100000" } else { "3300000" },
            );
            if high {
                fs::write(
                    root.path().join(format!(
                        "sys/devices/system/cpu/cpufreq/policy{number}/amd_pstate_prefcore_ranking"
                    )),
                    "200",
                )
                .unwrap();
            }
        }
        let acpi = root.path().join("sys/firmware/acpi");
        fs::create_dir_all(&acpi).unwrap();
        fs::write(acpi.join("platform_profile"), "balanced").unwrap();
        fs::write(
            acpi.join("platform_profile_choices"),
            "low-power balanced performance",
        )
        .unwrap();
        fs::write(
            root.path().join("sys/devices/system/cpu/cpufreq/boost"),
            "1",
        )
        .unwrap();
        let amd = root.path().join("sys/devices/system/cpu/amd_pstate");
        fs::create_dir_all(&amd).unwrap();
        fs::write(amd.join("status"), "active").unwrap();
        fs::write(amd.join("prefcore"), "1").unwrap();
        let mut collector = CpuFreqCollector::new(root.path().to_path_buf());
        assert_eq!(collector.discover(&Env::default()), Health::Ok);
        let mut frame = crate::collect::frame::MonitorFrame::default();
        collector
            .sample(&crate::collect::TickCtx { tick: 0, now_ms: 0 }, &mut frame)
            .unwrap();
        assert_eq!(frame.parts.cpu_freq.policies.len(), 24);
        assert_eq!(frame.parts.cpu_freq.policies[0].policy, 0);
        assert_eq!(frame.parts.cpu_freq.policies[0].hw_max_khz, Some(5_100_000));
        assert_eq!(frame.parts.cpu_freq.policies[0].prefcore_rank, Some(200));
        assert_eq!(frame.parts.cpu_freq.policies[4].hw_max_khz, Some(3_300_000));
        assert_eq!(frame.parts.cpu_freq.policies[4].prefcore_rank, Some(120));
        assert_eq!(frame.parts.cpu_freq.power.boost, Some(true));
        assert_eq!(
            frame
                .parts
                .cpu_freq
                .power
                .platform_profile
                .unwrap()
                .as_str(),
            "balanced"
        );
        assert_eq!(frame.parts.cpu_freq.power.platform_profile_choices.len(), 3);
        assert_eq!(frame.parts.cpu_freq.power.epp_choices.len(), 3);
        assert_eq!(
            frame
                .parts
                .cpu_freq
                .power
                .amd_pstate_status
                .unwrap()
                .as_str(),
            "active"
        );
        assert_eq!(frame.parts.cpu_freq.power.amd_prefcore, Some(true));
        assert!(frame.sampled.contains(crate::collect::CollectorId::CpuFreq));
        assert!(!frame.parts.cpu_freq.any_capped());
        collector.close();
        assert_eq!(collector.held_count(), 0);
    }
    #[test]
    fn no_hardware_is_absent_and_platform_profile_alone_is_supported() {
        let root = tempdir().unwrap();
        let mut collector = CpuFreqCollector::new(root.path().to_path_buf());
        assert_eq!(
            collector.discover(&Env::default()),
            Health::Absent(Absent::NoHardware)
        );
        let mut frame = crate::collect::frame::MonitorFrame::default();
        collector
            .sample(&crate::collect::TickCtx { tick: 0, now_ms: 0 }, &mut frame)
            .unwrap();
        assert!(frame.parts.cpu_freq.policies.is_empty());
        fs::create_dir_all(root.path().join("sys/firmware/acpi")).unwrap();
        fs::write(
            root.path().join("sys/firmware/acpi/platform_profile"),
            "balanced",
        )
        .unwrap();
        assert_eq!(collector.discover(&Env::default()), Health::Ok);
        collector
            .sample(&crate::collect::TickCtx { tick: 0, now_ms: 0 }, &mut frame)
            .unwrap();
        assert_eq!(
            frame
                .parts
                .cpu_freq
                .power
                .platform_profile
                .unwrap()
                .as_str(),
            "balanced"
        );
    }

    #[test]
    fn intel_hybrid_uses_policy_classes_and_inverts_no_turbo() {
        let root = tempdir().unwrap();
        for number in 0..16_u16 {
            policy(
                root.path(),
                number,
                &number.to_string(),
                if number < 8 { "5400000" } else { "4300000" },
            );
        }
        let base = root.path().join("sys/devices/system/cpu/cpufreq");
        for number in 0..16_u16 {
            fs::write(
                base.join(format!("policy{number}/scaling_driver")),
                "intel_pstate",
            )
            .unwrap();
            fs::remove_file(base.join(format!("policy{number}/amd_pstate_prefcore_ranking")))
                .unwrap();
        }
        let intel = root.path().join("sys/devices/system/cpu/intel_pstate");
        fs::create_dir_all(&intel).unwrap();
        fs::write(intel.join("no_turbo"), "0").unwrap();
        let mut collector = CpuFreqCollector::new(root.path().to_path_buf());
        collector.discover(&Env::default());
        let mut frame = crate::collect::frame::MonitorFrame::default();
        collector
            .sample(&crate::collect::TickCtx { tick: 0, now_ms: 0 }, &mut frame)
            .unwrap();
        assert_eq!(frame.parts.cpu_freq.policies.len(), 16);
        assert_eq!(frame.parts.cpu_freq.policies[0].hw_max_khz, Some(5_400_000));
        assert_eq!(frame.parts.cpu_freq.policies[8].hw_max_khz, Some(4_300_000));
        assert_eq!(
            frame.parts.cpu_freq.policies[0].driver.unwrap().as_str(),
            "intel_pstate"
        );
        assert_eq!(frame.parts.cpu_freq.power.boost, Some(true));
        assert_eq!(frame.parts.cpu_freq.policies[0].prefcore_rank, None);
        assert_eq!(frame.parts.cpu_freq.policies[0].highest_perf, None);
        fs::write(intel.join("no_turbo"), "1").unwrap();
        collector
            .sample(&crate::collect::TickCtx { tick: 5, now_ms: 5 }, &mut frame)
            .unwrap();
        assert_eq!(frame.parts.cpu_freq.power.boost, Some(false));
    }

    #[test]
    fn scaling_frequency_is_used_when_average_is_unavailable() {
        let root = tempdir().unwrap();
        policy(root.path(), 0, "0-3", "3000000");
        let dir = root.path().join("sys/devices/system/cpu/cpufreq/policy0");
        fs::remove_file(dir.join("cpuinfo_avg_freq")).unwrap();
        fs::write(dir.join("scaling_cur_freq"), "1800000").unwrap();
        let mut collector = CpuFreqCollector::new(root.path().to_path_buf());
        collector.discover(&Env::default());
        let mut frame = crate::collect::frame::MonitorFrame::default();
        collector
            .sample(&crate::collect::TickCtx { tick: 0, now_ms: 0 }, &mut frame)
            .unwrap();
        assert_eq!(frame.parts.cpu_freq.policies[0].cur_khz, Some(1_800_000));
        assert_eq!(
            frame.parts.cpu_freq.policies[0].cur_source,
            Some(crate::collect::model::cpu_policy::FreqSource::Scaling)
        );
    }

    #[test]
    fn scaling_frequency_fallback_is_used_when_average_read_fails() {
        let root = tempdir().unwrap();
        policy(root.path(), 0, "0-3", "3000000");
        let dir = root.path().join("sys/devices/system/cpu/cpufreq/policy0");
        fs::write(dir.join("scaling_cur_freq"), "1800000").unwrap();
        let mut collector = CpuFreqCollector::new(root.path().to_path_buf());
        collector.discover(&Env::default());
        collector.policies[0].cur.reader = Some(Box::new(BrokenRead { failed: false }));
        let mut frame = crate::collect::frame::MonitorFrame::default();
        collector
            .sample(&crate::collect::TickCtx { tick: 0, now_ms: 0 }, &mut frame)
            .unwrap();
        assert_eq!(frame.parts.cpu_freq.policies[0].cur_khz, Some(1_800_000));
        assert_eq!(
            frame.parts.cpu_freq.policies[0].cur_source,
            Some(crate::collect::model::cpu_policy::FreqSource::Scaling)
        );
    }
    #[test]
    fn slow_pass_is_due_first_then_each_five_ticks() {
        assert!(slow_due(None, 12));
        assert!(!slow_due(Some(0), 4));
        assert!(slow_due(Some(0), 5));
    }

    #[test]
    fn held_files_reread_and_slow_fields_wait_for_the_slow_pass() {
        let root = tempdir().unwrap();
        policy(root.path(), 0, "0-3", "5000000");
        let dir = root.path().join("sys/devices/system/cpu/cpufreq/policy0");
        let mut collector = CpuFreqCollector::new(root.path().to_path_buf());
        assert_eq!(collector.discover(&Env::default()), Health::Ok);
        let mut frame = crate::collect::frame::MonitorFrame::default();
        let sample = |collector: &mut CpuFreqCollector,
                      frame: &mut crate::collect::frame::MonitorFrame,
                      tick| {
            collector
                .sample(&crate::collect::TickCtx { tick, now_ms: tick }, frame)
                .unwrap();
        };
        sample(&mut collector, &mut frame, 0);
        assert_eq!(frame.parts.cpu_freq.policies[0].cur_khz, Some(1_200_000));
        fs::write(dir.join("cpuinfo_avg_freq"), "2200000").unwrap();
        fs::write(dir.join("scaling_governor"), "performance").unwrap();
        sample(&mut collector, &mut frame, 1);
        assert_eq!(frame.parts.cpu_freq.policies[0].cur_khz, Some(2_200_000));
        assert_eq!(
            frame.parts.cpu_freq.policies[0].governor.unwrap().as_str(),
            "schedutil"
        );
        sample(&mut collector, &mut frame, 5);
        assert_eq!(
            frame.parts.cpu_freq.policies[0].governor.unwrap().as_str(),
            "performance"
        );
    }

    #[test]
    fn arm_cluster_policies_keep_related_cpu_ranges_and_detect_caps() {
        let root = tempdir().unwrap();
        policy(root.path(), 0, "0-3", "3000000");
        policy(root.path(), 4, "4-7", "3000000");
        let second = root.path().join("sys/devices/system/cpu/cpufreq/policy4");
        fs::write(second.join("scaling_max_freq"), "2000000").unwrap();
        let mut collector = CpuFreqCollector::new(root.path().to_path_buf());
        collector.discover(&Env::default());
        let mut frame = crate::collect::frame::MonitorFrame::default();
        collector
            .sample(&crate::collect::TickCtx { tick: 0, now_ms: 0 }, &mut frame)
            .unwrap();
        assert_eq!(frame.parts.cpu_freq.policies[0].cpus.len(), 4);
        assert!(frame.parts.cpu_freq.policies[0].cpus.contains(3));
        assert_eq!(frame.parts.cpu_freq.policies[1].cpus.first(), Some(4));
        assert!(frame.parts.cpu_freq.any_capped());
    }

    #[derive(Debug)]
    struct BrokenRead {
        failed: bool,
    }
    impl AttrRead for BrokenRead {
        fn read(&mut self) -> io::Result<&[u8]> {
            self.failed = true;
            Err(io::Error::from_raw_os_error(libc::ENODEV))
        }
        fn needs_reopen(&self) -> bool {
            self.failed
        }
    }

    #[test]
    fn enodev_degrades_only_the_failed_attribute() {
        let root = tempdir().unwrap();
        policy(root.path(), 0, "0", "5000000");
        let mut collector = CpuFreqCollector::new(root.path().to_path_buf());
        collector.discover(&Env::default());
        collector.policies[0].epp.reader = Some(Box::new(BrokenRead { failed: false }));
        let mut frame = crate::collect::frame::MonitorFrame::default();
        assert!(
            collector
                .sample(&crate::collect::TickCtx { tick: 0, now_ms: 0 }, &mut frame)
                .is_ok()
        );
        let policy = &frame.parts.cpu_freq.policies[0];
        assert_eq!(policy.epp, None);
        assert_eq!(policy.cur_khz, Some(1_200_000));
        assert_eq!(policy.governor.unwrap().as_str(), "schedutil");
        collector
            .sample(&crate::collect::TickCtx { tick: 5, now_ms: 5 }, &mut frame)
            .unwrap();
        assert_eq!(
            frame.parts.cpu_freq.policies[0].epp.unwrap().as_str(),
            "balance_performance"
        );
    }

    #[test]
    fn dynamic_fd_cap_is_respected() {
        let root = tempdir().unwrap();
        for number in 0..3_u16 {
            policy(root.path(), number, &number.to_string(), "5000000");
        }
        fs::remove_file(
            root.path()
                .join("sys/devices/system/cpu/cpufreq/policy2/amd_pstate_prefcore_ranking"),
        )
        .unwrap();
        fs::write(
            root.path().join("sys/devices/system/cpu/cpufreq/boost"),
            "1",
        )
        .unwrap();
        let mut collector = CpuFreqCollector::with_cap(root.path().to_path_buf(), 8);
        collector.discover(&Env::default());
        assert_eq!(collector.held_count(), 8);
        let mut frame = crate::collect::frame::MonitorFrame::default();
        collector
            .sample(&crate::collect::TickCtx { tick: 0, now_ms: 0 }, &mut frame)
            .unwrap();
        assert_eq!(frame.parts.cpu_freq.policies[2].prefcore_rank, None);
    }

    #[test]
    fn frequency_descriptors_are_reserved_for_every_policy() {
        let root = tempdir().unwrap();
        for number in 0..3_u16 {
            policy(root.path(), number, &number.to_string(), "5000000");
        }
        let mut collector = CpuFreqCollector::with_cap(root.path().to_path_buf(), 4);
        collector.discover(&Env::default());
        let mut frame = crate::collect::frame::MonitorFrame::default();
        collector
            .sample(&crate::collect::TickCtx { tick: 0, now_ms: 0 }, &mut frame)
            .unwrap();
        assert!(
            frame
                .parts
                .cpu_freq
                .policies
                .iter()
                .all(|policy| policy.cur_khz.is_some())
        );
        assert_eq!(
            frame.parts.cpu_freq.policies[2].governor.unwrap().as_str(),
            "schedutil"
        );
    }
}
