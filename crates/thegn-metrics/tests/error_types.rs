//! `CollectError` implements `std::error::Error + Send + Sync + 'static`
//! (Rust API Guidelines C-GOOD-ERR) and renders a readable message.

use std::error::Error;

use thegn_metrics::collect::CollectError;

fn assert_error<E: Error + Send + Sync + 'static>() {}

#[test]
fn collect_error_implements_std_error() {
    assert_error::<CollectError>();
}

#[test]
fn collect_error_messages_are_descriptive() {
    assert_eq!(CollectError::Gone.to_string(), "metrics source is gone");
    assert_eq!(
        CollectError::Parse("bad stat line").to_string(),
        "metrics source parse error: bad stat line"
    );
}
