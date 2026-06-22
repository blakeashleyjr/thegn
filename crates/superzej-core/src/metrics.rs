//! Minimal Prometheus text-format parser for the sidebar metrics scraper.
//!
//! The parser intentionally extracts only sample rows into a compact struct. It
//! ignores comments, malformed lines, and optional timestamps; high-cardinality
//! reduction happens via [`filter_samples`] before data reaches the UI.

use std::collections::BTreeMap;

/// One metric sample.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricSample {
    pub name: String,
    pub value: f64,
    pub labels: BTreeMap<String, String>,
}

/// Parse Prometheus text format into samples.
pub fn parse_metrics(input: &str) -> Vec<MetricSample> {
    input
        .lines()
        .filter_map(|line| parse_sample_line(line.trim()))
        .collect()
}

fn parse_sample_line(line: &str) -> Option<MetricSample> {
    if line.is_empty() || line.starts_with('#') {
        return None;
    }

    let (name_and_labels, rest) = split_first_ws(line)?;
    let value_str = rest.split_whitespace().next()?;
    let value = value_str.parse::<f64>().ok()?;
    if !value.is_finite() {
        return None;
    }

    let (name, labels) = parse_name_and_labels(name_and_labels)?;
    Some(MetricSample {
        name,
        value,
        labels,
    })
}

fn split_first_ws(s: &str) -> Option<(&str, &str)> {
    let idx = s.find(char::is_whitespace)?;
    let left = &s[..idx];
    let right = s[idx..].trim_start();
    (!left.is_empty() && !right.is_empty()).then_some((left, right))
}

fn parse_name_and_labels(s: &str) -> Option<(String, BTreeMap<String, String>)> {
    let Some(open) = s.find('{') else {
        return valid_metric_name(s).then(|| (s.to_string(), BTreeMap::new()));
    };
    let name = &s[..open];
    if !valid_metric_name(name) || !s.ends_with('}') {
        return None;
    }
    let labels = parse_labels(&s[open + 1..s.len() - 1])?;
    Some((name.to_string(), labels))
}

fn valid_metric_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first == ':' || first.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c == ':' || c.is_ascii_alphanumeric())
}

fn parse_labels(s: &str) -> Option<BTreeMap<String, String>> {
    let mut labels = BTreeMap::new();
    let mut rest = s.trim();
    while !rest.is_empty() {
        let eq = rest.find('=')?;
        let key = rest[..eq].trim();
        if !valid_label_name(key) {
            return None;
        }
        rest = rest[eq + 1..].trim_start();
        if !rest.starts_with('"') {
            return None;
        }
        let (value, consumed) = parse_quoted(&rest[1..])?;
        labels.insert(key.to_string(), value);
        rest = rest[1 + consumed..].trim_start();
        if rest.is_empty() {
            break;
        }
        if !rest.starts_with(',') {
            return None;
        }
        rest = rest[1..].trim_start();
    }
    Some(labels)
}

fn valid_label_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Parse a quoted Prometheus label value. Input starts after the opening quote;
/// returned byte count includes the closing quote.
fn parse_quoted(s: &str) -> Option<(String, usize)> {
    let mut out = String::new();
    let mut escaped = false;
    for (idx, ch) in s.char_indices() {
        if escaped {
            out.push(match ch {
                'n' => '\n',
                '\\' => '\\',
                '"' => '"',
                other => other,
            });
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => return Some((out, idx + ch.len_utf8())),
            other => out.push(other),
        }
    }
    None
}

/// Filter samples by allowlisted metric names and optional label matchers.
///
/// Allowlist entries are exact metric names. An empty allowlist admits all
/// samples, but label filters still apply.
pub fn filter_samples(
    samples: &[MetricSample],
    allowlist: &[String],
    labels: &BTreeMap<String, String>,
) -> Vec<MetricSample> {
    samples
        .iter()
        .filter(|s| {
            if !allowlist.is_empty() && !allowlist.iter().any(|name| name == &s.name) {
                return false;
            }
            labels
                .iter()
                .all(|(key, expected)| s.labels.get(key) == Some(expected))
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{filter_samples, parse_metrics};

    #[test]
    fn parses_prometheus_samples_with_labels_and_ignores_comments() {
        let input = r#"
# HELP http_requests_total Total requests
# TYPE http_requests_total counter
http_requests_total{method="GET",code="200"} 12345
process_resident_memory_bytes 82440192
invalid_without_value
bad_metric nope
"#;
        let samples = parse_metrics(input);
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].name, "http_requests_total");
        assert_eq!(samples[0].value, 12345.0);
        assert_eq!(
            samples[0].labels.get("method").map(String::as_str),
            Some("GET")
        );
        assert_eq!(
            samples[0].labels.get("code").map(String::as_str),
            Some("200")
        );
        assert_eq!(samples[1].name, "process_resident_memory_bytes");
        assert_eq!(samples[1].value, 82440192.0);
    }

    #[test]
    fn filter_samples_matches_allowlist_and_required_labels() {
        let samples = parse_metrics(
            r#"
http_requests_total{method="GET",code="200"} 10
http_requests_total{method="POST",code="200"} 3
go_goroutines 42
"#,
        );
        let mut labels = BTreeMap::new();
        labels.insert("method".to_string(), "GET".to_string());
        let allowlist = vec!["http_requests_total".to_string()];
        let filtered = filter_samples(&samples, &allowlist, &labels);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].value, 10.0);
    }

    #[test]
    fn parses_escaped_label_values() {
        let samples = parse_metrics("metric{path=\"/a,\\\"quoted\\\"\",kind=\"x\"} 1\n");
        assert_eq!(samples.len(), 1);
        assert_eq!(
            samples[0].labels.get("path").map(String::as_str),
            Some("/a,\"quoted\"")
        );
        assert_eq!(samples[0].labels.get("kind").map(String::as_str), Some("x"));
    }

    #[test]
    fn rejects_invalid_metric_names_and_non_finite_values() {
        assert!(parse_metrics("9bad 1\n").is_empty());
        assert!(parse_metrics("good NaN\n").is_empty());
        assert!(parse_metrics("good{bad-label=\"x\"} 1\n").is_empty());
    }

    #[test]
    fn rejects_inf_and_unparseable_values() {
        // +Inf / -Inf parse as f64 but are non-finite → rejected.
        assert!(parse_metrics("good inf\n").is_empty());
        assert!(parse_metrics("good -inf\n").is_empty());
        // A value token that isn't a number at all.
        assert!(parse_metrics("good notanumber\n").is_empty());
    }

    #[test]
    fn rejects_malformed_label_blocks() {
        // Opening brace but no closing brace.
        assert!(parse_metrics("m{a=\"1\" 5\n").is_empty());
        // A label with no '=' separator.
        assert!(parse_metrics("m{nope} 5\n").is_empty());
        // A label value that is not quoted.
        assert!(parse_metrics("m{a=1} 5\n").is_empty());
        // An empty label name.
        assert!(parse_metrics("m{=\"x\"} 5\n").is_empty());
        // Missing comma between two labels.
        assert!(parse_metrics("m{a=\"1\" b=\"2\"} 5\n").is_empty());
        // Unterminated quoted value (parse_quoted returns None).
        assert!(parse_metrics("m{a=\"unterminated} 5\n").is_empty());
    }

    #[test]
    fn rejects_lines_missing_value_or_name() {
        // Only whitespace after trimming the name → split_first_ws yields None
        // (handled as no-value); also a name-only line with no value token.
        assert!(parse_metrics("lonely\n").is_empty());
        // A line that is only a value with leading whitespace collapses to one
        // token after trim, so there is no value field.
        assert!(parse_metrics("   onlyname   \n").is_empty());
    }

    #[test]
    fn bare_metric_without_labels_parses() {
        let samples = parse_metrics("up 1\n");
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].name, "up");
        assert!(samples[0].labels.is_empty());
        // A name starting with ':' or '_' is valid.
        assert_eq!(parse_metrics(":colon_ok 1\n").len(), 1);
        assert_eq!(parse_metrics("_under 1\n").len(), 1);
    }

    #[test]
    fn empty_allowlist_admits_all_but_label_filter_still_applies() {
        let samples = parse_metrics("a{env=\"prod\"} 1\nb{env=\"dev\"} 2\nc 3\n");
        let mut labels = BTreeMap::new();
        labels.insert("env".to_string(), "prod".to_string());
        // Empty allowlist → all names admitted, but env=prod required.
        let filtered = filter_samples(&samples, &[], &labels);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "a");

        // Empty allowlist + empty label filter → everything passes.
        let all = filter_samples(&samples, &[], &BTreeMap::new());
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn parses_escaped_newline_and_passthrough_escapes() {
        // \n maps to newline; an unknown escape (e.g. \t) passes the char through.
        let samples = parse_metrics("m{a=\"x\\ny\",b=\"c\\td\"} 1\n");
        assert_eq!(samples[0].labels.get("a").map(String::as_str), Some("x\ny"));
        assert_eq!(samples[0].labels.get("b").map(String::as_str), Some("ctd"));
    }
}
