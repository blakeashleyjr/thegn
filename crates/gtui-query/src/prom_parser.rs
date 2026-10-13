use std::collections::HashMap;

use gtui_core::datasource::QueryError;
use gtui_core::frame::{Field, FieldType, Frame};
use serde::Deserialize;

#[derive(Deserialize, Debug)]
struct PromResponse {
    status: String,
    data: Option<PromData>,
    error: Option<String>,
}

#[derive(Deserialize, Debug)]
struct PromData {
    #[serde(rename = "resultType")]
    result_type: String,
    result: serde_json::Value,
}

#[derive(Deserialize, Debug)]
struct PromResult {
    #[serde(default)]
    metric: HashMap<String, String>,
    values: Option<Vec<PromValueTuple>>,
    value: Option<PromValueTuple>,
}

#[derive(Deserialize, Debug)]
struct PromValueTuple(f64, String);

/// Parse a Prometheus vector or matrix response. Unparseable sample values are
/// retained as NaN so renderers show a gap at that sample instead of silently
/// shifting later timestamps or dropping the series.
pub fn parse_prometheus_response(json: &str) -> Result<Vec<Frame>, QueryError> {
    let resp: PromResponse =
        serde_json::from_str(json).map_err(|e| QueryError::Syntax(e.to_string()))?;

    if resp.status != "success" {
        return Err(QueryError::Other(
            resp.error
                .unwrap_or_else(|| "Unknown Prometheus error".to_string()),
        ));
    }

    let data = resp
        .data
        .ok_or_else(|| QueryError::Syntax("Missing data field".to_string()))?;
    let result_type = data.result_type;
    if !matches!(result_type.as_str(), "matrix" | "vector") {
        return Err(QueryError::Syntax(format!(
            "unsupported Prometheus resultType '{result_type}'"
        )));
    }
    let serde_json::Value::Array(items) = data.result else {
        return Err(QueryError::Syntax(format!(
            "Prometheus {result_type} result must be an array"
        )));
    };
    let mut parsed = Vec::with_capacity(items.len());
    for item in items {
        let item: PromResult =
            serde_json::from_value(item).map_err(|error| QueryError::Syntax(error.to_string()))?;
        let shape_matches = match result_type.as_str() {
            "matrix" => item.values.is_some(),
            "vector" => item.value.is_some(),
            _ => false,
        };
        if !shape_matches {
            return Err(QueryError::Syntax(format!(
                "Prometheus {result_type} result has the wrong sample shape"
            )));
        }
        parsed.push(item);
    }

    let names = series_names(
        parsed.iter().map(|result| {
            (
                result.metric.get("__name__").map(String::as_str),
                &result.metric,
            )
        }),
        "series",
    );
    let mut frames = Vec::with_capacity(parsed.len());
    for (result, series_name) in parsed.into_iter().zip(names) {
        let samples = match result_type.as_str() {
            "matrix" => result.values.unwrap_or_default(),
            "vector" => result.value.into_iter().collect(),
            _ => Vec::new(),
        };
        let mut times = Vec::with_capacity(samples.len());
        let mut values = Vec::with_capacity(samples.len());
        for sample in samples {
            times.push(sample.0);
            values.push(sample.1.parse::<f64>().unwrap_or(f64::NAN));
        }
        frames.push(Frame::new(vec![
            Field::new("time", FieldType::Time, times),
            Field::new(&series_name, FieldType::Float64, values),
        ]));
    }
    Ok(frames)
}

fn series_names<'a>(
    items: impl Iterator<Item = (Option<&'a str>, &'a HashMap<String, String>)>,
    fallback_prefix: &str,
) -> Vec<String> {
    let items: Vec<_> = items.collect();
    let bases: Vec<_> = items
        .iter()
        .map(|(base, _)| base.unwrap_or_default())
        .collect();
    let mut groups: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, base) in bases.iter().enumerate() {
        groups.entry(*base).or_default().push(index);
    }
    let mut distinguishing_by_base: HashMap<&str, Vec<&str>> = HashMap::new();
    for (base, indices) in &groups {
        let mut stats: HashMap<&str, (&str, usize, bool)> = HashMap::new();
        for index in indices {
            let labels = items[*index].1;
            for (key, value) in labels {
                if !base.is_empty() && key == "__name__" {
                    continue;
                }
                let stat = stats
                    .entry(key.as_str())
                    .or_insert((value.as_str(), 0, false));
                stat.1 += 1;
                if stat.0 != value.as_str() {
                    stat.2 = true;
                }
            }
        }
        let distinguishing = stats
            .into_iter()
            .filter_map(|(key, (_, present, differs))| {
                (differs || present != indices.len()).then_some(key)
            })
            .collect();
        distinguishing_by_base.insert(*base, distinguishing);
    }

    let mut names = Vec::with_capacity(items.len());
    for (index, ((_, labels), base_value)) in items.iter().zip(&bases).enumerate() {
        let group = &groups[base_value];
        let distinguishing = &distinguishing_by_base[base_value];
        let name = if !base_value.is_empty()
            && (group.len() == 1 || labels.keys().all(|key| key.as_str() == "__name__"))
        {
            (*base_value).to_string()
        } else if labels.is_empty() {
            format!("{fallback_prefix}_{index}")
        } else {
            let mut keys: Vec<_> = if distinguishing.is_empty() {
                if !base_value.is_empty() {
                    Vec::new()
                } else {
                    labels.keys().map(String::as_str).collect()
                }
            } else {
                distinguishing.clone()
            };
            keys.retain(|key| labels.contains_key(*key));
            keys.sort_unstable();
            if !base_value.is_empty() && keys.is_empty() {
                names.push((*base_value).to_string());
                continue;
            }
            let label_set = keys
                .iter()
                .map(|key| {
                    format!(
                        "{key}=\"{}\"",
                        escape_label_value(labels.get(*key).unwrap())
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            if base_value.is_empty() {
                format!("{{{label_set}}}")
            } else {
                format!("{base_value}{{{label_set}}}")
            }
        };
        names.push(name);
    }
    make_unique(names)
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

fn make_unique(names: Vec<String>) -> Vec<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_prometheus_instant() {
        let json = r#"{"status":"success","data":{"resultType":"vector","result":[{"metric":{"__name__":"up","job":"prometheus","instance":"localhost:9090"},"value":[1435781451.781,"1"]}]}}"#;
        let frames = parse_prometheus_response(json).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].fields.len(), 2);
        assert_eq!(frames[0].fields[0].name, "time");
        assert_eq!(frames[0].fields[1].name, "up");
    }

    #[test]
    fn test_parse_prometheus_range() {
        let json = r#"{"status":"success","data":{"resultType":"matrix","result":[{"metric":{"__name__":"up"},"values":[[1435781430.781,"1"],[1435781445.781,"1"],[1435781460.781,"1"]]}]}}"#;
        let frames = parse_prometheus_response(json).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].fields.len(), 2);
    }

    #[test]
    fn vector_series_names_include_only_distinguishing_labels() {
        let json = r#"{"status":"success","data":{"resultType":"vector","result":[{"metric":{"__name__":"up","job":"a","instance":"same"},"value":[1,"1"]},{"metric":{"__name__":"up","job":"b","instance":"same"},"value":[1,"1"]}]}}"#;
        let frames = parse_prometheus_response(json).unwrap();
        assert_eq!(frames[0].fields[1].name, "up{job=\"a\"}");
        assert_eq!(frames[1].fields[1].name, "up{job=\"b\"}");
        let second_json = r#"{"status":"success","data":{"resultType":"matrix","result":[{"metric":{"instance":"x"},"values":[[2,"1"]]},{"metric":{"instance":"y"},"values":[[2,"1"]]}]}}"#;
        let second_frames = parse_prometheus_response(second_json).unwrap();
        assert_eq!(second_frames[0].fields[1].name, "{instance=\"x\"}");
        assert_eq!(second_frames[1].fields[1].name, "{instance=\"y\"}");
    }

    #[test]
    fn matrix_without_metric_name_uses_labels_and_duplicate_names_are_unique() {
        let json = r#"{"status":"success","data":{"resultType":"matrix","result":[{"metric":{"instance":"x"},"values":[[1,"1"]]},{"metric":{"instance":"x"},"values":[[2,"2"]]}]}}"#;
        let frames = parse_prometheus_response(json).unwrap();
        assert_eq!(frames[0].fields[1].name, "{instance=\"x\"}");
        assert_eq!(frames[1].fields[1].name, "{instance=\"x\"} #2");
    }

    #[test]
    fn scalar_result_type_is_a_typed_syntax_error() {
        let json = r#"{"status":"success","data":{"resultType":"scalar","result":[1,"2"]}}"#;
        assert!(matches!(
            parse_prometheus_response(json),
            Err(QueryError::Syntax(message)) if message.contains("scalar")
        ));
    }

    #[test]
    fn matrix_and_vector_require_their_matching_sample_shapes() {
        for (result_type, sample) in [
            ("matrix", r#""value":[1,"1"]"#),
            ("vector", r#""values":[[1,"1"]]"#),
        ] {
            let json = format!(
                r#"{{"status":"success","data":{{"resultType":"{result_type}","result":[{{"metric":{{"__name__":"up"}},{sample}}}]}}}}"#
            );
            assert!(matches!(
                parse_prometheus_response(&json),
                Err(QueryError::Syntax(message)) if message.contains(result_type)
            ));
        }
    }

    #[test]
    fn malformed_sample_value_is_preserved_as_a_nan_gap() {
        let json = r#"{"status":"success","data":{"resultType":"vector","result":[{"metric":{"__name__":"up"},"value":[1,"not-a-number"]}]}}"#;
        let frames = parse_prometheus_response(json).unwrap();
        assert!(frames[0].fields[1].floats()[0].is_nan());
    }
}
