use gtui_core::datasource::{DataSource, Query, QueryError};
use gtui_core::frame::{Field, FieldType, Frame};
use std::future::Future;
use std::pin::Pin;

pub struct SyntheticSource;

impl SyntheticSource {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SyntheticSource {
    fn default() -> Self {
        Self::new()
    }
}

impl DataSource for SyntheticSource {
    fn query(
        &self,
        queries: Vec<Query>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Frame>, QueryError>> + Send>> {
        let res: Vec<Frame> = queries
            .into_iter()
            .map(|_q| {
                let field = Field::new("val", FieldType::Float64, vec![1.0, 1.5, 2.0]);
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
    async fn test_synthetic_query() {
        let source = SyntheticSource::new();
        let q = Query {
            ref_id: "A".to_string(),
            expr: "random_walk".to_string(),
            time_range: TimeRange {
                from: Utc::now(),
                to: Utc::now(),
            },
        };
        let res = source.query(vec![q]).await.unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].fields.len(), 1);
        assert_eq!(res[0].fields[0].name, "val");
    }
}
