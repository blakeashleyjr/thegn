use chrono::Utc;
use gtui_core::datasource::{Query, TimeRange};

pub struct ExploreMode {
    pub current_query: String,
    pub history: Vec<String>,
}

impl ExploreMode {
    pub fn new() -> Self {
        Self {
            current_query: String::new(),
            history: Vec::new(),
        }
    }

    pub fn submit_query(&mut self) -> Query {
        if !self.current_query.is_empty() {
            self.history.push(self.current_query.clone());
        }

        // Auto-detect viz would happen in the render layer based on returned frame types.
        // For MVP, we just emit a Query struct.
        Query {
            ref_id: "A".to_string(),
            expr: self.current_query.clone(),
            time_range: TimeRange {
                from: Utc::now(),
                to: Utc::now(),
            },
        }
    }
}

impl Default for ExploreMode {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_explore_history() {
        let mut mode = ExploreMode::new();
        mode.current_query = "up".to_string();

        let q = mode.submit_query();
        assert_eq!(q.expr, "up");
        assert_eq!(mode.history.len(), 1);
        assert_eq!(mode.history[0], "up");
    }
}
