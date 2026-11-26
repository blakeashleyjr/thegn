//! Public error types stay usable through `std::error::Error` (Rust API
//! Guidelines C-GOOD-ERR): each implements `Error + Send + Sync + 'static` so
//! callers can propagate it with `?` into boxed or `anyhow` errors.

use std::error::Error;

fn assert_error<E: Error + Send + Sync + 'static>() {}

#[test]
fn public_error_types_implement_std_error() {
    assert_error::<thegn_core::pipeline_chunk::MatchError>();
    assert_error::<thegn_core::fsperm::SecretFileError>();
    assert_error::<thegn_core::worktree::AddError>();
    assert_error::<thegn_core::proxy::attribution::AttrError>();
    assert_error::<thegn_core::mcp::protocol::JsonRpcError>();
    assert_error::<thegn_core::config_repo::RepoOverlayParseError>();
}

#[test]
fn attr_and_jsonrpc_errors_render_messages() {
    use thegn_core::mcp::protocol::JsonRpcError;
    use thegn_core::proxy::attribution::AttrError;

    assert_eq!(
        AttrError::Expired.to_string(),
        "attribution token has expired"
    );
    let rpc = JsonRpcError {
        code: -32600,
        message: "bad request".to_string(),
        data: None,
    };
    assert_eq!(rpc.to_string(), "json-rpc error -32600: bad request");
}
