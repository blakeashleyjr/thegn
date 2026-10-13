use std::collections::{HashMap, HashSet};

use gtui_core::datasource::QueryError;
use gtui_core::frame::{Field, FieldType, Frame};
use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub struct LokiResponse {
    pub status: String,
    pub data: Option<LokiData>,
    pub error: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct LokiData {
    #[serde(rename = "resultType")]
    pub result_type: String,
    pub result: serde_json::Value,
}

#[derive(Deserialize, Debug)]
pub struct LokiResult {
    pub stream: std::collections::HashMap<String, String>,
    pub values: Vec<LokiValueTuple>,
}

#[derive(Deserialize, Debug)]
pub struct LokiValueTuple(String, String);

/// Parse Loki stream responses. Rows with timestamps that are not valid
/// nanosecond integers are skipped; valid rows retain their original order.
pub fn parse_loki_response(json: &str) -> Result<Vec<Frame>, QueryError> {
    let resp: LokiResponse =
        serde_json::from_str(json).map_err(|e| QueryError::Syntax(e.to_string()))?;

    if resp.status != "success" {
        return Err(QueryError::Other(
            resp.error
                .unwrap_or_else(|| "Unknown Loki error".to_string()),
        ));
    }

    let data = resp
        .data
        .ok_or_else(|| QueryError::Syntax("Missing data field".to_string()))?;
    if data.result_type != "streams" {
        return Err(QueryError::Syntax(format!(
            "unsupported Loki resultType '{}' (expected 'streams')",
            data.result_type
        )));
    }
    let result = data
        .result
        .as_array()
        .ok_or_else(|| QueryError::Syntax("Loki streams result must be an array".to_string()))?;
    let results = result
        .iter()
        .map(|item| {
            serde_json::from_value::<LokiResult>(item.clone())
                .map_err(|error| QueryError::Syntax(error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let mut frames = Vec::new();
    let names = series_names(&results);
    for (result, series_name) in results.into_iter().zip(names) {
        let mut times = Vec::new();
        let mut values = Vec::new();

        for v in result.values {
            // Loki timestamps are nanoseconds as strings
            if let Ok(ts_nano) = v.0.parse::<u64>() {
                // We'll store it as f64 seconds for the MVP Time Field representation
                let ts_sec = (ts_nano as f64) / 1_000_000_000.0;
                times.push(ts_sec);
                values.push(v.1);
            }
        }

        let time_field = Field::new("time", FieldType::Time, times);

        // The log lines themselves, as a real string series.
        let val_field = Field::new_str(&series_name, values);

        frames.push(Frame::new(vec![time_field, val_field]));
    }

    Ok(frames)
}

fn series_names(results: &[LokiResult]) -> Vec<String> {
    let bases: Vec<_> = results
        .iter()
        .map(|result| {
            result
                .stream
                .get("filename")
                .map(String::as_str)
                .unwrap_or_default()
        })
        .collect();
    let mut names = Vec::with_capacity(results.len());
    for (index, result) in results.iter().enumerate() {
        let base = bases[index];
        let group: Vec<_> = results
            .iter()
            .enumerate()
            .filter(|(other_index, _)| bases[*other_index] == base)
            .collect();
        let name = if !base.is_empty() && group.len() == 1 {
            base.to_string()
        } else if result.stream.is_empty() {
            format!("stream_{index}")
        } else {
            let mut keys: Vec<_> = result
                .stream
                .keys()
                .filter(|key| base.is_empty() || key.as_str() != "filename")
                .collect();
            keys.sort_unstable();
            let distinguishing: Vec<_> = keys
                .into_iter()
                .filter(|key| {
                    let mut values = HashSet::new();
                    group.iter().for_each(|(_, other)| {
                        values.insert(other.stream.get(*key));
                    });
                    values.len() > 1
                })
                .collect();
            let keys = if distinguishing.is_empty() {
                if base.is_empty() {
                    let mut keys: Vec<_> = result.stream.keys().collect();
                    keys.sort_unstable();
                    keys
                } else {
                    Vec::new()
                }
            } else {
                distinguishing
            };
            if !base.is_empty() && keys.is_empty() {
                names.push(base.to_string());
                continue;
            }
            let label_set = keys
                .iter()
                .map(|key| {
                    format!(
                        "{key}=\"{}\"",
                        escape_label_value(result.stream.get(*key).unwrap())
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            if base.is_empty() {
                format!("{{{label_set}}}")
            } else {
                format!("{base}{{{label_set}}}")
            }
        };
        names.push(name);
    }
    let mut seen: HashMap<String, usize> = HashMap::new();
    names
        .into_iter()
        .map(|name| {
            let count = seen.entry(name.clone()).or_default();
            *count += 1;
            if *count == 1 {
                name
            } else {
                format!("{name} #{}", *count)
            }
        })
        .collect()
}

fn escape_label_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            other => escaped.push(other),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_loki_response() {
        let json = r#"{
            "status": "success",
            "data": {
                "resultType": "streams",
                "result": [
                    {
                        "stream": {
                            "filename": "/var/log/syslog",
                            "job": "syslog"
                        },
                        "values": [
                            ["1569266497240578000", "foo"],
                            ["1569266492548155000", "bar"]
                        ]
                    }
                ]
            }
        }"#;

        let frames = parse_loki_response(json).unwrap();
        assert_eq!(frames.len(), 1);
        let frame = &frames[0];
        assert_eq!(frame.fields.len(), 2);

        // Check fields
        assert_eq!(frame.fields[0].name, "time");
        assert_eq!(frame.fields[1].name, "/var/log/syslog");
    }

    #[test]
    fn same_filename_streams_use_distinguishing_labels() {
        let json = r#"{"status":"success","data":{"resultType":"streams","result":[{"stream":{"filename":"/var/log/app","job":"a","host":"same"},"values":[["1000000000","a"]]},{"stream":{"filename":"/var/log/app","job":"b","host":"same"},"values":[["2000000000","b"]]}]}}"#;
        let frames = parse_loki_response(json).unwrap();
        assert_eq!(frames[0].fields[1].name, "/var/log/app{job=\"a\"}");
        assert_eq!(frames[1].fields[1].name, "/var/log/app{job=\"b\"}");
    }

    #[test]
    fn metric_result_type_is_a_typed_syntax_error() {
        let json = r#"{"status":"success","data":{"resultType":"matrix","result":[{"metric":{"__name__":"up"},"value":[1,"1"]}]}}"#;
        assert!(matches!(
            parse_loki_response(json),
            Err(QueryError::Syntax(message)) if message.contains("matrix")
        ));
    }

    #[test]
    fn invalid_timestamp_rows_are_skipped() {
        let json = r#"{"status":"success","data":{"resultType":"streams","result":[{"stream":{"filename":"/logs/app"},"values":[["invalid","dropped"],["2000000000","kept"]]}]}}"#;
        let frames = parse_loki_response(json).unwrap();
        assert_eq!(frames[0].fields[0].floats(), &[2.0]);
        assert_eq!(frames[0].fields[1].strings()[0], "kept");
    }
}
