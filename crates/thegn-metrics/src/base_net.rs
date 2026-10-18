//! Held `/proc/net/dev` reader for the base-tier network rates.

use std::io;
use std::path::{Path, PathBuf};

use crate::collect::{linux::procfile::ProcFile, parse::netdev::parse};

const PROC_NET_DEV: &str = "/proc/net/dev";

#[derive(Debug)]
pub(crate) struct BaseNet {
    path: PathBuf,
    file: Option<ProcFile>,
    prev: Vec<(String, u64, u64)>,
    primed: bool,
}

impl BaseNet {
    pub(crate) fn new() -> Self {
        Self::at(PROC_NET_DEV)
    }

    fn at(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        let mut reader = Self {
            file: match ProcFile::open(&path) {
                Ok(file) => Some(file),
                Err(_) => None,
            },
            path,
            prev: Vec::new(),
            primed: false,
        };
        // sysinfo's Networks::new_with_refreshed_list() primes counters before
        // the first ticker sample; retain that first-sample delta behaviour.
        if let Some(file) = reader.file.as_mut()
            && let Ok(bytes) = file.read_all()
            && let Some(rows) = counters(bytes)
        {
            reader.prev = rows;
            reader.primed = true;
        }
        reader
    }

    pub(crate) fn rates(
        &mut self,
        dt_secs: f64,
        retry_open: bool,
    ) -> Option<Vec<(String, u64, u64)>> {
        if self.file.is_none() && retry_open {
            self.file = match ProcFile::open(&self.path) {
                Ok(file) => Some(file),
                Err(_) => None,
            };
        }
        let Some(file) = self.file.as_mut() else {
            return None;
        };
        let bytes = match file.read_all() {
            Ok(bytes) => bytes,
            Err(error) => {
                let should_reopen = file.needs_reopen() || error.kind() == io::ErrorKind::NotFound;
                if should_reopen {
                    self.file = None;
                }
                self.prev.clear();
                self.primed = false;
                return None;
            }
        };
        let Some(rows) = counters(bytes) else {
            self.prev.clear();
            self.primed = false;
            return None;
        };
        let previous = if self.primed { &self.prev } else { &rows };
        let result = rates(previous, &rows, dt_secs);
        self.prev = rows;
        self.primed = true;
        Some(result)
    }
}

fn counters(bytes: &[u8]) -> Option<Vec<(String, u64, u64)>> {
    let mut rows = Vec::new();
    let parsed = parse(bytes, |name, counters| {
        if let (Some(rx), Some(tx)) = (counters.rx_bytes, counters.tx_bytes) {
            rows.push((String::from_utf8_lossy(name).into_owned(), rx, tx));
        }
    });
    parsed.map(|_| rows)
}

fn rates(
    prev: &[(String, u64, u64)],
    cur: &[(String, u64, u64)],
    dt_secs: f64,
) -> Vec<(String, u64, u64)> {
    let dt_secs = dt_secs.max(0.001);
    cur.iter()
        .map(|(name, rx, tx)| {
            let previous = prev.iter().find(|(previous, _, _)| previous == name);
            let (old_rx, old_tx) = previous.map_or((*rx, *tx), |(_, rx, tx)| (*rx, *tx));
            (
                name.clone(),
                (rx.saturating_sub(old_rx) as f64 / dt_secs) as u64,
                (tx.saturating_sub(old_tx) as f64 / dt_secs) as u64,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::os::unix::fs::FileExt;

    use super::{BaseNet, rates};
    use crate::NetMedium;
    use crate::net_kind::{self, IfaceClass, IfaceKind};

    const FIRST: &[u8] = b"Inter-| Receive | Transmit\n face |bytes packets errs drop fifo frame compressed multicast|bytes packets errs drop fifo colls carrier compressed\nlo: 100 0 0 0 0 0 0 0 200 0 0 0 0 0 0 0\nenp4s0: 1000 0 0 0 0 0 0 0 2000 0 0 0 0 0 0 0\nwlan0: 5000 0 0 0 0 0 0 0 7000 0 0 0 0 0 0 0\ntun0: 9000 0 0 0 0 0 0 0 11000 0 0 0 0 0 0 0\n";
    const SECOND: &[u8] = b"Inter-| Receive | Transmit\n face |bytes packets errs drop fifo frame compressed multicast|bytes packets errs drop fifo colls carrier compressed\nlo: 1100 0 0 0 0 0 0 0 2200 0 0 0 0 0 0 0\nenp4s0: 1300 0 0 0 0 0 0 0 2400 0 0 0 0 0 0 0\nwlan0: 5600 0 0 0 0 0 0 0 7800 0 0 0 0 0 0 0\ntun0: 9500 0 0 0 0 0 0 0 11700 0 0 0 0 0 0 0\n";

    #[test]
    fn captured_snapshots_match_rates_and_existing_aggregation() {
        let first = super::counters(FIRST).unwrap();
        let second = super::counters(SECOND).unwrap();
        let readings = rates(&first, &second, 1.0);
        assert_eq!(
            readings,
            [
                ("lo".to_owned(), 1000, 2000),
                ("enp4s0".to_owned(), 300, 400),
                ("wlan0".to_owned(), 600, 800),
                ("tun0".to_owned(), 500, 700),
            ]
        );
        let mut classes = std::collections::BTreeMap::new();
        classes.insert(
            "enp4s0".to_owned(),
            IfaceClass {
                kind: IfaceKind::Ethernet,
                bond_slave: false,
                has_device: true,
                medium: NetMedium::Ethernet,
            },
        );
        classes.insert(
            "wlan0".to_owned(),
            IfaceClass {
                kind: IfaceKind::Wifi,
                bond_slave: false,
                has_device: true,
                medium: NetMedium::Wifi,
            },
        );
        classes.insert(
            "tun0".to_owned(),
            IfaceClass {
                kind: IfaceKind::Tun,
                bond_slave: false,
                has_device: false,
                medium: NetMedium::Unknown,
            },
        );
        let up = [("enp4s0".to_owned(), true), ("wlan0".to_owned(), true)]
            .into_iter()
            .collect();
        let (_, physical, all) = net_kind::aggregate(
            readings
                .iter()
                .map(|(name, rx, tx)| (name.as_str(), *rx, *tx)),
            &classes,
            &up,
            true,
        );
        assert_eq!(physical, (900, 1200));
        assert_eq!(all, (1400, 1900));
    }

    #[test]
    fn resets_disappearing_interfaces_and_new_interfaces_use_saturating_deltas() {
        let previous = vec![("reset".to_owned(), 900, 700), ("gone".to_owned(), 10, 20)];
        let current = vec![("reset".to_owned(), 4, 3), ("new".to_owned(), 50, 80)];
        assert_eq!(
            rates(&previous, &current, 1.0),
            [("reset".to_owned(), 0, 0), ("new".to_owned(), 0, 0)]
        );
        assert_eq!(
            rates(&current, &[("gone".to_owned(), 500, 600)], 1.0),
            [("gone".to_owned(), 0, 0)]
        );
    }

    #[test]
    fn rates_keep_interface_names_longer_than_inline_collector_names() {
        let long_name = "enp12345678901234567890";
        let current = vec![(long_name.to_owned(), 12, 34)];
        assert_eq!(rates(&[], &current, 1.0)[0].0, long_name);
    }

    #[test]
    fn holds_the_open_file_and_reads_once_per_sample() {
        let dir = tempfile::Builder::new()
            .permissions(0o700)
            .tempdir()
            .unwrap();
        let path = dir.path().join("net.dev");
        fs::write(&path, FIRST).unwrap();
        let mut reader = BaseNet::at(&path);
        assert!(
            reader
                .rates(1.0, false)
                .unwrap()
                .iter()
                .any(|(name, rx, _)| name == "enp4s0" && *rx == 0)
        );

        let held_writer = File::options().write(true).open(&path).unwrap();
        fs::rename(&path, dir.path().join("old-net.dev")).unwrap();
        fs::write(
            &path,
            b"Inter-| Receive | Transmit\n face |bytes packets errs drop fifo frame compressed multicast|bytes packets errs drop fifo colls carrier compressed\nenp4s0: 9000 0 0 0 0 0 0 0 9000 0 0 0 0 0 0 0\n",
        )
        .unwrap();
        held_writer.set_len(0).unwrap();
        held_writer.write_at(SECOND, 0).unwrap();
        let result = reader.rates(1.0, false).unwrap();
        assert!(
            result
                .iter()
                .any(|(name, rx, _)| name == "enp4s0" && *rx == 300)
        );
    }

    #[test]
    fn unavailable_source_stays_absent_until_a_slow_retry() {
        let dir = tempfile::Builder::new()
            .permissions(0o700)
            .tempdir()
            .unwrap();
        let path = dir.path().join("net.dev");
        let mut reader = BaseNet::at(&path);
        assert!(reader.rates(1.0, false).is_none());
        fs::write(&path, FIRST).unwrap();
        assert!(reader.rates(1.0, false).is_none());
        assert_eq!(reader.rates(1.0, true).unwrap().len(), 4);
    }
}
