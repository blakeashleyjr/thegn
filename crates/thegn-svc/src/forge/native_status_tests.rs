use super::regression_tests::{client, runtime};
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

    let bare_token = format!("Bad credentials ghp_{}", "a".repeat(36));
    let bare_token_message = bounded_message(Some(&bare_token));
    assert!(!bare_token_message.contains("ghp_"));
    assert!(bare_token_message.contains("***redacted***"));

    let userinfo = format!("see https://x:ghp_{}@github.com", "b".repeat(36));
    let userinfo_message = bounded_message(Some(&userinfo));
    assert!(!userinfo_message.contains("ghp_"));
    assert!(userinfo_message.contains("***redacted***"));

    let with_bidi = bounded_message(Some("before\u{202E}after"));
    assert!(!with_bidi.contains('\u{202E}'));
    assert!(with_bidi.contains("before after"));

    let long_message = "x".repeat(500);
    let bounded = bounded_message(Some(&long_message));
    assert!(bounded.chars().count() <= 203);
    assert!(bounded.ends_with("..."));

    let multibyte = "é".repeat(500);
    let bounded_multibyte = bounded_message(Some(&multibyte));
    assert_eq!(bounded_multibyte, format!("{}...", "é".repeat(200)));
    assert!(bounded_multibyte.is_char_boundary(bounded_multibyte.len()));
    assert_eq!(bounded_message(None), "");
}

#[test]
fn graphql_status_other_error_redacts_bare_provider_tokens() {
    let token_body = format!("{{\"message\":\"Bad credentials ghp_{}\"}}", "c".repeat(36));
    let rt = runtime();
    rt.block_on(async {
        let body = serde_json::json!({"query": "fixture"});
        let health = GhCircuit::new();
        let error = graphql_request(&client(502, &token_body), &body, "fixture", &health).await;
        let Err(ForgeError::Other(message)) = error else {
            panic!("expected an HTTP status error");
        };
        assert!(!message.contains("ghp_"));
    });
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
