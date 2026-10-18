//! Pure parser for UPower history filenames and tab-separated samples.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryKind {
    Charge,
    Rate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryState {
    Charging,
    Discharging,
    Empty,
    FullyCharged,
    PendingCharge,
    PendingDischarge,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HistoryPoint {
    pub unix_s: u64,
    pub value: f32,
    pub state: HistoryState,
}

pub fn parse_name(name: &str) -> Option<(HistoryKind, &str)> {
    let name = name.strip_suffix(".dat")?;
    let (kind, id) = name.strip_prefix("history-")?.split_once('-')?;
    if id.is_empty() {
        return None;
    }
    let kind = match kind {
        "charge" => HistoryKind::Charge,
        "rate" => HistoryKind::Rate,
        _ => return None,
    };
    Some((kind, id))
}

pub fn parse(bytes: &[u8], now_s: u64, out: &mut Vec<HistoryPoint>) -> usize {
    out.clear();
    let mut skipped = 0;
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let parsed = (|| {
            let mut fields = line.split(|byte| *byte == b'\t');
            let timestamp = fields.next()?;
            let raw_value = fields.next()?;
            let raw_state = fields.next()?;
            if fields.next().is_some() {
                return None;
            }
            let Ok(timestamp) = std::str::from_utf8(timestamp) else {
                return None;
            };
            let Ok(unix_s) = timestamp.trim().parse::<u64>() else {
                return None;
            };
            let Ok(raw_value) = std::str::from_utf8(raw_value) else {
                return None;
            };
            let Ok(value) = raw_value.trim().parse::<f32>() else {
                return None;
            };
            if unix_s > now_s || !value.is_finite() || value < 0.0 {
                return None;
            }
            let Ok(state_text) = std::str::from_utf8(raw_state) else {
                return None;
            };
            let state_text = state_text.trim();
            let state = match state_text {
                "charging" => HistoryState::Charging,
                "discharging" => HistoryState::Discharging,
                "empty" => HistoryState::Empty,
                "fully-charged" => HistoryState::FullyCharged,
                "pending-charge" => HistoryState::PendingCharge,
                "pending-discharge" => HistoryState::PendingDischarge,
                _ => HistoryState::Unknown,
            };
            Some(HistoryPoint {
                unix_s,
                value,
                state,
            })
        })();
        if let Some(point) = parsed {
            out.push(point);
        } else {
            skipped += 1;
        }
    }
    skipped
}

#[cfg(test)]
mod tests {
    use super::{HistoryKind, HistoryPoint, HistoryState, parse, parse_name};
    #[test]
    fn names_reject_temporary_and_unrecognized_files() {
        assert_eq!(
            parse_name("history-charge-BAT0.dat"),
            Some((HistoryKind::Charge, "BAT0"))
        );
        assert_eq!(parse_name("history-rate-X.dat.4H92Q3"), None);
        assert_eq!(parse_name("history-time-full-X.dat"), None);
    }
    #[test]
    fn parser_reuses_output_and_counts_invalid_rows() {
        let bytes = b"100\t50.5\tcharging\n101\t49\tdischarging\n\nnope\n102\tnan\tcharging\n103\t-1\tempty\n201\t2\tempty\n100\t2\n";
        let mut points = vec![HistoryPoint {
            unix_s: 0,
            value: 0.0,
            state: HistoryState::Unknown,
        }];
        assert_eq!(parse(bytes, 200, &mut points), 5);
        assert_eq!(
            points,
            vec![
                HistoryPoint {
                    unix_s: 100,
                    value: 50.5,
                    state: HistoryState::Charging
                },
                HistoryPoint {
                    unix_s: 101,
                    value: 49.0,
                    state: HistoryState::Discharging
                },
            ]
        );
    }
}
