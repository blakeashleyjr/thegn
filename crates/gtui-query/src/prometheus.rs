use gtui_core::datasource::{DataSource, Query, QueryError};
use gtui_core::frame::{Field, FieldType, Frame};
use std::future::Future;
use std::pin::Pin;

pub struct PrometheusSource;

impl PrometheusSource {
    pub fn new() -> Self {
        Self
    }
}

impl Default for PrometheusSource {
    fn default() -> Self {
        Self::new()
    }
}

impl DataSource for PrometheusSource {
    fn query(
        &self,
        queries: Vec<Query>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Frame>, QueryError>> + Send>> {
        let res: Vec<Frame> = queries
            .into_iter()
            .map(|_q| {
                // Return an empty frame for now (MVP stub)
                let field = Field::new("value", FieldType::Float64, vec![]);
                Frame::new(vec![field])
            })
            .collect();
        Box::pin(async move { Ok(res) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use gtui_core::datasource::{Query, TimeRange};

    #[tokio::test]
    async fn test_prometheus_query() {
        let source = PrometheusSource::new();
        let q = Query {
            ref_id: "A".to_string(),
            expr: "up".to_string(),
            time_range: TimeRange {
                from: Utc::now(),
                to: Utc::now(),
            },
        };
        let res = source.query(vec![q]).await.unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].fields.len(), 1);
    }
}
