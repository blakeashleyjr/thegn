//! Embedded Fluent source registry and strict key parity checks.

use std::collections::BTreeSet;

pub const DEFAULT_LOCALE: &str = "en-US";
pub const DEFAULT_SOURCE: &str = include_str!("../locales/en-US/main.ftl");

#[derive(Debug, Clone, Copy)]
pub struct LocaleSource {
    pub locale: &'static str,
    pub source: &'static str,
}

/// The one registry of locales shipped in the binary. Adding a source here
/// automatically enrolls it in strict parity testing.
pub const SHIPPED_LOCALES: &[LocaleSource] = &[LocaleSource {
    locale: DEFAULT_LOCALE,
    source: DEFAULT_SOURCE,
}];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParityIssue {
    Orphan { locale: String, key: String },
    Missing { locale: String, key: String },
}

pub fn parity_issues(sources: &[LocaleSource]) -> Vec<ParityIssue> {
    let Some(default) = sources
        .iter()
        .find(|source| source.locale == DEFAULT_LOCALE)
    else {
        return vec![ParityIssue::Missing {
            locale: DEFAULT_LOCALE.to_string(),
            key: "<default locale source>".to_string(),
        }];
    };
    let schema = message_keys(default.source);
    let mut issues = Vec::new();

    for source in sources {
        let keys = message_keys(source.source);
        issues.extend(keys.difference(&schema).map(|key| ParityIssue::Orphan {
            locale: source.locale.to_string(),
            key: key.clone(),
        }));
        issues.extend(schema.difference(&keys).map(|key| ParityIssue::Missing {
            locale: source.locale.to_string(),
            key: key.clone(),
        }));
    }
    issues
}

/// Fold top-level Fluent message/term identifiers. The embedded loader parses
/// syntax at build time; this fold deliberately only owns schema comparison.
fn message_keys(source: &str) -> BTreeSet<String> {
    source
        .lines()
        .filter(|line| !line.starts_with(char::is_whitespace))
        .filter_map(|line| line.split_once('=').map(|(id, _)| id.trim()))
        .filter(|id| {
            !id.is_empty()
                && id
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        })
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shipped_locale_has_exact_default_key_parity() {
        assert_eq!(parity_issues(SHIPPED_LOCALES), Vec::new());
    }

    #[test]
    fn fold_names_every_orphan_and_missing_key() {
        let sources = [
            LocaleSource {
                locale: DEFAULT_LOCALE,
                source: "alpha = A\nbeta = B\n",
            },
            LocaleSource {
                locale: "ja-JP",
                source: "beta = B\ngamma = G\n",
            },
        ];
        assert_eq!(
            parity_issues(&sources),
            vec![
                ParityIssue::Orphan {
                    locale: "ja-JP".to_string(),
                    key: "gamma".to_string(),
                },
                ParityIssue::Missing {
                    locale: "ja-JP".to_string(),
                    key: "alpha".to_string(),
                },
            ]
        );
    }

    #[test]
    fn fold_ignores_comments_attributes_and_multiline_values() {
        let keys = message_keys(
            "# prose\nmessage = Value\n    .label = Label\n    continuation\n-term = Term\n",
        );
        assert_eq!(
            keys,
            BTreeSet::from(["-term".to_string(), "message".to_string()])
        );
    }
}
