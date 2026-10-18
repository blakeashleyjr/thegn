//! Held-fd `/proc/net/dev` collector with bounded interface identities.
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use super::procfile::ProcFile;
use crate::collect::{
    CollectError, Collector, CollectorId, TickCtx,
    delta::Counter,
    env::Env,
    frame::MonitorFrame,
    health::{Absent, Health},
    instances::{Family, InstanceId, Interned, Interner},
    model::netdev::{IfaceName, IfaceSample, NetdevPart, ScopeTotal},
    parse::netdev::{NetdevCounters, parse},
    policy::net_scope::{IfaceKind, Scope},
    schedule::Cadence,
};
use crate::net_kind;

#[derive(Debug, Clone)]
struct InterfaceState {
    kind: IfaceKind,
    scope: Scope,
    rates: [Counter; 8],
}

impl InterfaceState {
    fn new(kind: IfaceKind, scope: Scope) -> Self {
        Self {
            kind,
            scope,
            rates: std::array::from_fn(|_| Counter::new()),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct ParsedRow {
    name: IfaceName,
    counters: NetdevCounters,
}

#[derive(Debug)]
pub struct NetdevCollector {
    path: PathBuf,
    sysfs_root: PathBuf,
    file: Option<ProcFile>,
    interner: Interner,
    states: HashMap<u16, InterfaceState>,
    rows: Vec<ParsedRow>,
    seen: Vec<InstanceId>,
    stale: Vec<u16>,
}

impl NetdevCollector {
    pub fn at(proc_net_dev: impl AsRef<Path>, sysfs_root: impl AsRef<Path>) -> Self {
        Self {
            path: proc_net_dev.as_ref().to_path_buf(),
            sysfs_root: sysfs_root.as_ref().to_path_buf(),
            file: None,
            interner: Interner::new(Family::Iface),
            states: HashMap::new(),
            rows: Vec::new(),
            seen: Vec::new(),
            stale: Vec::new(),
        }
    }
}

impl Collector for NetdevCollector {
    fn id(&self) -> CollectorId {
        CollectorId::Netdev
    }
    fn cadence(&self) -> Cadence {
        Cadence::Every(1)
    }

    fn discover(&mut self, _env: &Env) -> Health {
        if self.file.is_some() {
            return Health::Warming;
        }
        match ProcFile::open(&self.path) {
            Ok(file) => {
                self.file = Some(file);
                Health::Warming
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Health::Absent(Absent::NotOnThisOs)
            }
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                Health::Absent(Absent::NoPermission)
            }
            Err(_) => Health::Warming.after_error(0),
        }
    }

    fn sample(&mut self, ctx: &TickCtx, frame: &mut MonitorFrame) -> Result<(), CollectError> {
        let Some(file) = self.file.as_mut() else {
            return Err(CollectError::Gone);
        };
        let bytes = match file.read_all() {
            Ok(bytes) => bytes,
            Err(error) => {
                if file.needs_reopen() {
                    self.file = None;
                }
                return Err(error.into());
            }
        };
        let rows = &mut self.rows;
        rows.clear();
        let Some(_count) = parse(bytes, |name, counters| {
            rows.push(ParsedRow {
                name: IfaceName::new(name),
                counters: counters.clone(),
            })
        }) else {
            return Err(CollectError::Parse("netdev: missing header"));
        };
        self.seen.clear();
        let root = &self.sysfs_root;
        let rows = &self.rows;
        let interner = &mut self.interner;
        let states = &mut self.states;
        let seen = &mut self.seen;
        let stale = &mut self.stale;
        let mut truncated = false;
        let part = &mut frame.parts.netdev;
        part.ifaces.clear();
        part.totals = [ScopeTotal::default(); 5];
        part.all = ScopeTotal::default();
        for row in rows {
            let name = row.name.as_str();
            if name.is_empty() {
                truncated = true;
                continue;
            }
            let interned = interner.intern(name);
            let Interned::Id(id) = interned else {
                truncated = true;
                continue;
            };
            seen.push(id);
            let state = states.entry(id.0).or_insert_with(|| {
                let facts = net_kind::facts_at(root, name);
                let kind = crate::collect::policy::net_scope::classify(name, &facts);
                let scope = crate::collect::policy::net_scope::scope(kind, facts.bonding_slave);
                InterfaceState::new(kind, scope)
            });
            if state.kind == IfaceKind::Other {
                truncated = true;
                continue;
            }
            let counters = &row.counters;
            let mut sample = IfaceSample {
                id,
                name: row.name,
                kind: state.kind,
                scope: state.scope,
                counters: counters.clone(),
                ..Default::default()
            };
            sample.rx_bps = rate(&mut state.rates[0], counters.rx_bytes, ctx.now_ms);
            sample.tx_bps = rate(&mut state.rates[1], counters.tx_bytes, ctx.now_ms);
            sample.rx_pps = rate(&mut state.rates[2], counters.rx_packets, ctx.now_ms);
            sample.tx_pps = rate(&mut state.rates[3], counters.tx_packets, ctx.now_ms);
            sample.rx_errs_ps = rate(&mut state.rates[4], counters.rx_errs, ctx.now_ms);
            sample.tx_errs_ps = rate(&mut state.rates[5], counters.tx_errs, ctx.now_ms);
            sample.rx_drop_ps = rate(&mut state.rates[6], counters.rx_drop, ctx.now_ms);
            sample.tx_drop_ps = rate(&mut state.rates[7], counters.tx_drop, ctx.now_ms);
            part.ifaces.push(sample);
        }
        stale.clear();
        stale.extend(
            states
                .keys()
                .copied()
                .filter(|id| !seen.iter().any(|seen| seen.0 == *id)),
        );
        for id in stale.iter().copied() {
            states.remove(&id);
            interner.retire(InstanceId(id));
        }
        part.truncated = truncated || interner.truncated();
        aggregate(part);
        frame.sampled.insert(CollectorId::Netdev);
        Ok(())
    }

    fn close(&mut self) {
        self.file = None;
    }
}

fn rate(counter: &mut Counter, value: Option<u64>, now_ms: u64) -> Option<f64> {
    value.and_then(|current| counter.rate(current, now_ms))
}

fn aggregate(part: &mut NetdevPart) {
    for iface in &part.ifaces {
        if iface.scope != Scope::Loopback {
            add(&mut part.all, iface.rx_bps, iface.tx_bps);
        }
        add(
            &mut part.totals[iface.scope.index()],
            iface.rx_bps,
            iface.tx_bps,
        );
    }
}

fn add(total: &mut ScopeTotal, rx: Option<f64>, tx: Option<f64>) {
    if let Some(value) = rx {
        total.rx_bps = Some(total.rx_bps.unwrap_or(0.0) + value);
    }
    if let Some(value) = tx {
        total.tx_bps = Some(total.tx_bps.unwrap_or(0.0) + value);
    }
}

#[cfg(test)]
mod tests {
    use super::{IfaceSample, NetdevCollector};
    use crate::collect::{
        Collector, TickCtx, env::Env, frame::MonitorFrame, health::Health, policy::net_scope::Scope,
    };
    use std::fs::{self, File};
    use std::os::unix::fs::FileExt;

    fn proc(rx: u64, tx: u64, vpn_rx: u64, vpn_tx: u64, lo_rx: u64, lo_tx: u64) -> Vec<u8> {
        format!("Inter-| Receive | Transmit\n face |bytes packets errs drop fifo frame compressed multicast|bytes packets errs drop fifo colls carrier compressed\nwlan0: {rx} 1 0 0 0 0 0 0 {tx} 1 0 0 0 0 0 0\nwg0-mullvad:{vpn_rx} 1 0 0 0 0 0 0 {vpn_tx} 1 0 0 0 0 0 0\nenp95s0: 50 1 0 0 0 0 0 0 60 1 0 0 0 0 0 0\nlo: {lo_rx} 1 0 0 0 0 0 0 {lo_tx} 1 0 0 0 0 0 0\n").into_bytes()
    }

    #[test]
    fn g1_scopes_prevent_vpn_double_count_and_keep_down_physical_link() {
        let dir = tempfile::tempdir().unwrap();
        let proc_path = dir.path().join("netdev");
        let sys = dir.path().join("sys");
        for name in ["wlan0", "wg0-mullvad", "enp95s0", "lo"] {
            fs::create_dir_all(sys.join(name)).unwrap();
        }
        fs::create_dir_all(sys.join("wlan0/device")).unwrap();
        fs::create_dir_all(sys.join("wlan0/wireless")).unwrap();
        fs::write(sys.join("wlan0/type"), "1\n").unwrap();
        fs::write(sys.join("wlan0/uevent"), "DEVTYPE=wlan\n").unwrap();
        fs::write(sys.join("wg0-mullvad/uevent"), "DEVTYPE=wireguard\n").unwrap();
        fs::write(sys.join("enp95s0/type"), "1\n").unwrap();
        fs::create_dir_all(sys.join("enp95s0/device")).unwrap();
        fs::write(sys.join("enp95s0/operstate"), "down\n").unwrap();
        fs::write(sys.join("lo/type"), "772\n").unwrap();
        fs::write(&proc_path, proc(1000, 2000, 100, 200, 772, 772)).unwrap();
        let mut collector = NetdevCollector::at(&proc_path, &sys);
        assert_eq!(collector.discover(&Env::default()), Health::Warming);
        let mut frame = MonitorFrame::default();
        collector
            .sample(
                &TickCtx {
                    tick: 1,
                    now_ms: 1000,
                },
                &mut frame,
            )
            .unwrap();
        let writer = File::options().write(true).open(&proc_path).unwrap();
        writer.set_len(0).unwrap();
        writer
            .write_at(&proc(1100, 2200, 160, 280, 1772, 1772), 0)
            .unwrap();
        collector
            .sample(
                &TickCtx {
                    tick: 2,
                    now_ms: 2000,
                },
                &mut frame,
            )
            .unwrap();
        let part = &frame.parts.netdev;
        assert_eq!(part.totals[Scope::Physical.index()].rx_bps, Some(100.0));
        assert_eq!(part.totals[Scope::Vpn.index()].rx_bps, Some(60.0));
        assert_eq!(part.all.rx_bps, Some(160.0));
        assert_ne!(part.totals[ScopeIndex::PHYSICAL], part.all);
        assert!(part.ifaces.iter().any(|row| row.name.as_str() == "enp95s0"
            && row.scope == Scope::Physical
            && row.rx_bps == Some(0.0)));
    }

    fn one_iface(rx: u64) -> Vec<u8> {
        format!("Inter-| Receive | Transmit\n face |bytes packets errs drop fifo frame compressed multicast|bytes packets errs drop fifo colls carrier compressed\nfoo0: {rx} 1 0 0 0 0 0 0 {rx} 1 0 0 0 0 0 0\n").into_bytes()
    }

    fn rewrite(path: &std::path::Path, content: &[u8]) {
        let writer = File::options().write(true).open(path).unwrap();
        writer.set_len(0).unwrap();
        writer.write_at(content, 0).unwrap();
    }

    fn sample_one(
        collector: &mut NetdevCollector,
        path: &std::path::Path,
        frame: &mut MonitorFrame,
        now_ms: u64,
        input: &[u8],
    ) -> IfaceSample {
        rewrite(path, input);
        collector
            .sample(
                &TickCtx {
                    tick: now_ms,
                    now_ms,
                },
                frame,
            )
            .unwrap();
        assert_eq!(frame.parts.netdev.ifaces.len(), 1);
        frame.parts.netdev.ifaces.first().unwrap().clone()
    }

    #[test]
    fn counter_reset_and_reappearance_reprime_without_changing_interface_id() {
        let dir = tempfile::tempdir().unwrap();
        let proc_path = dir.path().join("netdev");
        fs::write(&proc_path, one_iface(100)).unwrap();
        let mut collector = NetdevCollector::at(&proc_path, dir.path().join("sys"));
        assert_eq!(collector.discover(&Env::default()), Health::Warming);
        let mut frame = MonitorFrame::default();
        let first = sample_one(
            &mut collector,
            &proc_path,
            &mut frame,
            1000,
            &one_iface(100),
        );
        assert_eq!(first.rx_bps, None);
        let normal = sample_one(
            &mut collector,
            &proc_path,
            &mut frame,
            2000,
            &one_iface(200),
        );
        assert_eq!(normal.rx_bps, Some(100.0));
        let reset = sample_one(&mut collector, &proc_path, &mut frame, 3000, &one_iface(5));
        assert_eq!(reset.rx_bps, None);
        let recovered = sample_one(&mut collector, &proc_path, &mut frame, 4000, &one_iface(15));
        assert_eq!(recovered.rx_bps, Some(10.0));
        let empty = b"Inter-| Receive | Transmit\n face |bytes packets errs drop fifo frame compressed multicast|bytes packets errs drop fifo colls carrier compressed\n";
        rewrite(&proc_path, empty);
        collector
            .sample(
                &TickCtx {
                    tick: 5000,
                    now_ms: 5000,
                },
                &mut frame,
            )
            .unwrap();
        assert!(frame.parts.netdev.ifaces.is_empty());
        let returned = sample_one(&mut collector, &proc_path, &mut frame, 6000, &one_iface(25));
        assert_eq!(returned.id, first.id);
        assert_eq!(returned.rx_bps, None);
    }

    #[test]
    fn interface_cap_marks_truncation_at_thirty_three_rows() {
        let dir = tempfile::tempdir().unwrap();
        let proc_path = dir.path().join("netdev");
        let mut input = String::from(
            "Inter-| Receive | Transmit\n face |bytes packets errs drop fifo frame compressed multicast|bytes packets errs drop fifo colls carrier compressed\n",
        );
        for iface in 0..33 {
            input.push_str(&format!("if{iface}: 1 1 0 0 0 0 0 0 1 1 0 0 0 0 0 0\n"));
        }
        fs::write(&proc_path, input).unwrap();
        let mut collector = NetdevCollector::at(&proc_path, dir.path().join("sys"));
        assert_eq!(collector.discover(&Env::default()), Health::Warming);
        let mut frame = MonitorFrame::default();
        collector
            .sample(
                &TickCtx {
                    tick: 1,
                    now_ms: 1000,
                },
                &mut frame,
            )
            .unwrap();
        assert_eq!(frame.parts.netdev.ifaces.len(), 32);
        assert!(frame.parts.netdev.truncated);
    }
}
