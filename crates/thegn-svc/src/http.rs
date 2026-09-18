//! Safe, pooled HTTP transport for remote calendar accounts.
//!
//! Calendar transport deliberately has a smaller security surface than the
//! general-purpose reqwest clients used by other services: it never follows
//! redirects, never uses ambient proxies or cookies, never enables automatic
//! decompression, and resolves DNS through the same checked resolver that the
//! connector consumes.  The two process-wide clients are only split by the
//! trusted private-network permission; credentials and validators stay on each
//! request, so pooling cannot cross-contaminate accounts.

use futures_util::StreamExt;
use reqwest::{Client, RequestBuilder, Response, Url};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::time::{Instant, timeout_at};

pub(crate) const MAX_BODY_BYTES: usize = 32 << 20;
pub(crate) const MAX_ERROR_BODY_BYTES: usize = 8 << 10;
pub(crate) const MAX_REQUEST_BYTES: usize = 64 << 10;
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CalendarHttpError {
    InvalidUrl,
    DestinationRefused,
    ClientConfiguration,
    Network,
    Timeout,
    Redirect,
    Encoding,
    BodyLimit,
    MediaType,
}

/// Shared bounded response reader used by calendar and tracker adapters. It
/// owns no protocol-specific error strings, so callers can preserve their
/// existing seam error contracts without reimplementing the allocation guard.
#[derive(Debug)]
pub(crate) enum BodyReadError {
    Network(reqwest::Error),
    Timeout,
    Limit,
}

/// The body types accepted by the two remote calendar protocols.  These
/// compatibility variants are documented because several CalDAV servers use
/// `text/xml` or `application/dav+xml`, while subscribed feeds commonly use
/// `application/ics` or `text/plain`.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ExpectedMedia {
    Ics,
    Dav,
}

#[derive(Clone)]
pub(crate) struct CalendarHttpClient {
    client: Client,
    url: Url,
}

impl CalendarHttpClient {
    pub(crate) fn new(
        raw_url: &str,
        allow_private_network: bool,
    ) -> Result<Self, CalendarHttpError> {
        let url = Url::parse(raw_url).map_err(|_| CalendarHttpError::InvalidUrl)?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(CalendarHttpError::InvalidUrl);
        }
        if let Some(host) = url.host_str()
            && let Ok(ip) = host.parse::<IpAddr>()
            && !address_allowed(ip, allow_private_network)
        {
            return Err(CalendarHttpError::DestinationRefused);
        }
        let client = process_client(allow_private_network)?;
        Ok(Self { client, url })
    }

    pub(crate) fn request(&self, method: reqwest::Method) -> RequestBuilder {
        self.client.request(method, self.url.clone())
    }

    pub(crate) async fn send(
        &self,
        request: RequestBuilder,
        deadline: Instant,
    ) -> Result<Response, CalendarHttpError> {
        timeout_at(deadline, request.send())
            .await
            .map_err(|_| CalendarHttpError::Timeout)?
            .map_err(|_| CalendarHttpError::Network)
    }
}

fn process_client(allow_private_network: bool) -> Result<Client, CalendarHttpError> {
    static PUBLIC: OnceLock<Result<Client, CalendarHttpError>> = OnceLock::new();
    static PRIVATE: OnceLock<Result<Client, CalendarHttpError>> = OnceLock::new();
    let slot = if allow_private_network {
        &PRIVATE
    } else {
        &PUBLIC
    };
    slot.get_or_init(|| build_client(allow_private_network))
        .clone()
}

fn build_client(allow_private_network: bool) -> Result<Client, CalendarHttpError> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        // Compressed bodies are rejected by validate_encoding before reading.
        // Disabling decompression makes the 32 MiB limit a wire/body limit and
        // prevents a small encoded response from becoming an allocation bomb.
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_IDLE_TIMEOUT)
        .dns_resolver2(CalendarResolver {
            allow_private_network,
        })
        .build()
        .map_err(|_| CalendarHttpError::ClientConfiguration)
}

#[derive(Clone)]
struct CalendarResolver {
    allow_private_network: bool,
}

impl reqwest::dns::Resolve for CalendarResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        let allow_private_network = self.allow_private_network;
        Box::pin(async move {
            let resolved = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|_| resolver_error())?;
            let addresses: Vec<SocketAddr> = resolved.collect();
            if !addresses_allowed(&addresses, allow_private_network) {
                return Err(resolver_error());
            }
            Ok(Box::new(addresses.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

fn resolver_error() -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "calendar destination refused",
    ))
}

/// Public-calendar mode admits only globally routable unicast addresses.
/// Private-calendar mode admits intentional local unicast destinations but
/// still rejects unspecified, multicast, and broadcast addresses.
fn address_allowed(ip: IpAddr, allow_private_network: bool) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => match v6.to_ipv4() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        other => other,
    };
    if allow_private_network {
        return match ip {
            IpAddr::V4(v4) => !v4.is_unspecified() && !v4.is_multicast() && !v4.is_broadcast(),
            IpAddr::V6(v6) => !v6.is_unspecified() && !v6.is_multicast(),
        };
    }
    match ip {
        IpAddr::V4(v4) => public_ipv4(v4),
        IpAddr::V6(v6) => public_ipv6(v6),
    }
}

fn addresses_allowed(addresses: &[SocketAddr], allow_private_network: bool) -> bool {
    !addresses.is_empty()
        && addresses
            .iter()
            .all(|address| address_allowed(address.ip(), allow_private_network))
}

fn public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    !ip.is_private()
        && !ip.is_loopback()
        && !ip.is_link_local()
        && !ip.is_unspecified()
        && !ip.is_multicast()
        && !ip.is_broadcast()
        && a != 0
        && !(a == 100 && (64..=127).contains(&b)) // shared address space
        && !(a == 192 && b == 0) // IETF protocol assignments and documentation
        && !(a == 198 && (18..=19).contains(&b)) // benchmarking
        && !(a == 198 && b == 51) // documentation block
        && !(a == 203 && b == 0) // documentation block
        && a < 240
}

fn public_ipv6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    !ip.is_loopback()
        && !ip.is_unspecified()
        && !ip.is_multicast()
        && (segments[0] & 0xfe00) != 0xfc00 // unique local
        && (segments[0] & 0xffc0) != 0xfe80 // link local
        && ip != Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 0) // documentation prefix
        && !(segments[0] == 0x2001 && segments[1] == 0x0db8)
}

pub(crate) fn validate_encoding(response: &Response) -> Result<(), CalendarHttpError> {
    if response
        .headers()
        .get(reqwest::header::CONTENT_ENCODING)
        .is_some_and(|value| !value.as_bytes().eq_ignore_ascii_case(b"identity"))
    {
        return Err(CalendarHttpError::Encoding);
    }
    Ok(())
}

pub(crate) fn validate_media(
    response: &Response,
    expected: ExpectedMedia,
) -> Result<(), CalendarHttpError> {
    let value = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .ok_or(CalendarHttpError::MediaType)?
        .to_str()
        .map_err(|_| CalendarHttpError::MediaType)?;
    let media = value.split(';').next().unwrap_or("").trim();
    let accepted = match expected {
        ExpectedMedia::Ics => matches!(
            media,
            "text/calendar" | "application/calendar" | "application/ics" | "text/plain"
        ),
        ExpectedMedia::Dav => {
            matches!(
                media,
                "application/xml" | "text/xml" | "application/dav+xml"
            )
        }
    };
    accepted.then_some(()).ok_or(CalendarHttpError::MediaType)
}

pub(crate) async fn read_body(
    response: Response,
    deadline: Instant,
) -> Result<Vec<u8>, CalendarHttpError> {
    read_bounded_response(response, deadline, MAX_BODY_BYTES)
        .await
        .map_err(|error| match error {
            BodyReadError::Network(_) => CalendarHttpError::Network,
            BodyReadError::Timeout => CalendarHttpError::Timeout,
            BodyReadError::Limit => CalendarHttpError::BodyLimit,
        })
}

pub(crate) async fn read_bounded_response(
    response: Response,
    deadline: Instant,
    limit: usize,
) -> Result<Vec<u8>, BodyReadError> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(BodyReadError::Limit);
    }
    let capacity = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or(0)
        .min(limit);
    let mut body = Vec::with_capacity(capacity);
    let mut stream = response.bytes_stream();
    while let Some(chunk) = timeout_at(deadline, stream.next())
        .await
        .map_err(|_| BodyReadError::Timeout)?
    {
        let chunk = chunk.map_err(BodyReadError::Network)?;
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(BodyReadError::Limit);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Drain an error body without retaining it.  This bounds both diagnostic
/// work and memory while ensuring never-ending error responses still obey the
/// same idle/total deadlines as successful responses.
pub(crate) async fn discard_body(
    response: Response,
    deadline: Instant,
) -> Result<(), CalendarHttpError> {
    let mut count = 0usize;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = timeout_at(deadline, stream.next())
        .await
        .map_err(|_| CalendarHttpError::Timeout)?
    {
        let chunk = chunk.map_err(|_| CalendarHttpError::Network)?;
        count = count.saturating_add(chunk.len());
        if count > MAX_ERROR_BODY_BYTES {
            return Err(CalendarHttpError::BodyLimit);
        }
    }
    Ok(())
}

pub(crate) fn map_error(error: CalendarHttpError) -> &'static str {
    match error {
        CalendarHttpError::InvalidUrl => "calendar URL is invalid",
        CalendarHttpError::DestinationRefused => "calendar destination refused",
        CalendarHttpError::ClientConfiguration => "calendar HTTP client unavailable",
        CalendarHttpError::Network => "calendar request failed",
        CalendarHttpError::Timeout => "calendar request timed out",
        CalendarHttpError::Redirect => "calendar redirect refused",
        CalendarHttpError::Encoding => "calendar response encoding refused",
        CalendarHttpError::BodyLimit => "calendar response exceeds limit",
        CalendarHttpError::MediaType => "calendar response content type refused",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destination_policy_covers_literal_special_and_mapped_addresses() {
        for raw in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.1.1",
            "224.0.0.1",
            "0.0.0.0",
            "::1",
            "fc00::1",
            "fe80::1",
            "ff02::1",
            "::ffff:127.0.0.1",
        ] {
            let ip = raw.parse().unwrap();
            assert!(!address_allowed(ip, false), "{raw} must be refused");
            assert!(address_allowed(ip, true) || raw == "0.0.0.0" || raw == "::1");
        }
        assert!(address_allowed(
            "2001:4860:4860::8888".parse().unwrap(),
            false
        ));
        assert!(address_allowed("8.8.8.8".parse().unwrap(), false));
        let mixed = [
            SocketAddr::from(([8, 8, 8, 8], 443)),
            SocketAddr::from(([10, 0, 0, 1], 443)),
        ];
        assert!(!addresses_allowed(&mixed, false));
        assert!(addresses_allowed(&mixed, true));
    }

    #[test]
    fn invalid_calendar_urls_never_construct_a_transport() {
        for raw in [
            "ftp://calendar.example/events.ics",
            "https://user:pass@calendar.example/events.ics",
            "https://127.0.0.1/events.ics",
        ] {
            assert!(CalendarHttpClient::new(raw, false).is_err(), "{raw}");
        }
    }
}
