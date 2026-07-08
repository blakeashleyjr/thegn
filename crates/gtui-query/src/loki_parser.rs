use gtui_core::datasource::QueryError;
use gtui_core::frame::{Field, FieldType, Frame};
use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub struct LokiResponse {
    pub status: String,
    pub data: Option<LokiData>,
    pub error: Option<String>,
}

#[allow(dead_code)]
#[derive(Deserialize, Debug)]
pub struct LokiData {
    #[serde(rename = "resultType")]
    pub result_type: String,
    pub result: Vec<LokiResult>,
}

#[derive(Deserialize, Debug)]
pub struct LokiResult {
    pub stream: std::collections::HashMap<String, String>,
    pub values: Vec<LokiValueTuple>,
}

#[derive(Deserialize, Debug)]
pub struct LokiValueTuple(String, String);

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

    let mut frames = Vec::new();

    for (i, result) in data.result.into_iter().enumerate() {
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

        let series_name = result
            .stream
            .get("filename")
            .cloned()
            .unwrap_or_else(|| format!("stream_{}", i));

        // The log lines themselves, as a real string series.
        let val_field = Field::new_str(&series_name, values);

        frames.push(Frame::new(vec![time_field, val_field]));
    }

    Ok(frames)
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
}
