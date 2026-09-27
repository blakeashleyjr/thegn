//! Host boundary for MPD endpoint credentials. Passwords are resolved and
//! audited here, before any leaf backend is constructed.

use thegn_core::config::MediaConfig;
use thegn_core::secretref::SecretRef;

use crate::secret::{SecretResolveFailure, resolve_ref_for_outcome};

const CONSUMER: &str = "media:mpd";

pub(crate) fn resolve_opts(
    cfg: &MediaConfig,
) -> Result<thegn_media::ResolveOpts, SecretResolveFailure> {
    let mut opts = cfg.resolve_opts();
    if opts.backend != thegn_media::BackendKind::Mpd
        && !(opts.backend == thegn_media::BackendKind::Auto && cfg!(target_os = "linux"))
    {
        return Ok(opts);
    }
    let (socket, embedded) = effective_socket(&opts.mpd_socket);
    opts.mpd_socket = socket;

    let configured = cfg.mpd.password.as_ref().map(|value| value.secret_ref());
    if let Some(reference) = configured.or(embedded.as_ref()) {
        let value = match resolve_ref_for_outcome(&reference, CONSUMER) {
            Ok(Some(value)) => value,
            Ok(None) if opts.backend == thegn_media::BackendKind::Auto => {
                let outcome = SecretResolveFailure::Missing;
                tracing::warn!(target: "thegn::media", component = CONSUMER, %outcome, "MPD source skipped after credential resolution refusal");
                opts.mpd_socket.clear();
                return Ok(opts);
            }
            Ok(None) => return Err(SecretResolveFailure::Missing),
            Err(outcome) if opts.backend == thegn_media::BackendKind::Auto => {
                tracing::warn!(target: "thegn::media", component = CONSUMER, %outcome, "MPD source skipped after credential resolution refusal");
                opts.mpd_socket.clear();
                return Ok(opts);
            }
            Err(outcome) => return Err(outcome),
        };
        opts.mpd_password = Some(thegn_media::MpdPassword::new(value));
    }
    Ok(opts)
}

pub(crate) async fn client_for(cfg: &MediaConfig) -> Option<thegn_media::MediaClient> {
    let opts = match resolve_opts(cfg) {
        Ok(opts) => opts,
        Err(outcome) => {
            tracing::warn!(target: "thegn::media", component = CONSUMER, %outcome, "MPD credential resolution refused");
            return None;
        }
    };
    thegn_media::client_for(opts).await
}

fn effective_socket(configured: &str) -> (String, Option<SecretRef>) {
    let trimmed = configured.trim();
    if !trimmed.is_empty() && trimmed != "127.0.0.1:6600" {
        return (trimmed.to_string(), None);
    }
    let Ok(host) = std::env::var("MPD_HOST") else {
        return (configured.to_string(), None);
    };
    if host.is_empty() {
        return (configured.to_string(), None);
    }
    let port = std::env::var("MPD_PORT")
        .ok()
        .and_then(|port| port.trim().parse::<u16>().ok())
        .unwrap_or(6600);
    parse_host(&host, port)
}

fn parse_host(value: &str, port: u16) -> (String, Option<SecretRef>) {
    let (host, reference) = match value.split_once('@') {
        Some((password, host)) if !password.is_empty() => (
            host.to_string(),
            Some(SecretRef::Literal(
                thegn_core::secretref::LiteralSecret::new(password.to_string()),
            )),
        ),
        _ => (value.to_string(), None),
    };
    let socket = if host.starts_with('/') {
        host
    } else {
        format!("{host}:{port}")
    };
    (socket, reference)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_host_password_is_parsed_as_a_redacted_literal() {
        let password = "must-not-leak-test-secret";
        let (socket, reference) = parse_host(&format!("{password}@music.example"), 6602);
        assert_eq!(socket, "music.example:6602");
        let reference = reference.unwrap();
        assert!(reference.is_literal());
        assert!(!format!("{reference:?}").contains(password));
    }

    #[test]
    fn explicit_host_and_abstract_prefix_never_parse_as_a_password() {
        assert_eq!(parse_host("music.example", 6600).0, "music.example:6600");
        let (socket, reference) = parse_host("@music.example", 6600);
        assert_eq!(socket, "@music.example:6600");
        assert!(reference.is_none());
    }

    #[test]
    fn resolution_failures_are_typed_and_never_render_the_configured_value() {
        let sentinel = "must-not-appear-in-error";
        for failure in [
            SecretResolveFailure::Missing,
            SecretResolveFailure::Unavailable,
            SecretResolveFailure::Denied,
        ] {
            let rendered = format!("media:mpd: {failure}");
            assert!(!rendered.contains(sentinel));
            assert!(rendered.contains("media:mpd"));
        }
        assert_ne!(
            SecretResolveFailure::Missing,
            SecretResolveFailure::Unavailable
        );
    }

    #[test]
    fn explicit_missing_ref_fails_before_mpd_client_construction() {
        let mut cfg = MediaConfig::default();
        cfg.backend = thegn_core::config::MediaBackendKind::Mpd;
        cfg.mpd.password = Some("env:TG_MPD_SECRET_DEFINITELY_UNSET".into());
        assert!(matches!(
            resolve_opts(&cfg),
            Err(SecretResolveFailure::Missing)
        ));
    }

    #[test]
    fn auto_skips_only_mpd_when_its_explicit_credential_is_missing() {
        let mut cfg = MediaConfig::default();
        cfg.mpd.password = Some("env:TG_MPD_SECRET_DEFINITELY_UNSET".into());
        let opts = resolve_opts(&cfg).unwrap();
        assert!(opts.mpd_socket.is_empty());
        assert!(opts.mpd_password.is_none());
    }
}
