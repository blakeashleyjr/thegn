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
use url::Host;

pub(crate) const MAX_BODY_BYTES: usize = 32 << 20;
pub(crate) const MAX_ERROR_BODY_BYTES: usize = 8 << 10;
pub(crate) const MAX_REQUEST_BYTES: usize = 64 << 10;
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_DNS_ANSWERS: usize = 32;

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
/// `application/ics` or `text/plain`.  A missing or repeated Content-Type is
/// refused: accepting an unknown representation would make parser selection
/// ambiguous.
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
        match url.host() {
            Some(Host::Ipv4(ip)) if !address_allowed(IpAddr::V4(ip), allow_private_network) => {
                return Err(CalendarHttpError::DestinationRefused);
            }
            Some(Host::Ipv6(ip)) if !address_allowed(IpAddr::V6(ip), allow_private_network) => {
                return Err(CalendarHttpError::DestinationRefused);
            }
            _ => {}
        }
        let client = process_client(allow_private_network)?;
        Ok(Self { client, url })
    }

    pub(crate) fn request(&self, method: reqwest::Method) -> RequestBuilder {
        self.client
            .request(method, self.url.clone())
            // Calendar transport never accepts compressed responses.  Saying
            // so on the wire also prevents an intermediary from selecting an
            // encoding that the response validator will later reject.
            .header(reqwest::header::ACCEPT_ENCODING, "identity")
    }

    pub(crate) async fn send(
        &self,
        request: RequestBuilder,
        deadline: Instant,
    ) -> Result<Response, CalendarHttpError> {
        timeout_at(deadline, request.send())
            .await
            .map_err(|_| CalendarHttpError::Timeout)?
            .map_err(|error| {
                if error.is_timeout() {
                    CalendarHttpError::Timeout
                } else {
                    CalendarHttpError::Network
                }
            })
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
    build_client_with_resolver(CalendarResolver {
        allow_private_network,
        #[cfg(test)]
        test_answers: None,
        #[cfg(test)]
        test_resolutions: None,
    })
}

fn build_client_with_resolver(resolver: CalendarResolver) -> Result<Client, CalendarHttpError> {
    build_client_with_resolver_and_read_timeout(resolver, READ_IDLE_TIMEOUT)
}

fn build_client_with_resolver_and_read_timeout(
    resolver: CalendarResolver,
    read_timeout: Duration,
) -> Result<Client, CalendarHttpError> {
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
        .read_timeout(read_timeout)
        .dns_resolver2(resolver)
        .build()
        .map_err(|_| CalendarHttpError::ClientConfiguration)
}

#[derive(Clone)]
struct CalendarResolver {
    allow_private_network: bool,
    #[cfg(test)]
    test_answers: Option<std::sync::Arc<std::sync::Mutex<Vec<Vec<SocketAddr>>>>>,
    #[cfg(test)]
    test_resolutions: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
}

impl reqwest::dns::Resolve for CalendarResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        let allow_private_network = self.allow_private_network;
        #[cfg(test)]
        let test_answers = self.test_answers.clone();
        #[cfg(test)]
        let test_resolutions = self.test_resolutions.clone();
        Box::pin(async move {
            #[cfg(test)]
            if let Some(test_resolutions) = test_resolutions {
                test_resolutions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            #[cfg(test)]
            let addresses = if let Some(test_answers) = test_answers {
                test_answers
                    .lock()
                    .map_err(|_| resolver_error())?
                    .pop()
                    .ok_or_else(resolver_error)?
            } else {
                let resolved = tokio::net::lookup_host((host.as_str(), 0))
                    .await
                    .map_err(|_| resolver_error())?;
                collect_resolved_addresses(resolved).map_err(|_| resolver_error())?
            };
            #[cfg(not(test))]
            let addresses = {
                let resolved = tokio::net::lookup_host((host.as_str(), 0))
                    .await
                    .map_err(|_| resolver_error())?;
                collect_resolved_addresses(resolved).map_err(|_| resolver_error())?
            };
            if !addresses_allowed(&addresses, allow_private_network) {
                return Err(resolver_error());
            }
            Ok(Box::new(addresses.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

fn collect_resolved_addresses<I>(resolved: I) -> Result<Vec<SocketAddr>, ()>
where
    I: IntoIterator<Item = SocketAddr>,
{
    let mut addresses = Vec::with_capacity(MAX_DNS_ANSWERS);
    for address in resolved {
        if addresses.len() == MAX_DNS_ANSWERS {
            // Do not hand an unbounded resolver result to reqwest.  The OS
            // resolver may allocate internally; this cap is the boundary
            // owned by the calendar transport.
            return Err(());
        }
        addresses.push(address);
    }
    Ok(addresses)
}

fn resolver_error() -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "calendar destination refused",
    ))
}

/// Public-calendar mode admits only reviewed global-unicast ranges.  This is
/// deliberately an explicit policy rather than `IpAddr::is_global`: the
/// latter has changed semantics across Rust releases and is not a stable
/// security contract.  Private-calendar mode adds only the intentional local
/// ranges (RFC1918, CGNAT, loopback, link-local, and ULA); reserved,
/// documentation, transition, unspecified, multicast, and broadcast ranges
/// remain refused even with the opt-in.
fn address_allowed(ip: IpAddr, allow_private_network: bool) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => match mapped_ipv4(v6) {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        other => other,
    };
    match ip {
        IpAddr::V4(v4) => public_ipv4(v4) || (allow_private_network && local_ipv4(v4)),
        IpAddr::V6(v6) => public_ipv6(v6) || (allow_private_network && local_ipv6(v6)),
    }
}

fn mapped_ipv4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let segments = ip.segments();
    (segments[..5].iter().all(|segment| *segment == 0) && segments[5] == 0xffff)
        .then(|| ip.to_ipv4())
        .flatten()
}

fn addresses_allowed(addresses: &[SocketAddr], allow_private_network: bool) -> bool {
    !addresses.is_empty()
        && addresses
            .iter()
            .all(|address| address_allowed(address.ip(), allow_private_network))
}

fn public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, d] = ip.octets();
    a >= 1
        && a < 224
        && !in_ipv4_prefix(ip, [10, 0, 0, 0], 8)
        && !in_ipv4_prefix(ip, [100, 64, 0, 0], 10) // shared address space
        && !in_ipv4_prefix(ip, [127, 0, 0, 0], 8)
        && !in_ipv4_prefix(ip, [169, 254, 0, 0], 16)
        && !in_ipv4_prefix(ip, [172, 16, 0, 0], 12)
        && !in_ipv4_prefix(ip, [192, 0, 0, 0], 24)
        && !in_ipv4_prefix(ip, [192, 0, 2, 0], 24) // TEST-NET-1
        && !(a == 192 && b == 88 && c == 99) // 6to4 relay anycast
        && !in_ipv4_prefix(ip, [198, 18, 0, 0], 15) // benchmarking
        && !in_ipv4_prefix(ip, [198, 51, 100, 0], 24) // TEST-NET-2
        && !in_ipv4_prefix(ip, [203, 0, 113, 0], 24) // TEST-NET-3
        && !(a == 192 && b == 168)
        && !(a == 0)
        && !(a == 255 && b == 255 && c == 255 && d == 255)
}

fn public_ipv6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    // Global unicast is 2000::/3.  The exclusions below are the reviewed
    // special-purpose/transition blocks that sit inside that range.  The
    // conservative 2001::/23 exclusion covers IANA's entire enclosing
    // protocol-assignment range, including ORCHID (2001:10::/28) and
    // ORCHIDv2 (2001:20::/28), rather than relying on a partial subrange.
    (segments[0] & 0xe000) == 0x2000
        && !in_ipv6_prefix(ip, [0x2001, 0, 0, 0, 0, 0, 0, 0], 23)
        && !in_ipv6_prefix(ip, [0x2001, 0x0db8, 0, 0, 0, 0, 0, 0], 32) // documentation
        && !in_ipv6_prefix(ip, [0x2002, 0, 0, 0, 0, 0, 0, 0], 16) // 6to4
        && !in_ipv6_prefix(ip, [0x3fff, 0, 0, 0, 0, 0, 0, 0], 20) // documentation
        && !ip.is_unspecified()
        && !ip.is_loopback()
        && !ip.is_multicast()
}

fn local_ipv4(ip: Ipv4Addr) -> bool {
    in_ipv4_prefix(ip, [10, 0, 0, 0], 8)
        || in_ipv4_prefix(ip, [100, 64, 0, 0], 10)
        || in_ipv4_prefix(ip, [127, 0, 0, 0], 8)
        || in_ipv4_prefix(ip, [169, 254, 0, 0], 16)
        || in_ipv4_prefix(ip, [172, 16, 0, 0], 12)
        || in_ipv4_prefix(ip, [192, 168, 0, 0], 16)
}

fn local_ipv6(ip: Ipv6Addr) -> bool {
    ip == Ipv6Addr::LOCALHOST
        || in_ipv6_prefix(ip, [0xfc00, 0, 0, 0, 0, 0, 0, 0], 7)
        || in_ipv6_prefix(ip, [0xfe80, 0, 0, 0, 0, 0, 0, 0], 10)
}

fn in_ipv4_prefix(ip: Ipv4Addr, prefix: [u8; 4], bits: u8) -> bool {
    let ip = u32::from(ip);
    let prefix = u32::from_be_bytes(prefix);
    let mask = if bits == 0 {
        0
    } else {
        u32::MAX << (32 - bits)
    };
    ip & mask == prefix & mask
}

fn in_ipv6_prefix(ip: Ipv6Addr, prefix: [u16; 8], bits: u8) -> bool {
    let ip = u128::from_be_bytes(ip.octets());
    let prefix = prefix
        .into_iter()
        .fold(0u128, |value, segment| (value << 16) | u128::from(segment));
    let mask = if bits == 0 {
        0
    } else {
        u128::MAX << (128 - bits)
    };
    ip & mask == prefix & mask
}

pub(crate) fn validate_encoding(response: &Response) -> Result<(), CalendarHttpError> {
    let mut values = response
        .headers()
        .get_all(reqwest::header::CONTENT_ENCODING)
        .iter();
    if let Some(value) = values.next() {
        if values.next().is_some() {
            return Err(CalendarHttpError::Encoding);
        }
        let value = value.to_str().map_err(|_| CalendarHttpError::Encoding)?;
        let mut tokens = value.split(',').map(str::trim);
        if tokens
            .next()
            .is_none_or(|token| !token.eq_ignore_ascii_case("identity"))
            || tokens.next().is_some()
        {
            return Err(CalendarHttpError::Encoding);
        }
    }
    Ok(())
}

pub(crate) fn validate_media(
    response: &Response,
    expected: ExpectedMedia,
) -> Result<(), CalendarHttpError> {
    let mut values = response
        .headers()
        .get_all(reqwest::header::CONTENT_TYPE)
        .iter();
    let value = values.next().ok_or(CalendarHttpError::MediaType)?;
    if values.next().is_some() {
        return Err(CalendarHttpError::MediaType);
    }
    let value = value.to_str().map_err(|_| CalendarHttpError::MediaType)?;
    let media = value.split(';').next().unwrap_or("").trim();
    let accepted = match expected {
        ExpectedMedia::Ics => [
            "text/calendar",
            "application/calendar",
            "application/ics",
            "text/plain",
        ]
        .iter()
        .any(|accepted| media.eq_ignore_ascii_case(accepted)),
        ExpectedMedia::Dav => ["application/xml", "text/xml", "application/dav+xml"]
            .iter()
            .any(|accepted| media.eq_ignore_ascii_case(accepted)),
    };
    accepted.then_some(()).ok_or(CalendarHttpError::MediaType)
}

/// Copy a response validator (ETag or CalDAV state token) only after applying
/// the same application-owned bound used for request state.  Repeated values
/// are refused instead of silently selecting one attacker-controlled value.
pub(crate) fn bounded_header_value(
    response: &Response,
    name: &reqwest::header::HeaderName,
) -> Result<Option<String>, CalendarHttpError> {
    let mut values = response.headers().get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(CalendarHttpError::BodyLimit);
    }
    let value = value.to_str().map_err(|_| CalendarHttpError::BodyLimit)?;
    if value.len() > MAX_REQUEST_BYTES {
        return Err(CalendarHttpError::BodyLimit);
    }
    Ok(Some(value.to_owned()))
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
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = timeout_at(deadline, stream.next())
        .await
        .map_err(|_| BodyReadError::Timeout)?
    {
        let chunk = chunk.map_err(|error| {
            if error.is_timeout() {
                BodyReadError::Timeout
            } else {
                BodyReadError::Network(error)
            }
        })?;
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(BodyReadError::Limit);
        }
        let additional = bounded_reserve_needed(body.len(), body.capacity(), chunk.len(), limit)
            .map_err(|_| BodyReadError::Limit)?;
        if additional > 0 {
            body.try_reserve_exact(additional)
                .map_err(|_| BodyReadError::Limit)?;
        }
        if body.capacity() > limit {
            return Err(BodyReadError::Limit);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn bounded_reserve_needed(
    len: usize,
    capacity: usize,
    additional: usize,
    limit: usize,
) -> Result<usize, ()> {
    let new_len = len.checked_add(additional).ok_or(())?;
    if new_len > limit {
        return Err(());
    }
    if new_len <= capacity {
        return Ok(0);
    }
    let target_capacity = capacity.saturating_mul(2).min(limit).max(new_len);
    target_capacity.checked_sub(len).ok_or(())
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
        let chunk = chunk.map_err(|error| {
            if error.is_timeout() {
                CalendarHttpError::Timeout
            } else {
                CalendarHttpError::Network
            }
        })?;
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
        for raw in ["127.0.0.1", "10.0.0.1", "169.254.1.1", "0.0.0.0"] {
            let ip = raw.parse().unwrap();
            assert!(!address_allowed(ip, false), "{raw} must be refused");
            assert!(
                address_allowed(ip, true),
                "{raw} is an intentional local range"
            );
        }
        for raw in ["224.0.0.1", "255.255.255.255", "::", "ff02::1"] {
            let ip = raw.parse().unwrap();
            assert!(!address_allowed(ip, false), "{raw} must be refused");
            assert!(
                !address_allowed(ip, true),
                "{raw} must stay refused with opt-in"
            );
        }
        for raw in ["::1", "fc00::1", "fe80::1", "::ffff:127.0.0.1"] {
            let ip = raw.parse().unwrap();
            assert!(
                !address_allowed(ip, false),
                "{raw} must be refused by default"
            );
            assert!(
                address_allowed(ip, true),
                "{raw} is an intentional local range"
            );
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
    fn public_policy_allows_only_precise_ipv4_boundaries() {
        for raw in ["192.0.1.1", "198.51.1.1", "203.0.1.1", "8.8.8.8"] {
            assert!(address_allowed(raw.parse().unwrap(), false), "{raw}");
        }
        for raw in [
            "192.0.0.1",
            "192.0.2.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "240.0.0.1",
        ] {
            assert!(!address_allowed(raw.parse().unwrap(), false), "{raw}");
            assert!(!address_allowed(raw.parse().unwrap(), true), "{raw}");
        }
    }

    #[test]
    fn public_destination_policy_rejects_non_global_ipv6_special_prefixes() {
        // These are not globally routable unicast destinations, but the
        // hand-written allowlist below would otherwise admit them.
        for raw in [
            "2001:2::1",
            "2001:10::1",
            "2001:100::1",
            "2001:1ff::1",
            "3fff::1",
        ] {
            let ip = raw.parse().unwrap();
            assert!(!address_allowed(ip, false), "{raw} must be refused");
        }
        assert!(address_allowed("2001:200::1".parse().unwrap(), false));
        for raw in ["::192.0.2.1", "::8.8.8.8"] {
            let ip = raw.parse().unwrap();
            assert!(!address_allowed(ip, false), "{raw} must be refused");
            assert!(
                !address_allowed(ip, true),
                "{raw} is deprecated compatible form"
            );
        }
    }

    #[test]
    fn bounded_reader_grows_geometrically_without_crossing_the_limit() {
        assert_eq!(bounded_reserve_needed(6, 8, 4, 10), Ok(4));
        assert_eq!(bounded_reserve_needed(7, 7, 2, 16), Ok(7));
        assert_eq!(bounded_reserve_needed(15, 15, 2, 16), Err(()));
        assert_eq!(
            bounded_reserve_needed(usize::MAX, usize::MAX, 1, usize::MAX),
            Err(())
        );

        let mut body = Vec::with_capacity(8);
        body.extend_from_slice(&[0; 6]);
        let extra = bounded_reserve_needed(body.len(), body.capacity(), 4, 10).unwrap();
        body.try_reserve_exact(extra).unwrap();
        assert_eq!(body.capacity(), 10);
        body.extend_from_slice(&[1; 4]);
        assert_eq!(body.len(), 10);
        assert!(body.capacity() <= 10);

        let mut exact = Vec::with_capacity(10);
        exact.extend_from_slice(&[0; 10]);
        assert_eq!(
            bounded_reserve_needed(exact.len(), exact.capacity(), 0, 10),
            Ok(0)
        );
        assert_eq!(bounded_reserve_needed(10, 10, 1, 10), Err(()));

        let mut repeated = Vec::new();
        for _ in 0..20 {
            let extra =
                bounded_reserve_needed(repeated.len(), repeated.capacity(), 3, 1024).unwrap();
            repeated.try_reserve_exact(extra).unwrap();
            repeated.extend_from_slice(&[2; 3]);
            assert!(repeated.capacity() <= repeated.len().saturating_mul(2));
        }
    }

    #[test]
    fn resolver_answer_cap_fails_closed() {
        let answers = (0..MAX_DNS_ANSWERS)
            .map(|port| SocketAddr::from(([8, 8, 8, 8], port)))
            .collect::<Vec<_>>();
        assert_eq!(
            collect_resolved_addresses(answers).unwrap().len(),
            MAX_DNS_ANSWERS
        );
        let too_many = (0..=MAX_DNS_ANSWERS)
            .map(|port| SocketAddr::from(([8, 8, 8, 8], port)))
            .collect::<Vec<_>>();
        assert!(collect_resolved_addresses(too_many).is_err());
    }

    #[test]
    fn calendar_requests_advertise_identity_encoding() {
        let client = CalendarHttpClient::new("https://calendar.example/feed", false).unwrap();
        let request = client.request(reqwest::Method::GET).build().unwrap();
        assert_eq!(
            request
                .headers()
                .get(reqwest::header::ACCEPT_ENCODING)
                .and_then(|value| value.to_str().ok()),
            Some("identity")
        );
    }

    #[tokio::test]
    async fn ambient_proxy_environment_cannot_bypass_destination_policy() {
        if std::env::var_os("THEGN_CALENDAR_PROXY_CHILD").is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "http::tests::ambient_proxy_environment_cannot_bypass_destination_policy",
                    "--nocapture",
                ])
                .env("THEGN_CALENDAR_PROXY_CHILD", "1")
                .env("HTTP_PROXY", "http://127.0.0.1:9")
                .env("HTTPS_PROXY", "http://127.0.0.1:9")
                .env("ALL_PROXY", "http://127.0.0.1:9")
                .env("http_proxy", "http://127.0.0.1:9")
                .env("https_proxy", "http://127.0.0.1:9")
                .env("all_proxy", "http://127.0.0.1:9")
                .env_remove("NO_PROXY")
                .env_remove("no_proxy")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "proxy-isolation child failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().fallback(axum::routing::any(
                    |_request: axum::extract::Request| async { "direct" },
                )),
            )
            .await
            .unwrap();
        });
        let client = CalendarHttpClient::new(&format!("http://{address}/feed"), true).unwrap();
        let response = client
            .send(
                client.request(reqwest::Method::GET),
                Instant::now() + Duration::from_secs(2),
            )
            .await
            .unwrap();
        assert!(response.status().is_success());
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn checked_dns_answers_are_the_addresses_used_by_the_connection_path() {
        use axum::response::IntoResponse;
        use std::sync::{Arc, Mutex};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed_hits = Arc::clone(&hits);
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().fallback(axum::routing::any(
                    move |_request: axum::extract::Request| {
                        observed_hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        async {
                            let mut response = "direct".into_response();
                            response.headers_mut().insert(
                                "connection",
                                axum::http::HeaderValue::from_static("close"),
                            );
                            response
                        }
                    },
                )),
            )
            .await
            .unwrap();
        });

        let resolutions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let make_client = |allow_private_network: bool,
                           answers: Arc<Mutex<Vec<Vec<SocketAddr>>>>|
         -> CalendarHttpClient {
            CalendarHttpClient {
                client: build_client_with_resolver(CalendarResolver {
                    allow_private_network,
                    test_answers: Some(answers),
                    test_resolutions: Some(Arc::clone(&resolutions)),
                })
                .unwrap(),
                url: Url::parse(&format!("http://calendar.test:{}/feed", address.port())).unwrap(),
            }
        };
        let answers = Arc::new(Mutex::new(vec![vec![address]]));
        let allowed = make_client(true, Arc::clone(&answers));
        // The response closes its connection below, forcing the same client
        // to resolve again.  The second answer models DNS rebinding.
        let response = allowed
            .send(
                allowed.request(reqwest::Method::GET),
                Instant::now() + Duration::from_secs(2),
            )
            .await
            .unwrap();
        assert!(response.status().is_success());
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        answers
            .lock()
            .unwrap()
            .push(vec![SocketAddr::from(([224, 0, 0, 1], address.port()))]);
        assert!(
            allowed
                .send(
                    allowed.request(reqwest::Method::GET),
                    Instant::now() + Duration::from_secs(2),
                )
                .await
                .is_err()
        );
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(resolutions.load(std::sync::atomic::Ordering::SeqCst), 2);

        // A resolver result that contains one approved and one forbidden
        // address fails closed before reqwest can try either destination.
        let mixed_answers = Arc::new(Mutex::new(vec![vec![
            address,
            SocketAddr::from(([224, 0, 0, 1], address.port())),
        ]]));
        let mixed = make_client(true, mixed_answers);
        assert!(
            mixed
                .send(
                    mixed.request(reqwest::Method::GET),
                    Instant::now() + Duration::from_secs(2),
                )
                .await
                .is_err()
        );
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);

        assert_eq!(resolutions.load(std::sync::atomic::Ordering::SeqCst), 3);
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn total_and_idle_deadlines_bound_headers_and_never_ending_bodies() {
        use axum::body::Body;
        use axum::extract::Request;
        use axum::response::IntoResponse;
        use std::convert::Infallible;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().fallback(|request: Request| async move {
                    if request.uri().path() == "/headers" {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        return ("late").into_response();
                    }
                    if request.uri().path() == "/idle" {
                        let stream = futures_util::stream::once(async {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            Ok::<_, Infallible>(vec![b'x'])
                        });
                        return Body::from_stream(stream).into_response();
                    }
                    let stream = futures_util::stream::unfold((), |_| async {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        Some((Ok::<_, Infallible>(vec![b'x']), ()))
                    });
                    let mut response = Body::from_stream(stream).into_response();
                    if request.uri().path() == "/error" {
                        *response.status_mut() = axum::http::StatusCode::INTERNAL_SERVER_ERROR;
                    }
                    response
                }),
            )
            .await
            .unwrap();
        });

        let headers = CalendarHttpClient::new(&format!("http://{address}/headers"), true).unwrap();
        assert!(matches!(
            headers
                .send(
                    headers.request(reqwest::Method::GET),
                    Instant::now() + Duration::from_millis(20),
                )
                .await,
            Err(CalendarHttpError::Timeout)
        ));

        let idle = CalendarHttpClient {
            client: build_client_with_resolver_and_read_timeout(
                CalendarResolver {
                    allow_private_network: true,
                    test_answers: None,
                    test_resolutions: None,
                },
                Duration::from_millis(20),
            )
            .unwrap(),
            url: Url::parse(&format!("http://{address}/idle")).unwrap(),
        };
        let idle_response = idle
            .send(
                idle.request(reqwest::Method::GET),
                Instant::now() + Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert!(matches!(
            read_bounded_response(idle_response, Instant::now() + Duration::from_secs(1), 128)
                .await,
            Err(BodyReadError::Timeout)
        ));

        // A body that keeps dripping is governed by the operation deadline,
        // independently of the injected idle/read timeout above.
        let body = CalendarHttpClient {
            client: build_client_with_resolver_and_read_timeout(
                CalendarResolver {
                    allow_private_network: true,
                    test_answers: None,
                    test_resolutions: None,
                },
                Duration::from_secs(1),
            )
            .unwrap(),
            url: Url::parse(&format!("http://{address}/body")).unwrap(),
        };
        let response = body
            .send(
                body.request(reqwest::Method::GET),
                Instant::now() + Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert!(matches!(
            read_bounded_response(response, Instant::now() + Duration::from_millis(25), 128,).await,
            Err(BodyReadError::Timeout)
        ));
        let error_client =
            CalendarHttpClient::new(&format!("http://{address}/error"), true).unwrap();
        let error = error_client
            .send(
                error_client.request(reqwest::Method::GET),
                Instant::now() + Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert!(matches!(
            discard_body(error, Instant::now() + Duration::from_millis(25)).await,
            Err(CalendarHttpError::Timeout)
        ));
        server.abort();
        let _ = server.await;
    }

    #[test]
    fn invalid_calendar_urls_never_construct_a_transport() {
        for raw in [
            "ftp://calendar.example/events.ics",
            "https://user:pass@calendar.example/events.ics",
            "https://127.0.0.1/events.ics",
            "http://[::1]/events.ics",
            "http://[::ffff:127.0.0.1]/events.ics",
        ] {
            assert!(CalendarHttpClient::new(raw, false).is_err(), "{raw}");
        }
    }
}
