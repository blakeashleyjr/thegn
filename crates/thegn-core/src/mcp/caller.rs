//! The MCP caller label (`THEGN_CALLER`, THE-2172).
//!
//! Audit-only: it is self-asserted environment, logged beside every mutating
//! MCP tool call, and never used for authorization.
//!
//! Stub created by the b152 spine (lane 1); filled by b152 lane 6, which
//! sanitizes the value (charset `[A-Za-z0-9._:/@-]`, at most 64 bytes,
//! otherwise `invalid`).

/// The caller label from the environment, if any. Stub: always `None`.
pub fn caller_from_env() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::caller_from_env;

    #[test]
    fn stub_reports_no_caller() {
        assert_eq!(caller_from_env(), None);
    }
}
