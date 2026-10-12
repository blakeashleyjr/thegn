use gtui_core::datasource::QueryError;

pub(crate) const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

pub(crate) async fn read_limited(
    mut response: reqwest::Response,
    label: &str,
) -> Result<String, QueryError> {
    if response
        .content_length()
        .is_some_and(|n| n > MAX_RESPONSE_BYTES as u64)
    {
        return Err(QueryError::Other(format!(
            "{label} response exceeds 4 MiB limit"
        )));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| QueryError::Network(e.to_string()))?
    {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(QueryError::Other(format!(
                "{label} response exceeds 4 MiB limit"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body).map_err(|e| QueryError::Syntax(e.to_string()))
}
