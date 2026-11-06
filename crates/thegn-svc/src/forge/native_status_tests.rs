use super::*;

#[test]
fn status_class_separates_401_403_and_rate_limits() {
    for (code, rate_limited, expected) in [
        (401, false, "unauthorized"),
        (403, false, "forbidden"),
        (403, true, "rate_limited"),
        (429, false, "rate_limited"),
        (502, false, "server"),
        (404, false, "http"),
    ] {
        assert_eq!(status_class(code, rate_limited), expected);
    }
}

#[test]
fn bounded_message_redacts_truncates_and_flattens() {
    let message = bounded_message(Some("Authorization: Bearer abc\nsecond line"));
    assert!(!message.contains('\n'));
    assert!(message.contains("***redacted***"));
    assert!(!message.contains("Bearer abc"));

    let long_message = "x".repeat(500);
    let bounded = bounded_message(Some(&long_message));
    assert!(bounded.chars().count() <= 203);
    assert!(bounded.ends_with("..."));
    assert_eq!(bounded_message(None), "");
}

#[test]
fn request_repo_reads_owner_and_repo_variables() {
    let body = serde_json::json!({
        "query": "q",
        "variables": {"owner": "o", "repo": "r"}
    });
    assert_eq!(request_repo(&body), "o/r");

    let body_with_name = serde_json::json!({"variables": {"owner": "o", "name": "r"}});
    assert_eq!(request_repo(&body_with_name), "o/r");
    assert_eq!(request_repo(&serde_json::json!({"query": "q"})), "-");
}
