use gtui_core::datasource::QueryError;
use gtui_core::frame::{Field, FieldType, Frame};
use serde::Deserialize;

#[derive(Deserialize, Debug)]
struct PromResponse {
    status: String,
    data: Option<PromData>,
    error: Option<String>,
}

#[allow(dead_code)]
#[derive(Deserialize, Debug)]
struct PromData {
    #[serde(rename = "resultType")]
    result_type: String,
    result: Vec<PromResult>,
}

#[derive(Deserialize, Debug)]
struct PromResult {
    metric: std::collections::HashMap<String, String>,
    values: Option<Vec<PromValueTuple>>, // For range queries
    value: Option<PromValueTuple>,       // For instant queries
}

#[derive(Deserialize, Debug)]
struct PromValueTuple(f64, String);

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

    let mut frames = Vec::new();

    for (i, result) in data.result.into_iter().enumerate() {
        let mut times = Vec::new();
        let mut values = Vec::new();

        if let Some(vals) = result.values {
            for v in vals {
                times.push(v.0);
                if let Ok(fv) = v.1.parse::<f64>() {
                    values.push(fv);
                } else {
                    values.push(f64::NAN);
                }
            }
        } else if let Some(v) = result.value {
            times.push(v.0);
            if let Ok(fv) = v.1.parse::<f64>() {
                values.push(fv);
            } else {
                values.push(f64::NAN);
            }
        }

        let time_field = Field::new("time", FieldType::Time, times);

        let series_name = result
            .metric
            .get("__name__")
            .cloned()
            .unwrap_or_else(|| format!("series_{}", i));
        let val_field = Field::new(&series_name, FieldType::Float64, values);

        frames.push(Frame::new(vec![time_field, val_field]));
    }

    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_prometheus_instant() {
        let json = r#"{
            "status": "success",
            "data": {
                "resultType": "vector",
                "result": [
                    {
                        "metric": {
                            "__name__": "up",
                            "job": "prometheus",
                            "instance": "localhost:9090"
                        },
                        "value": [1435781451.781, "1"]
                    }
                ]
            }
        }"#;

        let frames = parse_prometheus_response(json).unwrap();
        assert_eq!(frames.len(), 1);
        let frame = &frames[0];
        assert_eq!(frame.fields.len(), 2);

        // Check fields
        assert_eq!(frame.fields[0].name, "time");
        assert_eq!(frame.fields[1].name, "up");
    }

    #[test]
    fn test_parse_prometheus_range() {
        let json = r#"{
            "status": "success",
            "data": {
                "resultType": "matrix",
                "result": [
                    {
                        "metric": {
                            "__name__": "up"
                        },
                        "values": [
                            [1435781430.781, "1"],
                            [1435781445.781, "1"],
                            [1435781460.781, "1"]
                        ]
                    }
                ]
            }
        }"#;

        let frames = parse_prometheus_response(json).unwrap();
        assert_eq!(frames.len(), 1);
        let frame = &frames[0];
        assert_eq!(frame.fields.len(), 2);
    }
}
