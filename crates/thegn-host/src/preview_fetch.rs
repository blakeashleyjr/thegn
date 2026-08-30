//! Bounded, credential-free HTTP executor for `preview.fetch`.
//!
//! This is deliberately a fresh blocking client per call: it owns no cookie
//! jar or browser profile, inherits no proxy configuration, follows redirects
//! manually so every target crosses the core URL-policy chokepoint, and is run
//! by the daemon through `spawn_blocking`.

use std::io::Read as _;
use std::time::{Duration, Instant};

use thegn_core::config::PreviewConfig;
use thegn_core::preview::{PreviewUrlError, validate_preview_redirect, validate_preview_url};
use thegn_svc::control::{PreviewFetchReply, PreviewFetchRequest};

const MAX_URL_BYTES: usize = 8 * 1024;
const MAX_WORKTREE_BYTES: usize = 16 * 1024;
const MAX_CONTENT_TYPE_BYTES: usize = 512;
const MAX_REDIRECTS: usize = 5;
const MAX_CONSOLE_ERRORS: usize = 32;
const MAX_CONSOLE_LINE_BYTES: usize = 512;

#[derive(Debug)]
pub(crate) enum FetchError {
    Invalid(String),
    Precondition(String),
    Limit(String),
    Timeout,
    Transport(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) | Self::Precondition(message) | Self::Limit(message) => {
                f.write_str(message)
            }
            Self::Timeout => f.write_str("preview fetch timed out"),
            Self::Transport(message) => write!(f, "preview fetch transport: {message}"),
        }
    }
}

impl std::error::Error for FetchError {}

fn policy_error(error: PreviewUrlError) -> FetchError {
    match error {
        PreviewUrlError::ExternalForbidden => FetchError::Precondition(error.to_string()),
        PreviewUrlError::Invalid
        | PreviewUrlError::UnsupportedScheme
        | PreviewUrlError::CredentialsForbidden => FetchError::Invalid(error.to_string()),
    }
}

fn validate_request(
    config: &PreviewConfig,
    request: &PreviewFetchRequest,
) -> Result<(), FetchError> {
    if !config.enabled {
        return Err(FetchError::Precondition(
            "preview support is disabled by configuration".into(),
        ));
    }
    if request.url.len() > MAX_URL_BYTES {
        return Err(FetchError::Limit(format!(
            "preview URL exceeds {MAX_URL_BYTES} bytes"
        )));
    }
    if request
        .worktree
        .as_ref()
        .is_some_and(|worktree| worktree.is_empty() || worktree.len() > MAX_WORKTREE_BYTES)
    {
        return Err(FetchError::Invalid(format!(
            "worktree must be 1..={MAX_WORKTREE_BYTES} bytes when present"
        )));
    }
    validate_preview_url(&request.url, config.allow_external_urls)
        .map_err(policy_error)
        .map(|_| ())
}

/// Execute one GET. `diagnostic` is the bounded snapshot captured before this
/// blocking function runs; it is never allowed to grow the reply unboundedly.
pub(crate) fn fetch(
    config: &PreviewConfig,
    request: PreviewFetchRequest,
    diagnostic: Option<String>,
) -> Result<PreviewFetchReply, FetchError> {
    validate_request(config, &request)?;
    let timeout = Duration::from_millis(config.fetch_timeout_ms);
    let started = Instant::now();
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(timeout)
        .build()
        .map_err(|error| FetchError::Transport(error.to_string()))?;

    let mut current = reqwest::Url::parse(&request.url)
        .map_err(|error| FetchError::Invalid(format!("invalid preview URL: {error}")))?;
    let mut redirects = 0usize;
    loop {
        let remaining = timeout
            .checked_sub(started.elapsed())
            .ok_or(FetchError::Timeout)?;
        let mut response = client
            .get(current.clone())
            .timeout(remaining)
            .send()
            .map_err(request_error)?;

        if response.status().is_redirection()
            && let Some(location) = response.headers().get(reqwest::header::LOCATION)
        {
            if redirects >= MAX_REDIRECTS {
                return Err(FetchError::Limit(format!(
                    "preview redirect limit exceeded ({MAX_REDIRECTS})"
                )));
            }
            let location = location
                .to_str()
                .map_err(|_| FetchError::Invalid("redirect Location is not text".into()))?;
            let next = current.join(location).map_err(|error| {
                FetchError::Invalid(format!("invalid preview redirect: {error}"))
            })?;
            if next.as_str().len() > MAX_URL_BYTES {
                return Err(FetchError::Limit(format!(
                    "preview redirect URL exceeds {MAX_URL_BYTES} bytes"
                )));
            }
            validate_preview_redirect(next.as_str(), config.allow_external_urls)
                .map_err(policy_error)?;
            current = next;
            redirects += 1;
            continue;
        }

        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| bounded_text(value, MAX_CONTENT_TYPE_BYTES));
        let mut bytes = Vec::with_capacity(config.max_body_bytes.min(64 * 1024) + 1);
        response
            .by_ref()
            .take(config.max_body_bytes as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| FetchError::Transport(error.to_string()))?;
        let truncated = bytes.len() > config.max_body_bytes;
        bytes.truncate(config.max_body_bytes);
        let (console_errors, diagnostics_source) = diagnostics(request.include_console, diagnostic);
        return Ok(PreviewFetchReply {
            url: current.to_string(),
            status,
            content_type,
            body: String::from_utf8_lossy(&bytes).into_owned(),
            truncated,
            console_errors,
            diagnostics_source,
        });
    }
}

fn request_error(error: reqwest::Error) -> FetchError {
    if error.is_timeout() {
        FetchError::Timeout
    } else {
        FetchError::Transport(error.to_string())
    }
}

fn diagnostics(include: bool, diagnostic: Option<String>) -> (Vec<String>, String) {
    if !include {
        return (Vec::new(), "unavailable".into());
    }
    let Some(diagnostic) = diagnostic else {
        return (Vec::new(), "unavailable".into());
    };
    let plain = thegn_core::history::AnsiStripper::strip_str(&diagnostic);
    let errors = plain
        .lines()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("error") || lower.contains("failed") || lower.contains("exception")
        })
        .take(MAX_CONSOLE_ERRORS)
        .map(redact_line)
        .collect();
    (errors, "dev-server-pane".into())
}

fn redact_line(line: &str) -> String {
    let mut tokens = vec!["diagnostic".to_string()];
    tokens.extend(line.split_whitespace().map(str::to_string));
    let redacted = thegn_core::log_redact::redact_argv(&tokens);
    bounded_text(&redacted[1..].join(" "), MAX_CONSOLE_LINE_BYTES)
}

fn bounded_text(value: &str, max_bytes: usize) -> String {
    let mut end = value.len().min(max_bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    fn serve(count: usize, handler: impl Fn(usize, &str) -> String + Send + 'static) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            for (index, incoming) in listener.incoming().take(count).enumerate() {
                let mut stream = incoming.unwrap();
                let request = read_request(&mut stream);
                stream
                    .write_all(handler(index, &request).as_bytes())
                    .unwrap();
            }
        });
        format!("http://{addr}/")
    }

    fn read_request(stream: &mut TcpStream) -> String {
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut byte = [0u8; 1];
        while stream.read_exact(&mut byte).is_ok() {
            bytes.push(byte[0]);
            if bytes.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn request(url: String) -> PreviewFetchRequest {
        PreviewFetchRequest {
            url,
            worktree: None,
            include_console: false,
        }
    }

    #[test]
    fn loopback_success_preserves_status_and_bounds_body() {
        let url = serve(1, |_, _| {
            "HTTP/1.1 201 Created\r\nContent-Type: text/plain\r\nContent-Length: 6\r\n\r\nabcdef"
                .into()
        });
        let config = PreviewConfig {
            max_body_bytes: 3,
            ..PreviewConfig::default()
        };
        let reply = fetch(&config, request(url), None).unwrap();
        assert_eq!(reply.status, 201);
        assert_eq!(reply.content_type.as_deref(), Some("text/plain"));
        assert_eq!(reply.body, "abc");
        assert!(reply.truncated);
    }

    #[test]
    fn rejects_external_and_redirect_escape_before_connecting() {
        let error = fetch(
            &PreviewConfig::default(),
            request("https://example.com/".into()),
            None,
        )
        .unwrap_err();
        assert!(matches!(error, FetchError::Precondition(_)));

        let url = serve(1, |_, _| {
            "HTTP/1.1 302 Found\r\nLocation: https://example.com/escape\r\nContent-Length: 0\r\n\r\n"
                .into()
        });
        let error = fetch(&PreviewConfig::default(), request(url), None).unwrap_err();
        assert!(matches!(error, FetchError::Precondition(_)));

        let allow = PreviewConfig {
            allow_external_urls: true,
            ..PreviewConfig::default()
        };
        assert!(validate_request(&allow, &request("https://example.com/".into())).is_ok());
    }

    #[test]
    fn times_out_and_does_not_replay_cookies() {
        let url = serve(2, |index, request| {
            if index == 0 {
                "HTTP/1.1 302 Found\r\nSet-Cookie: secret=leak\r\nLocation: /next\r\nContent-Length: 0\r\n\r\n".into()
            } else {
                assert!(!request.to_ascii_lowercase().contains("cookie:"));
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".into()
            }
        });
        assert_eq!(
            fetch(&PreviewConfig::default(), request(url), None)
                .unwrap()
                .body,
            "ok"
        );

        let url = serve(1, |_, _| {
            thread::sleep(Duration::from_millis(250));
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".into()
        });
        let config = PreviewConfig {
            fetch_timeout_ms: 100,
            ..PreviewConfig::default()
        };
        assert!(matches!(
            fetch(&config, request(url), None),
            Err(FetchError::Timeout)
        ));
    }

    #[test]
    fn diagnostics_are_source_labelled_bounded_and_redacted() {
        let url = serve(1, |_, _| {
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".into()
        });
        let mut req = request(url);
        req.include_console = true;
        let reply = fetch(
            &PreviewConfig::default(),
            req,
            Some("info ready\nERROR API_TOKEN=sekrit failed\n".into()),
        )
        .unwrap();
        assert_eq!(reply.diagnostics_source, "dev-server-pane");
        assert_eq!(reply.console_errors.len(), 1);
        assert!(reply.console_errors[0].contains(thegn_core::log_redact::REDACTED));
        assert!(!reply.console_errors[0].contains("sekrit"));
    }
}
