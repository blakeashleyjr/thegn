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

/// The metadata table at the top of a recipe TOML file.
///
/// Payload tables (for example `[sandbox]`) are intentionally left to the
/// recipe-kind owner. Unknown keys inside `[recipe]` remain rejected by
/// [`RecipeHeader`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RecipeDocumentHeader {
    /// The `[recipe]` table.
    pub recipe: RecipeHeader,
}

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

/// A valid Gregorian `YYYY-MM-DD` date.
fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    let digits = |range: std::ops::Range<usize>| b[range].iter().all(u8::is_ascii_digit);
    if !(digits(0..4) && digits(5..7) && digits(8..10)) {
        return false;
    }
    let year = u16::from(b[0] - b'0') * 1000
        + u16::from(b[1] - b'0') * 100
        + u16::from(b[2] - b'0') * 10
        + u16::from(b[3] - b'0');
    let month = (b[5] - b'0') * 10 + (b[6] - b'0');
    let day = (b[8] - b'0') * 10 + (b[9] - b'0');
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 400 == 0 || (year % 4 == 0 && year % 100 != 0) => 29,
        2 => 28,
        _ => return false,
    };
    (1..=max_day).contains(&day)
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
        if self
            .last_verified
            .as_deref()
            .is_some_and(|date| !is_date(date))
        {
            return Err("last_verified is not a valid YYYY-MM-DD date".into());
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

    fn parse(text: &str) -> Result<RecipeDocumentHeader, toml::de::Error> {
        toml::from_str(text)
    }

    #[test]
    fn header_parses_minimal() {
        let header =
            parse("[recipe]\nkind = \"provider\"\ntier = \"documented-only\"\nrevision = \"2026-11-17.1\"\n")
                .unwrap();
        assert_eq!(header.recipe.kind, RecipeKind::Provider);
        assert_eq!(header.recipe.tier, RecipeTier::DocumentedOnly);
        assert_eq!(header.recipe.register, RecipeRegister::Template);
        assert!(header.recipe.live_tested_on.is_empty());
        assert!(header.recipe.spikes.is_empty() && header.recipe.platforms.is_empty());
        assert_eq!(header.recipe.last_verified, None);
        assert_eq!(header.recipe.validate(), Ok(()));

        let full = parse(
            "[recipe]\nkind = \"sandbox\"\ntier = \"live-tested\"\n\
             live_tested_on = [\"linux/x86_64@2026-11-20\"]\nrevision = \"2026-11-20.2\"\n\
             last_verified = \"2026-11-20\"\nspikes = [\"S3\"]\nplatforms = [\"linux\"]\n\
             register = \"shipped\"\n",
        )
        .unwrap();
        assert_eq!(full.recipe.register, RecipeRegister::Shipped);
        assert_eq!(full.recipe.tier, RecipeTier::LiveTested);
        assert_eq!(full.recipe.validate(), Ok(()));
        let file_with_payload = parse(
            "[recipe]\nkind = \"sandbox\"\ntier = \"fixture-tested\"\nrevision = \"2026-11-20.1\"\n\
             [sandbox]\nimage = \"example.invalid/inert\"\n",
        )
        .unwrap();
        assert_eq!(file_with_payload.recipe.validate(), Ok(()));
        let connector = parse(
            "[recipe]\nkind = \"connector\"\ntier = \"fixture-tested\"\nrevision = \"2026-01-31.10\"\n",
        )
        .unwrap();
        assert_eq!(connector.recipe.kind, RecipeKind::Connector);
        assert_eq!(connector.recipe.tier, RecipeTier::FixtureTested);
        assert_eq!(connector.recipe.validate(), Ok(()));
    }

    #[test]
    fn header_rejects_unknown_key() {
        assert!(
            parse(
                "[recipe]\nkind = \"sandbox\"\ntier = \"documented-only\"\nrevision = \"2026-11-17.1\"\n\
                 trusted = true\n"
            )
            .is_err()
        );
        assert!(
            parse(
                "[recipe]\nkind = \"vm\"\ntier = \"documented-only\"\nrevision = \"2026-11-17.1\"\n"
            )
            .is_err()
        );
        assert!(parse("[recipe]\nkind = \"sandbox\"\nrevision = \"2026-11-17.1\"\n").is_err());
        // A header key outside `[recipe]` is not part of the document.
        assert!(parse("kind = \"sandbox\"\n").is_err());
    }

    #[test]
    fn shipped_only_for_sandbox() {
        for kind in ["provider", "connector"] {
            let header = parse(&format!(
                "[recipe]\nkind = \"{kind}\"\ntier = \"documented-only\"\nrevision = \"2026-11-17.1\"\n\
                 register = \"shipped\"\n"
            ))
            .unwrap();
            let err = header.recipe.validate().unwrap_err();
            assert!(err.contains("shipped"), "{err}");
        }
    }

    #[test]
    fn live_tested_needs_evidence() {
        let header = parse(
            "[recipe]\nkind = \"sandbox\"\ntier = \"live-tested\"\nrevision = \"2026-11-17.1\"\n",
        )
        .unwrap();
        let err = header.recipe.validate().unwrap_err();
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
            "2026-02-29.1",
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
        assert!(is_revision("2024-02-29.1"));
        assert!(!is_date("2026-02-29"));
        assert!(is_date("2024-02-29"));
        assert!(is_date("2000-02-29"));
        assert!(!is_date("1900-02-29"));
        let header = parse(
            "[recipe]\nkind = \"sandbox\"\ntier = \"documented-only\"\nrevision = \"2026-11-17.1\"\n\
             last_verified = \"2026-02-30\"\n",
        )
        .unwrap();
        assert!(
            header
                .recipe
                .validate()
                .unwrap_err()
                .contains("last_verified")
        );
    }
}
