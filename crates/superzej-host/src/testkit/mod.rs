//! Test-result ingestion matchers, grouped by mode.
//!
//! - `json`  — structured event streams (libtest/nextest, dart/flutter).
//! - text scraping stays in `panel::parse_test_output` (the fragile baseline).
//! - report-file parsing (JUnit XML / TRX) lands in a later phase.
//!
//! The dispatcher in `task::parse_task_outcome` selects a parser from the task's
//! `Ingestion` mode so adding a structured runner never touches the text path.

pub mod json;
pub mod report;
