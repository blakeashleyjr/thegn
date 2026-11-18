//! The header every recipe under `config/recipes/{sandbox,provider,connector}/`
//! carries (resolutions B4): what kind of recipe it is, how well it is tested,
//! and whether it is a copy-paste template or part of the shipped catalog.
//!
//! Stub landed by WS00-P1-08: the type and its validation only; nothing embeds
//! or reads a recipe yet. Owner after WS00: WS08-P6-00 (recipe embedding,
//! `INDEX.md` and the recipe CLIs). Recipe authors by kind: WS07 (sandbox), WS08
//! (provider), WS09 (connector).
//!
//! `config/` is already a root of the nix source filter (`nix/source.nix`), so
//! recipes there reach the sandboxed build without a filter change; there is
//! never a top-level `recipes/` directory (`tests/recipe_dir.rs` guards both).

use serde::Deserialize;

/// Which seam a recipe configures. Stub (WS00-P1-08); owner WS08-P6-00.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecipeKind {
    /// A `[sandbox.backends.<name>]` definition (WS07).
    Sandbox,
    /// A `[provider.<name>]` definition (WS08).
    Provider,
    /// A `[connector.<name>]` definition (WS09).
    Connector,
}

/// How much evidence backs a recipe. Stub (WS00-P1-08); owner WS08-P6-00.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecipeTier {
    /// Written from vendor documentation; never run.
    DocumentedOnly,
    /// Exercised against recorded fixtures.
    FixtureTested,
    /// Run against the real service; `live_tested_on` records where and when.
    LiveTested,
}

/// Whether a recipe is a template the user copies or part of the shipped
/// catalog. Stub (WS00-P1-08); owner WS08-P6-00.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecipeRegister {
    /// Copied into user config by hand or by `recipe add` (the default).
    #[default]
    Template,
    /// Registered in the shipped backend catalog (sandbox recipes only).
    Shipped,
}

/// A recipe file's header. Stub (WS00-P1-08); owner WS08-P6-00.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeHeader {
    /// `sandbox`, `provider` or `connector`.
    pub kind: RecipeKind,
    /// `documented-only`, `fixture-tested` or `live-tested`.
    pub tier: RecipeTier,
    /// Where and when a live test passed, e.g. `"linux/x86_64@2026-11-20"`.
    #[serde(default)]
    pub live_tested_on: Vec<String>,
    /// `YYYY-MM-DD.N`.
    pub revision: String,
    /// `YYYY-MM-DD` of the last verification, if any.
    pub last_verified: Option<String>,
    /// Spike ids the recipe depends on.
    #[serde(default)]
    pub spikes: Vec<String>,
    /// Platforms the recipe applies to.
    #[serde(default)]
    pub platforms: Vec<String>,
    /// `template` (default) or `shipped`.
    #[serde(default)]
    pub register: RecipeRegister,
}

/// `YYYY-MM-DD` with a plausible month and day.
fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    let digits = |range: std::ops::Range<usize>| b[range].iter().all(u8::is_ascii_digit);
    if !(digits(0..4) && digits(5..7) && digits(8..10)) {
        return false;
    }
    let month = (b[5] - b'0') * 10 + (b[6] - b'0');
    let day = (b[8] - b'0') * 10 + (b[9] - b'0');
    (1..=12).contains(&month) && (1..=31).contains(&day)
}

/// `YYYY-MM-DD.N` with `N` a positive decimal number.
fn is_revision(s: &str) -> bool {
    let Some((date, n)) = s.split_once('.') else {
        return false;
    };
    is_date(date)
        && !n.is_empty()
        && n.bytes().all(|b| b.is_ascii_digit())
        && n.bytes().any(|b| b != b'0')
}

impl RecipeHeader {
    /// Check the rules a parse cannot: `shipped` only for sandbox recipes,
    /// `live-tested` needs at least one `live_tested_on` entry, and `revision`
    /// is `YYYY-MM-DD.N`. The error is a one-line reason.
    pub fn validate(&self) -> Result<(), String> {
        if self.register == RecipeRegister::Shipped && self.kind != RecipeKind::Sandbox {
            return Err("register = \"shipped\" is only valid for sandbox recipes".into());
        }
        if self.tier == RecipeTier::LiveTested && self.live_tested_on.is_empty() {
            return Err("tier = \"live-tested\" needs at least one live_tested_on entry".into());
        }
        if !is_revision(&self.revision) {
            return Err(format!("revision {:?} is not YYYY-MM-DD.N", self.revision));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<RecipeHeader, toml::de::Error> {
        toml::from_str(text)
    }

    #[test]
    fn header_parses_minimal() {
        let header =
            parse("kind = \"provider\"\ntier = \"documented-only\"\nrevision = \"2026-11-17.1\"\n")
                .unwrap();
        assert_eq!(header.kind, RecipeKind::Provider);
        assert_eq!(header.tier, RecipeTier::DocumentedOnly);
        assert_eq!(header.register, RecipeRegister::Template);
        assert!(header.live_tested_on.is_empty());
        assert!(header.spikes.is_empty() && header.platforms.is_empty());
        assert_eq!(header.last_verified, None);
        assert_eq!(header.validate(), Ok(()));

        let full = parse(
            "kind = \"sandbox\"\ntier = \"live-tested\"\n\
             live_tested_on = [\"linux/x86_64@2026-11-20\"]\nrevision = \"2026-11-20.2\"\n\
             last_verified = \"2026-11-20\"\nspikes = [\"S3\"]\nplatforms = [\"linux\"]\n\
             register = \"shipped\"\n",
        )
        .unwrap();
        assert_eq!(full.register, RecipeRegister::Shipped);
        assert_eq!(full.tier, RecipeTier::LiveTested);
        assert_eq!(full.validate(), Ok(()));
        let connector = parse(
            "kind = \"connector\"\ntier = \"fixture-tested\"\nrevision = \"2026-01-31.10\"\n",
        )
        .unwrap();
        assert_eq!(connector.kind, RecipeKind::Connector);
        assert_eq!(connector.tier, RecipeTier::FixtureTested);
        assert_eq!(connector.validate(), Ok(()));
    }

    #[test]
    fn header_rejects_unknown_key() {
        assert!(
            parse(
                "kind = \"sandbox\"\ntier = \"documented-only\"\nrevision = \"2026-11-17.1\"\n\
                 trusted = true\n"
            )
            .is_err()
        );
        assert!(
            parse("kind = \"vm\"\ntier = \"documented-only\"\nrevision = \"2026-11-17.1\"\n")
                .is_err()
        );
        assert!(parse("kind = \"sandbox\"\nrevision = \"2026-11-17.1\"\n").is_err());
    }

    #[test]
    fn shipped_only_for_sandbox() {
        for kind in ["provider", "connector"] {
            let header = parse(&format!(
                "kind = \"{kind}\"\ntier = \"documented-only\"\nrevision = \"2026-11-17.1\"\n\
                 register = \"shipped\"\n"
            ))
            .unwrap();
            let err = header.validate().unwrap_err();
            assert!(err.contains("shipped"), "{err}");
        }
    }

    #[test]
    fn live_tested_needs_evidence() {
        let header =
            parse("kind = \"sandbox\"\ntier = \"live-tested\"\nrevision = \"2026-11-17.1\"\n")
                .unwrap();
        let err = header.validate().unwrap_err();
        assert!(err.contains("live_tested_on"), "{err}");
    }

    #[test]
    fn revision_format_checked() {
        for bad in [
            "2026-11-17",
            "2026-11-17.",
            "2026-11-17.0",
            "2026-11-17.x",
            "2026-13-01.1",
            "2026-00-10.1",
            "2026-11-32.1",
            "2026-11-00.1",
            "26-11-17.1",
            "2026/11/17.1",
            "2026-1a-17.1",
            "",
        ] {
            let header = RecipeHeader {
                kind: RecipeKind::Sandbox,
                tier: RecipeTier::DocumentedOnly,
                live_tested_on: Vec::new(),
                revision: bad.into(),
                last_verified: None,
                spikes: Vec::new(),
                platforms: Vec::new(),
                register: RecipeRegister::Template,
            };
            let err = header.validate().unwrap_err();
            assert!(err.contains("YYYY-MM-DD.N"), "{bad}: {err}");
        }
        assert!(is_revision("2026-11-17.12"));
    }
}
