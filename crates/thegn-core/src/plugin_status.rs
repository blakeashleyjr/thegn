//! The UI-to-daemon `plugins.status` snapshot: file name, bounded wire types, encode/decode.

use serde::{Deserialize, Serialize};

pub const STATUS_FILE: &str = "plugins-status.json";
pub const STATUS_SCHEMA: u32 = 1;
pub const MAX_STATUS_ROWS: usize = 64;
pub const MAX_STATUS_BYTES: usize = 64 * 1024;
pub const MAX_PLUGIN_ID_BYTES: usize = 128;

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct BridgeCounts {
    pub calls: u64,
    pub timed_out: u64,
    pub cancelled: u64,
    pub late_replies: u64,
    pub malformed: u64,
    pub closed_wakeups: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginStatusRow {
    pub plugin: String,
    pub in_flight: u64,
    pub rejected: u64,
    pub timed_out: u64,
    pub lost: u64,
    pub queue_drops: u64,
    pub coalesced: u64,
    pub bridge: BridgeCounts,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginStatusSnapshot {
    pub schema: u32,
    pub pid: u32,
    pub published_at_ms: i64,
    pub rows: Vec<PluginStatusRow>,
}

/// Clamp a snapshot to the wire bounds and serialise it.
///
/// Empty plugin ids are omitted, ids are truncated on a UTF-8 boundary, the
/// row count is capped, and the schema is set to [`STATUS_SCHEMA`].
pub fn encode(snapshot: &PluginStatusSnapshot) -> Result<Vec<u8>, String> {
    let bounded = PluginStatusSnapshot {
        schema: STATUS_SCHEMA,
        pid: snapshot.pid,
        published_at_ms: snapshot.published_at_ms,
        rows: snapshot
            .rows
            .iter()
            .filter_map(|row| {
                if row.plugin.is_empty() {
                    return None;
                }
                let mut end = row.plugin.len().min(MAX_PLUGIN_ID_BYTES);
                while !row.plugin.is_char_boundary(end) {
                    end -= 1;
                }
                Some(PluginStatusRow {
                    plugin: row.plugin[..end].to_owned(),
                    in_flight: row.in_flight,
                    rejected: row.rejected,
                    timed_out: row.timed_out,
                    lost: row.lost,
                    queue_drops: row.queue_drops,
                    coalesced: row.coalesced,
                    bridge: row.bridge,
                })
            })
            .take(MAX_STATUS_ROWS)
            .collect(),
    };
    let bytes = serde_json::to_vec(&bounded)
        .map_err(|error| format!("plugin status snapshot is not valid JSON: {error}"))?;
    if bytes.len() > MAX_STATUS_BYTES {
        return Err(format!(
            "plugin status snapshot is {} bytes (max {MAX_STATUS_BYTES})",
            bytes.len()
        ));
    }
    Ok(bytes)
}

/// Parse and validate a bounded plugin status snapshot.
pub fn decode(bytes: &[u8]) -> Result<PluginStatusSnapshot, String> {
    if bytes.len() > MAX_STATUS_BYTES {
        return Err(format!(
            "plugin status snapshot is {} bytes (max {MAX_STATUS_BYTES})",
            bytes.len()
        ));
    }
    let snapshot: PluginStatusSnapshot = serde_json::from_slice(bytes)
        .map_err(|error| format!("plugin status snapshot is not valid JSON: {error}"))?;
    if snapshot.schema != STATUS_SCHEMA {
        return Err(format!(
            "plugin status snapshot schema {} is not {STATUS_SCHEMA}",
            snapshot.schema
        ));
    }
    if snapshot.rows.len() > MAX_STATUS_ROWS {
        return Err(format!(
            "plugin status snapshot has {} rows (max {MAX_STATUS_ROWS})",
            snapshot.rows.len()
        ));
    }
    if snapshot
        .rows
        .iter()
        .any(|row| row.plugin.len() > MAX_PLUGIN_ID_BYTES)
    {
        return Err(format!(
            "plugin status snapshot has a plugin id over {MAX_PLUGIN_ID_BYTES} bytes"
        ));
    }
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::{
        BridgeCounts, MAX_PLUGIN_ID_BYTES, MAX_STATUS_BYTES, MAX_STATUS_ROWS, PluginStatusRow,
        PluginStatusSnapshot, STATUS_SCHEMA, decode, encode,
    };

    fn sample_snapshot() -> PluginStatusSnapshot {
        PluginStatusSnapshot {
            schema: STATUS_SCHEMA,
            pid: 42,
            published_at_ms: 1234,
            rows: vec![PluginStatusRow {
                plugin: "example".to_owned(),
                in_flight: 1,
                rejected: 2,
                timed_out: 3,
                lost: 4,
                queue_drops: 5,
                coalesced: 6,
                bridge: BridgeCounts {
                    calls: 7,
                    timed_out: 8,
                    cancelled: 9,
                    late_replies: 10,
                    malformed: 11,
                    closed_wakeups: 12,
                },
            }],
        }
    }

    #[test]
    fn encode_then_decode_round_trips_a_snapshot() {
        let snapshot = sample_snapshot();
        let decoded = decode(&encode(&snapshot).expect("encode snapshot")).expect("decode bytes");
        assert_eq!(decoded, snapshot);
    }

    #[test]
    fn encode_clamps_rows_ids_and_schema() {
        let mut rows = (0..70)
            .map(|index| PluginStatusRow {
                plugin: format!("plugin-{index:03}"),
                ..Default::default()
            })
            .collect::<Vec<_>>();
        rows[0].plugin = format!("{}é{}", "a".repeat(127), "b".repeat(171));
        rows[1].plugin.clear();
        let input = PluginStatusSnapshot {
            schema: 99,
            rows,
            ..Default::default()
        };
        let decoded = decode(&encode(&input).expect("bounded snapshot")).expect("decode bytes");
        assert_eq!(decoded.schema, STATUS_SCHEMA);
        assert!(decoded.rows.len() <= MAX_STATUS_ROWS);
        assert!(decoded.rows.iter().all(|row| !row.plugin.is_empty()));
        assert!(
            decoded
                .rows
                .iter()
                .all(|row| row.plugin.len() <= MAX_PLUGIN_ID_BYTES)
        );
        assert_eq!(decoded.rows[0].plugin, "a".repeat(127));
    }

    #[test]
    fn decode_refuses_oversized_wrong_schema_too_many_rows_long_ids_and_garbage() {
        assert_eq!(
            decode(&vec![b' '; MAX_STATUS_BYTES + 1]).unwrap_err(),
            format!(
                "plugin status snapshot is {} bytes (max {MAX_STATUS_BYTES})",
                MAX_STATUS_BYTES + 1
            )
        );
        let wrong_schema = PluginStatusSnapshot {
            schema: STATUS_SCHEMA + 1,
            ..Default::default()
        };
        assert_eq!(
            decode(&serde_json::to_vec(&wrong_schema).expect("serialize wrong schema"))
                .unwrap_err(),
            format!(
                "plugin status snapshot schema {} is not {STATUS_SCHEMA}",
                STATUS_SCHEMA + 1
            )
        );
        let too_many = PluginStatusSnapshot {
            schema: STATUS_SCHEMA,
            rows: (0..=MAX_STATUS_ROWS)
                .map(|index| PluginStatusRow {
                    plugin: format!("p{index}"),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        assert_eq!(
            decode(&serde_json::to_vec(&too_many).expect("serialize rows")).unwrap_err(),
            format!(
                "plugin status snapshot has {} rows (max {MAX_STATUS_ROWS})",
                MAX_STATUS_ROWS + 1
            )
        );
        let long_id = PluginStatusSnapshot {
            schema: STATUS_SCHEMA,
            rows: vec![PluginStatusRow {
                plugin: "x".repeat(MAX_PLUGIN_ID_BYTES + 1),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            decode(&serde_json::to_vec(&long_id).expect("serialize long id")).unwrap_err(),
            format!("plugin status snapshot has a plugin id over {MAX_PLUGIN_ID_BYTES} bytes")
        );
        let parse_error = serde_json::from_slice::<PluginStatusSnapshot>(b"{")
            .expect_err("incomplete JSON is rejected");
        assert_eq!(
            decode(b"{").unwrap_err(),
            format!("plugin status snapshot is not valid JSON: {parse_error}")
        );
    }

    #[test]
    fn encode_refuses_a_snapshot_over_the_byte_bound() {
        let bytes = encode(&sample_snapshot()).expect("normal snapshot");
        assert!(bytes.len() < MAX_STATUS_BYTES);
        assert!(decode(&vec![b' '; MAX_STATUS_BYTES + 1]).is_err());

        let oversized = PluginStatusSnapshot {
            pid: u32::MAX,
            published_at_ms: i64::MAX,
            rows: (0..MAX_STATUS_ROWS)
                .map(|_| PluginStatusRow {
                    plugin: "\0".repeat(MAX_PLUGIN_ID_BYTES),
                    in_flight: u64::MAX,
                    rejected: u64::MAX,
                    timed_out: u64::MAX,
                    lost: u64::MAX,
                    queue_drops: u64::MAX,
                    coalesced: u64::MAX,
                    bridge: BridgeCounts {
                        calls: u64::MAX,
                        timed_out: u64::MAX,
                        cancelled: u64::MAX,
                        late_replies: u64::MAX,
                        malformed: u64::MAX,
                        closed_wakeups: u64::MAX,
                    },
                })
                .collect(),
            ..Default::default()
        };
        let mut encoded_shape = oversized.clone();
        encoded_shape.schema = STATUS_SCHEMA;
        let encoded_len = serde_json::to_vec(&encoded_shape)
            .expect("serialize escaped ids")
            .len();
        assert!(encoded_len > MAX_STATUS_BYTES);
        assert_eq!(
            encode(&oversized).unwrap_err(),
            format!("plugin status snapshot is {encoded_len} bytes (max {MAX_STATUS_BYTES})")
        );
    }
}
