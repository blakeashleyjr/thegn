use std::collections::{HashMap, HashSet};

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
    let result_type = data.result_type.as_str();
    if !matches!(result_type, "matrix" | "vector") {
        return Err(QueryError::Syntax(format!(
            "unsupported Prometheus resultType '{result_type}'"
        )));
    }
    let result = data.result.as_array().ok_or_else(|| {
        QueryError::Syntax(format!("Prometheus {result_type} result must be an array"))
    })?;
    let mut parsed = Vec::with_capacity(result.len());
    for item in result {
        let item: PromResult = serde_json::from_value(item.clone())
            .map_err(|error| QueryError::Syntax(error.to_string()))?;
        let shape_matches = match result_type {
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
        let samples = match result_type {
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
    let mut names = Vec::with_capacity(items.len());
    for (index, ((_, labels), base_value)) in items.iter().zip(&bases).enumerate() {
        let group: Vec<_> = items
            .iter()
            .enumerate()
            .filter(|(_, (other_base, _))| other_base.unwrap_or_default() == *base_value)
            .collect();
        let name = if !base_value.is_empty() && group.len() == 1 {
            (*base_value).to_string()
        } else if !base_value.is_empty() && labels.keys().all(|key| key.as_str() == "__name__") {
            (*base_value).to_string()
        } else if labels.is_empty() {
            format!("{fallback_prefix}_{index}")
        } else {
            let mut keys: Vec<_> = labels
                .keys()
                .filter(|key| base_value.is_empty() || key.as_str() != "__name__")
                .collect();
            keys.sort_unstable();
            let distinguishing: Vec<_> = keys
                .into_iter()
                .filter(|key| {
                    let mut values = HashSet::new();
                    group.iter().for_each(|(_, (_, other_labels))| {
                        values.insert(other_labels.get(*key));
                    });
                    values.len() > 1
                })
                .collect();
            let keys = if distinguishing.is_empty() {
                if !base_value.is_empty() {
                    Vec::new()
                } else {
                    let mut keys: Vec<_> = labels.keys().collect();
                    keys.sort_unstable();
                    keys
                }
            } else {
                distinguishing
            };
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
        assert_eq!(
            parse_prometheus_response(json).unwrap()[0].fields[1].name,
            frames[0].fields[1].name
        );
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
