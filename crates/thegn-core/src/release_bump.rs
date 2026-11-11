//! Pure release-bump core behind `thegn release prepare`.
//!
//! Computes the next version for a [`BumpKind`] and rewrites the four files a
//! release touches (`Cargo.toml`, `Cargo.lock`, `README.md`, `CHANGELOG.md`).
//! No I/O and no clock: the host reads and writes the files and passes
//! `today` in. Every rewrite refuses (typed [`ReleaseError`]) rather than
//! guessing, and refuses to run twice over already-bumped text.

use std::fmt;

/// Fewest `Cargo.lock` workspace entries a bump may touch; fewer means the
/// lock file does not look like this workspace's.
pub const MIN_LOCK_ENTRIES: usize = 5;

/// Repository URL used in the changelog link footer.
const COMPARE_BASE: &str = "https://github.com/blakeashleyjr/thegn/compare";

/// Which release step to take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BumpKind {
    /// `-alpha.N` -> `-alpha.N+1` (stable -> next patch `-alpha.1`).
    Alpha,
    /// alpha -> `-beta.1`, beta -> `-beta.N+1`.
    Beta,
    /// alpha/beta -> `-rc.1`, rc -> `-rc.N+1`.
    Rc,
    /// Pre-release -> its stable version; stable -> next patch.
    Patch,
}

/// A parsed `X.Y.Z[-channel.N]` version. `channel` is empty for stable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub channel: String,
    pub n: u32,
}

/// Everything a release rewrite can refuse for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseError {
    /// The version string is not `X.Y.Z` or `X.Y.Z-{alpha,beta,rc}.N`.
    Malformed(String),
    /// The requested bump would move the release channel backwards.
    Backwards { from: String, kind: &'static str },
    /// A marker the rewrite anchors on is absent from `file`.
    MissingMarker { file: &'static str, marker: String },
    /// `## [Unreleased]` appears `count` times (must be exactly once).
    AmbiguousUnreleased { count: usize },
    /// The text already carries the new version (a second run).
    AlreadyBumped { file: &'static str, version: String },
    /// The recorded version is not the expected old version.
    VersionMismatch {
        file: &'static str,
        found: String,
        expected: String,
    },
    /// Fewer than [`MIN_LOCK_ENTRIES`] workspace entries in `Cargo.lock`.
    TooFewLockEntries { found: usize },
}

impl fmt::Display for ReleaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(v) => write!(f, "malformed version `{v}`"),
            Self::Backwards { from, kind } => {
                write!(
                    f,
                    "cannot bump `{from}` with `{kind}`: that moves backwards"
                )
            }
            Self::MissingMarker { file, marker } => {
                write!(f, "{file}: marker `{marker}` not found")
            }
            Self::AmbiguousUnreleased { count } => write!(
                f,
                "CHANGELOG.md: expected exactly one `## [Unreleased]` heading, found {count}"
            ),
            Self::AlreadyBumped { file, version } => {
                write!(f, "{file}: already bumped to `{version}`")
            }
            Self::VersionMismatch {
                file,
                found,
                expected,
            } => write!(f, "{file}: found version `{found}`, expected `{expected}`"),
            Self::TooFewLockEntries { found } => write!(
                f,
                "Cargo.lock: only {found} workspace entries bumped (need at least {MIN_LOCK_ENTRIES})"
            ),
        }
    }
}

impl std::error::Error for ReleaseError {}

type Result<T> = std::result::Result<T, ReleaseError>;

impl BumpKind {
    fn name(self) -> &'static str {
        match self {
            Self::Alpha => "alpha",
            Self::Beta => "beta",
            Self::Rc => "rc",
            Self::Patch => "patch",
        }
    }
}

impl Version {
    /// Parse `X.Y.Z` or `X.Y.Z-{alpha,beta,rc}.N`.
    pub fn parse(s: &str) -> Result<Version> {
        let bad = || ReleaseError::Malformed(s.to_string());
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (s, None),
        };
        let mut nums = core.split('.');
        let mut num = || -> Result<u32> {
            let part = nums.next().ok_or_else(bad)?;
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return Err(bad());
            }
            part.parse().map_err(|_| bad())
        };
        let (major, minor, patch) = (num()?, num()?, num()?);
        if nums.next().is_some() {
            return Err(bad());
        }
        let (channel, n) = match pre {
            None => (String::new(), 0),
            Some(p) => {
                let (ch, n) = p.split_once('.').ok_or_else(bad)?;
                if !matches!(ch, "alpha" | "beta" | "rc")
                    || n.is_empty()
                    || !n.bytes().all(|b| b.is_ascii_digit())
                {
                    return Err(bad());
                }
                (ch.to_string(), n.parse().map_err(|_| bad())?)
            }
        };
        Ok(Version {
            major,
            minor,
            patch,
            channel,
            n,
        })
    }

    /// True when this is a stable (no pre-release) version.
    pub fn is_stable(&self) -> bool {
        self.channel.is_empty()
    }

    fn with(&self, patch: u32, channel: &str, n: u32) -> Version {
        Version {
            major: self.major,
            minor: self.minor,
            patch,
            channel: channel.to_string(),
            n,
        }
    }

    /// The version a `kind` bump produces from `self`.
    pub fn next(&self, kind: BumpKind) -> Result<Version> {
        let backwards = || ReleaseError::Backwards {
            from: self.to_string(),
            kind: kind.name(),
        };
        let ch = self.channel.as_str();
        match (kind, ch) {
            (BumpKind::Alpha, "") => Ok(self.with(self.patch + 1, "alpha", 1)),
            (BumpKind::Alpha, "alpha") => Ok(self.with(self.patch, "alpha", self.n + 1)),
            (BumpKind::Alpha, _) => Err(backwards()),
            (BumpKind::Beta, "alpha") => Ok(self.with(self.patch, "beta", 1)),
            (BumpKind::Beta, "beta") => Ok(self.with(self.patch, "beta", self.n + 1)),
            (BumpKind::Beta, _) => Err(backwards()),
            (BumpKind::Rc, "alpha" | "beta") => Ok(self.with(self.patch, "rc", 1)),
            (BumpKind::Rc, "rc") => Ok(self.with(self.patch, "rc", self.n + 1)),
            (BumpKind::Rc, _) => Err(backwards()),
            (BumpKind::Patch, "") => Ok(self.with(self.patch + 1, "", 0)),
            (BumpKind::Patch, _) => Ok(self.with(self.patch, "", 0)),
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.channel.is_empty() {
            write!(f, "-{}.{}", self.channel, self.n)?;
        }
        Ok(())
    }
}

/// The old -> new transition plus the date stamped into the changelog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BumpPlan {
    pub old: Version,
    pub new: Version,
    /// `YYYY-MM-DD`.
    pub today: String,
}

impl BumpPlan {
    /// Plan a `kind` bump from `old`.
    pub fn new(old: Version, kind: BumpKind, today: &str) -> Result<BumpPlan> {
        let new = old.next(kind)?;
        Ok(BumpPlan {
            old,
            new,
            today: today.to_string(),
        })
    }
}

/// Read the current `[workspace.package]` version out of `Cargo.toml`.
pub fn workspace_version(text: &str) -> Result<Version> {
    const MARKER: &str = "[workspace.package]\nversion = \"";
    let missing = || ReleaseError::MissingMarker {
        file: "Cargo.toml",
        marker: MARKER.replace('\n', "\\n"),
    };
    let start = text.find(MARKER).ok_or_else(missing)? + MARKER.len();
    let len = text[start..].find('"').ok_or_else(missing)?;
    Version::parse(&text[start..start + len])
}

/// Rewrite `[workspace.package] version = "<old>"` to the new version.
pub fn bump_cargo_toml(text: &str, plan: &BumpPlan) -> Result<String> {
    const FILE: &str = "Cargo.toml";
    const MARKER: &str = "[workspace.package]\nversion = \"";
    let missing = || ReleaseError::MissingMarker {
        file: FILE,
        marker: MARKER.replace('\n', "\\n"),
    };
    let start = text.find(MARKER).ok_or_else(missing)? + MARKER.len();
    let len = text[start..].find('"').ok_or_else(missing)?;
    let found = &text[start..start + len];
    let (old, new) = (plan.old.to_string(), plan.new.to_string());
    if found == new {
        return Err(ReleaseError::AlreadyBumped {
            file: FILE,
            version: new,
        });
    }
    if found != old {
        return Err(ReleaseError::VersionMismatch {
            file: FILE,
            found: found.to_string(),
            expected: old,
        });
    }
    Ok(format!("{}{new}{}", &text[..start], &text[start + len..]))
}

/// True when a lock entry's `version` belongs to this workspace: the old
/// version itself, or a pre-release of the same `X.Y.Z` (repairs an entry that
/// fell behind).
fn is_workspace_version(v: &str, old: &Version) -> bool {
    if v == old.to_string() {
        return true;
    }
    match Version::parse(v) {
        Ok(p) => !p.is_stable() && (p.major, p.minor, p.patch) == (old.major, old.minor, old.patch),
        Err(_) => false,
    }
}

/// Set every workspace `[[package]]` entry in `Cargo.lock` to the new version.
pub fn bump_cargo_lock(text: &str, plan: &BumpPlan) -> Result<String> {
    let new = plan.new.to_string();
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut out = String::with_capacity(text.len());
    let mut bumped = 0usize;
    let mut i = 0;
    while i < lines.len() {
        let is_entry = lines[i].trim_end() == "[[package]]"
            && lines.get(i + 1).is_some_and(|l| l.starts_with("name = "))
            && lines
                .get(i + 2)
                .is_some_and(|l| l.starts_with("version = \""));
        if is_entry {
            let vline = lines[i + 2];
            let ver = vline
                .trim_end()
                .trim_start_matches("version = \"")
                .trim_end_matches('"');
            out.push_str(lines[i]);
            out.push_str(lines[i + 1]);
            // Workspace path crates carry no `source = ` line; a registry or
            // git dependency at the same version must stay untouched.
            let has_source = lines[i + 3..]
                .iter()
                .take_while(|l| !l.trim().is_empty())
                .any(|l| l.starts_with("source = "));
            if !has_source && is_workspace_version(ver, &plan.old) {
                let nl = &vline[vline.trim_end().len()..];
                out.push_str(&format!("version = \"{new}\"{nl}"));
                bumped += 1;
            } else {
                out.push_str(vline);
            }
            i += 3;
        } else {
            out.push_str(lines[i]);
            i += 1;
        }
    }
    if bumped < MIN_LOCK_ENTRIES {
        return Err(ReleaseError::TooFewLockEntries { found: bumped });
    }
    Ok(out)
}

/// Replace the first `` (`<old>`) `` status marker with `` (`<new>`) ``.
pub fn bump_readme(text: &str, plan: &BumpPlan) -> Result<String> {
    let marker = format!("(`{}`)", plan.old);
    let Some(at) = text.find(&marker) else {
        return Err(ReleaseError::MissingMarker {
            file: "README.md",
            marker,
        });
    };
    Ok(format!(
        "{}(`{}`){}",
        &text[..at],
        plan.new,
        &text[at + marker.len()..]
    ))
}

/// Cut the Unreleased body into a new release section and fix the link footer.
pub fn bump_changelog(text: &str, old: &Version, new: &Version, today: &str) -> Result<String> {
    const FILE: &str = "CHANGELOG.md";
    let new_s = new.to_string();
    let old_s = old.to_string();
    let heading_prefix = format!("## [{new_s}]");
    if text.lines().any(|l| l.starts_with(&heading_prefix)) {
        return Err(ReleaseError::AlreadyBumped {
            file: FILE,
            version: new_s,
        });
    }
    let count = text
        .lines()
        .filter(|l| l.trim_end() == "## [Unreleased]")
        .count();
    if count != 1 {
        return Err(if count == 0 {
            ReleaseError::MissingMarker {
                file: FILE,
                marker: "## [Unreleased]".to_string(),
            }
        } else {
            ReleaseError::AmbiguousUnreleased { count }
        });
    }
    let old_link = format!("[Unreleased]: {COMPARE_BASE}/v{old_s}...HEAD");
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = String::with_capacity(text.len() + 256);
    let mut footer_done = false;
    for line in text.split_inclusive('\n') {
        let body = line.trim_end();
        let nl = &line[body.len()..];
        if body == "## [Unreleased]" {
            out.push_str(line);
            let sep = if nl.is_empty() { eol } else { "" };
            out.push_str(&format!("{sep}{eol}## [{new_s}] — {today}{eol}"));
        } else if body == old_link {
            footer_done = true;
            out.push_str(&format!(
                "[Unreleased]: {COMPARE_BASE}/v{new_s}...HEAD{eol}[{new_s}]: {COMPARE_BASE}/v{old_s}...v{new_s}{nl}"
            ));
        } else {
            out.push_str(line);
        }
    }
    if !footer_done {
        return Err(ReleaseError::MissingMarker {
            file: FILE,
            marker: old_link,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    fn next(from: &str, kind: BumpKind) -> String {
        v(from).next(kind).unwrap().to_string()
    }

    #[test]
    fn parse_and_display_round_trip() {
        for s in ["0.1.0-alpha.62", "1.2.3", "0.1.0-beta.1", "2.0.0-rc.10"] {
            assert_eq!(v(s).to_string(), s);
        }
        assert!(v("1.2.3").is_stable());
        assert!(!v("1.2.3-rc.1").is_stable());
    }

    #[test]
    fn parse_rejects_malformed() {
        for s in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "a.b.c",
            "1.2.x",
            "1..3",
            "1.2.3-",
            "1.2.3-alpha",
            "1.2.3-alpha.",
            "1.2.3-alpha.x",
            "1.2.3-gamma.1",
            "1.2.3-alpha.1x",
            "99999999999.0.0",
            "1.2.3-alpha.99999999999",
        ] {
            assert_eq!(
                Version::parse(s),
                Err(ReleaseError::Malformed(s.to_string())),
                "{s}"
            );
        }
    }

    #[test]
    fn alpha_transitions() {
        assert_eq!(next("0.1.0-alpha.62", BumpKind::Alpha), "0.1.0-alpha.63");
        assert_eq!(next("0.1.0", BumpKind::Alpha), "0.1.1-alpha.1");
        assert!(matches!(
            v("0.1.0-beta.2").next(BumpKind::Alpha),
            Err(ReleaseError::Backwards { .. })
        ));
        assert!(matches!(
            v("0.1.0-rc.2").next(BumpKind::Alpha),
            Err(ReleaseError::Backwards { .. })
        ));
    }

    #[test]
    fn beta_transitions() {
        assert_eq!(next("0.1.0-alpha.62", BumpKind::Beta), "0.1.0-beta.1");
        assert_eq!(next("0.1.0-beta.1", BumpKind::Beta), "0.1.0-beta.2");
        assert!(matches!(
            v("0.1.0-rc.1").next(BumpKind::Beta),
            Err(ReleaseError::Backwards { .. })
        ));
        assert!(matches!(
            v("0.1.0").next(BumpKind::Beta),
            Err(ReleaseError::Backwards { .. })
        ));
    }

    #[test]
    fn rc_transitions() {
        assert_eq!(next("0.1.0-alpha.9", BumpKind::Rc), "0.1.0-rc.1");
        assert_eq!(next("0.1.0-beta.3", BumpKind::Rc), "0.1.0-rc.1");
        assert_eq!(next("0.1.0-rc.1", BumpKind::Rc), "0.1.0-rc.2");
        assert!(matches!(
            v("0.1.0").next(BumpKind::Rc),
            Err(ReleaseError::Backwards { .. })
        ));
    }

    #[test]
    fn patch_transitions() {
        assert_eq!(next("0.1.0-alpha.62", BumpKind::Patch), "0.1.0");
        assert_eq!(next("0.1.0-beta.2", BumpKind::Patch), "0.1.0");
        assert_eq!(next("0.1.0-rc.2", BumpKind::Patch), "0.1.0");
        assert_eq!(next("0.1.0", BumpKind::Patch), "0.1.1");
    }

    #[test]
    fn plan_propagates_refusal() {
        assert!(BumpPlan::new(v("0.1.0-rc.1"), BumpKind::Beta, "2026-11-11").is_err());
        let p = BumpPlan::new(v("0.1.0-alpha.1"), BumpKind::Alpha, "2026-11-11").unwrap();
        assert_eq!(p.new.to_string(), "0.1.0-alpha.2");
        assert_eq!(p.today, "2026-11-11");
    }

    #[test]
    fn error_display_covers_every_variant() {
        let errs = [
            ReleaseError::Malformed("x".into()),
            ReleaseError::Backwards {
                from: "1.0.0".into(),
                kind: "beta",
            },
            ReleaseError::MissingMarker {
                file: "f",
                marker: "m".into(),
            },
            ReleaseError::AmbiguousUnreleased { count: 2 },
            ReleaseError::AlreadyBumped {
                file: "f",
                version: "1".into(),
            },
            ReleaseError::VersionMismatch {
                file: "f",
                found: "a".into(),
                expected: "b".into(),
            },
            ReleaseError::TooFewLockEntries { found: 1 },
        ];
        for e in errs {
            assert!(!e.to_string().is_empty());
            let _: &dyn std::error::Error = &e;
        }
    }

    fn plan() -> BumpPlan {
        BumpPlan::new(v("0.1.0-alpha.62"), BumpKind::Alpha, "2026-11-12").unwrap()
    }

    const TOML: &str = "[workspace]\nmembers = [\"a\"]\n\n[workspace.package]\nversion = \"0.1.0-alpha.62\"\nedition = \"2024\"\n";

    #[test]
    fn cargo_toml_bumps_exactly() {
        assert_eq!(
            bump_cargo_toml(TOML, &plan()).unwrap(),
            "[workspace]\nmembers = [\"a\"]\n\n[workspace.package]\nversion = \"0.1.0-alpha.63\"\nedition = \"2024\"\n"
        );
    }

    #[test]
    fn workspace_version_reads_and_errors() {
        assert_eq!(workspace_version(TOML).unwrap(), v("0.1.0-alpha.62"));
        assert!(matches!(
            workspace_version("nothing"),
            Err(ReleaseError::MissingMarker { .. })
        ));
        assert!(matches!(
            workspace_version("[workspace.package]\nversion = \"x\"\n"),
            Err(ReleaseError::Malformed(_))
        ));
        assert!(matches!(
            workspace_version("[workspace.package]\nversion = \"1"),
            Err(ReleaseError::MissingMarker { .. })
        ));
    }

    #[test]
    fn cargo_toml_errors() {
        assert!(matches!(
            bump_cargo_toml("[package]\nversion = \"1\"\n", &plan()),
            Err(ReleaseError::MissingMarker { .. })
        ));
        assert!(matches!(
            bump_cargo_toml("[workspace.package]\nversion = \"0.1.0-alpha.62", &plan()),
            Err(ReleaseError::MissingMarker { .. })
        ));
        let twice = bump_cargo_toml(TOML, &plan()).unwrap();
        assert!(matches!(
            bump_cargo_toml(&twice, &plan()),
            Err(ReleaseError::AlreadyBumped { .. })
        ));
        let other = TOML.replace("alpha.62", "alpha.50");
        assert!(matches!(
            bump_cargo_toml(&other, &plan()),
            Err(ReleaseError::VersionMismatch { .. })
        ));
    }

    fn lock(versions: &[&str]) -> String {
        let mut s =
            String::from("# This file is automatically @generated by Cargo.\nversion = 4\n");
        for (i, ver) in versions.iter().enumerate() {
            s.push_str(&format!(
                "\n[[package]]\nname = \"c{i}\"\nversion = \"{ver}\"\ndependencies = [\n \"serde\",\n]\n"
            ));
        }
        s
    }

    #[test]
    fn cargo_lock_bumps_workspace_entries_only() {
        let input = lock(&[
            "0.1.0-alpha.62",
            "0.1.0-alpha.62",
            "0.1.0-alpha.62",
            "0.1.0-alpha.62",
            "0.1.0-alpha.61", // lagging entry is repaired
            "1.0.219",        // third party untouched
            "0.2.0-alpha.1",  // different X.Y.Z untouched
            "not a version",
        ]);
        let want = lock(&[
            "0.1.0-alpha.63",
            "0.1.0-alpha.63",
            "0.1.0-alpha.63",
            "0.1.0-alpha.63",
            "0.1.0-alpha.63",
            "1.0.219",
            "0.2.0-alpha.1",
            "not a version",
        ]);
        assert_eq!(bump_cargo_lock(&input, &plan()).unwrap(), want);
    }

    #[test]
    fn cargo_lock_too_few_entries() {
        let input = lock(&["0.1.0-alpha.62", "0.1.0-alpha.62", "1.0.0"]);
        assert_eq!(
            bump_cargo_lock(&input, &plan()),
            Err(ReleaseError::TooFewLockEntries { found: 2 })
        );
    }

    #[test]
    fn cargo_lock_keeps_crlf_and_non_entry_headers() {
        let mut input = lock(&["0.1.0-alpha.62"; 5]).replace('\n', "\r\n");
        input.push_str("[[package]]\r\nname = \"tail\"\r\n"); // truncated entry
        let out = bump_cargo_lock(&input, &plan()).unwrap();
        assert_eq!(out.matches("version = \"0.1.0-alpha.63\"\r\n").count(), 5);
        assert!(out.ends_with("name = \"tail\"\r\n"));
    }

    #[test]
    fn readme_bumps_first_marker_only() {
        let text = "> **Status** (`0.1.0-alpha.62`). more\n\nlater (`0.1.0-alpha.62`)\n";
        assert_eq!(
            bump_readme(text, &plan()).unwrap(),
            "> **Status** (`0.1.0-alpha.63`). more\n\nlater (`0.1.0-alpha.62`)\n"
        );
    }

    #[test]
    fn readme_missing_marker_and_double_run() {
        assert!(matches!(
            bump_readme("no status here\n", &plan()),
            Err(ReleaseError::MissingMarker { .. })
        ));
        let once = bump_readme("(`0.1.0-alpha.62`)", &plan()).unwrap();
        assert!(bump_readme(&once, &plan()).is_err());
    }

    const CL: &str = "# Changelog\n\n## [Unreleased]\n\n### Added\n\n- thing\n\n## [0.1.0-alpha.62] — 2026-11-11\n\n- old\n\n[Unreleased]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.62...HEAD\n[0.1.0-alpha.62]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.61...v0.1.0-alpha.62\n";

    #[test]
    fn changelog_bumps_byte_exact() {
        let p = plan();
        let want = "# Changelog\n\n## [Unreleased]\n\n## [0.1.0-alpha.63] — 2026-11-12\n\n### Added\n\n- thing\n\n## [0.1.0-alpha.62] — 2026-11-11\n\n- old\n\n[Unreleased]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.63...HEAD\n[0.1.0-alpha.63]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.62...v0.1.0-alpha.63\n[0.1.0-alpha.62]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.61...v0.1.0-alpha.62\n";
        assert_eq!(bump_changelog(CL, &p.old, &p.new, &p.today).unwrap(), want);
    }

    #[test]
    fn changelog_refuses_second_run_and_bad_shapes() {
        let p = plan();
        let once = bump_changelog(CL, &p.old, &p.new, &p.today).unwrap();
        assert!(matches!(
            bump_changelog(&once, &p.old, &p.new, &p.today),
            Err(ReleaseError::AlreadyBumped { .. })
        ));
        assert!(matches!(
            bump_changelog("# Changelog\n", &p.old, &p.new, &p.today),
            Err(ReleaseError::MissingMarker { .. })
        ));
        let two = CL.replace("## [0.1.0-alpha.62]", "## [Unreleased]");
        assert_eq!(
            bump_changelog(&two, &p.old, &p.new, &p.today),
            Err(ReleaseError::AmbiguousUnreleased { count: 2 })
        );
        let no_footer = CL.replace("[Unreleased]: ", "[Unrel]: ");
        assert!(matches!(
            bump_changelog(&no_footer, &p.old, &p.new, &p.today),
            Err(ReleaseError::MissingMarker { .. })
        ));
    }

    #[test]
    fn cargo_lock_leaves_sourced_entries_at_stable_old_version() {
        let p = BumpPlan {
            old: v("1.2.3"),
            new: v("1.2.4"),
            today: "2026-11-12".into(),
        };
        let mut input = lock(&["1.2.3"; 5]);
        input.push_str(
            "\n[[package]]\nname = \"thirdparty\"\nversion = \"1.2.3\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"abc\"\n",
        );
        let out = bump_cargo_lock(&input, &p).unwrap();
        assert_eq!(out.matches("version = \"1.2.4\"").count(), 5);
        assert!(out.contains("name = \"thirdparty\"\nversion = \"1.2.3\"\nsource ="));
    }

    #[test]
    fn changelog_crlf_and_no_trailing_newline_succeed() {
        let p = plan();
        let text = format!(
            "# Changelog\r\n## [Unreleased]\r\nbody\r\n[Unreleased]: {COMPARE_BASE}/v{}...HEAD",
            p.old
        );
        let out = bump_changelog(&text, &p.old, &p.new, &p.today).unwrap();
        let want = format!(
            "# Changelog\r\n## [Unreleased]\r\n\r\n## [{n}] — {d}\r\nbody\r\n[Unreleased]: {COMPARE_BASE}/v{n}...HEAD\r\n[{n}]: {COMPARE_BASE}/v{o}...v{n}",
            n = p.new,
            o = p.old,
            d = p.today
        );
        assert_eq!(out, want);
        assert!(!out.replace("\r\n", "").contains('\n'));
        // LF file without a trailing newline on the heading-only path
        let lf = format!(
            "## [Unreleased]\n[Unreleased]: {COMPARE_BASE}/v{}...HEAD",
            p.old
        );
        assert!(
            bump_changelog(&lf, &p.old, &p.new, &p.today)
                .unwrap()
                .contains("\n## [")
        );
    }

    #[test]
    fn changelog_heading_without_trailing_newline() {
        let p = plan();
        let text = "## [Unreleased]";
        // heading present but no footer: refused, after the heading rewrite path ran
        assert!(bump_changelog(text, &p.old, &p.new, &p.today).is_err());
    }

    #[test]
    fn fixture_all_four_files_byte_exact() {
        let p = plan();
        let toml = bump_cargo_toml(TOML, &p).unwrap();
        assert!(toml.contains("version = \"0.1.0-alpha.63\""));
        let lk = bump_cargo_lock(&lock(&["0.1.0-alpha.62"; 6]), &p).unwrap();
        assert_eq!(lk, lock(&["0.1.0-alpha.63"; 6]));
        let rd = bump_readme(
            "> **Status: public alpha** (`0.1.0-alpha.62`). **x86_64\n",
            &p,
        )
        .unwrap();
        assert_eq!(
            rd,
            "> **Status: public alpha** (`0.1.0-alpha.63`). **x86_64\n"
        );
    }
}
