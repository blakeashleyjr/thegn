//! Batteries-included toolchain synthesis (`[toolchain]`).
//!
//! The pure half of the [`crate::envplan::Tier::SynthNix`] tier: a repo that
//! declares nothing but language manifests (package.json, pyproject.toml, …)
//! gets a **synthesized Nix devShell** covering its detected languages instead
//! of crude apt installs. This module maps detected [`Language`]s to nixpkgs
//! package sets ([`packages_for`]), renders a minimal self-contained flake
//! ([`synth_flake`]), and compiles the idempotent in-sandbox `/bin/sh` script
//! that writes + locks + warms it ([`synth_dir_script`]). Pure + unit-tested;
//! nothing here executes.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

use crate::config::{config_enum, config_warn};
use crate::envplan::Language;
use crate::toolchain_activation::ActivationPolicy;

config_enum! {
    /// `[toolchain] mode` — how a languages-only repo gets its toolchain:
    /// - `auto` — synthesize a Nix devShell for the detected languages (the
    ///   batteries-included default; requires Nix to be allowed).
    /// - `nix`  — same as `auto` (explicit spelling).
    /// - `mise` — install the language runtimes via mise instead.
    /// - `off`  — legacy behavior: best-effort native (apt) runtimes.
    pub enum ToolchainMode: "toolchain mode" {
        Auto = "auto", Nix = "nix", Mise = "mise", Off = "off",
    } default = Auto;
}

config_enum! {
    /// `[toolchain.mise] inject` — how a detected mise/asdf declaration is
    /// folded into worktree launches. Environment resolution remains
    /// trust-gated; both `auto` and `env` fall back to shims while pending.
    pub enum MiseInject: "mise injection mode" {
        Auto = "auto", Shims = "shims", Env = "env", Off = "off",
    } default = Auto;
}

/// Mise activation policy nested beneath `[toolchain]`. This is separate from
/// [`ToolchainMode`]: mode owns languages-only provisioning, while injection
/// layers detected declarations into every normal worktree launch.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct MiseConfig {
    pub inject: MiseInject,
}

impl MiseConfig {
    /// Resolve config + trust into provider-independent activation intent.
    pub fn activation_policy(&self, config_approved: bool) -> ActivationPolicy {
        match self.inject {
            MiseInject::Off => ActivationPolicy::Off,
            MiseInject::Shims => ActivationPolicy::Shims,
            MiseInject::Auto | MiseInject::Env if config_approved => ActivationPolicy::Environment,
            MiseInject::Auto | MiseInject::Env => ActivationPolicy::Shims,
        }
    }
}

/// `[toolchain]` — the batteries-included toolchain for languages-only repos.
/// Per-language lists override the built-in nixpkgs package defaults (empty ⇒
/// defaults); `extra` is always appended.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ToolchainConfig {
    pub mode: ToolchainMode,
    /// Activation of detected mise/asdf declarations in worktree launches.
    pub mise: MiseConfig,
    /// nixpkgs packages for Python repos (empty ⇒ `["python3", "uv"]`).
    pub python: Vec<String>,
    /// nixpkgs packages for Node repos (empty ⇒ `["nodejs_22"]`).
    pub node: Vec<String>,
    /// nixpkgs packages for Deno repos (empty ⇒ `["deno"]`).
    pub deno: Vec<String>,
    /// nixpkgs packages for JVM/Scala repos (empty ⇒ `["jdk21", "sbt", "scala-cli"]`).
    pub jvm: Vec<String>,
    /// nixpkgs packages for Go repos (empty ⇒ `["go"]`).
    pub go: Vec<String>,
    /// nixpkgs packages for Ruby repos (empty ⇒ `["ruby"]`).
    pub ruby: Vec<String>,
    /// Extra nixpkgs packages appended for every language mix.
    pub extra: Vec<String>,
}

const MAX_NIX_ATTR_PATH_BYTES: usize = 128;
const MAX_NIX_ATTR_SEGMENTS: usize = 8;
const MAX_SYNTH_PACKAGES: usize = 256;
const MAX_SYNTH_PACKAGE_SCAN: usize = 512;
const MAX_SYNTH_FLAKE_BYTES: usize = 40_000;

/// Parse one bounded nixpkgs attribute path, returning its trimmed form.
pub fn parse_nix_attr_path(raw: &str) -> Result<String, String> {
    let path = raw.trim();
    if path.len() > MAX_NIX_ATTR_PATH_BYTES {
        return Err(format!(
            "package path exceeds {MAX_NIX_ATTR_PATH_BYTES} bytes"
        ));
    }
    if path.is_empty() {
        return Err("empty package path".to_string());
    }
    let segments: Vec<&str> = path.split('.').collect();
    if segments.len() > MAX_NIX_ATTR_SEGMENTS {
        return Err(format!(
            "package path has more than {MAX_NIX_ATTR_SEGMENTS} segments"
        ));
    }
    const KEYWORDS: &[&str] = &[
        "if", "then", "else", "assert", "with", "let", "in", "rec", "inherit", "or",
    ];
    for segment in segments {
        let mut chars = segment.chars();
        let Some(first) = chars.next() else {
            return Err("package path contains an empty segment".to_string());
        };
        if !(first.is_ascii_alphabetic() || first == '_')
            || !chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '\'' | '-'))
        {
            return Err(
                "package path contains characters outside the Nix attribute grammar".to_string(),
            );
        }
        if KEYWORDS.contains(&segment) {
            return Err("package path contains a Nix keyword".to_string());
        }
    }
    Ok(path.to_string())
}

fn push_package_list(
    name: &str,
    values: &[String],
    out: &mut Vec<String>,
    seen: &mut HashSet<String>,
) {
    let mut warning_sent = false;
    for raw in values.iter().take(MAX_SYNTH_PACKAGE_SCAN) {
        if raw.trim().is_empty() {
            continue;
        }
        match parse_nix_attr_path(raw) {
            Ok(package) => {
                if !seen.contains(&package) && out.len() == MAX_SYNTH_PACKAGES {
                    if !warning_sent {
                        config_warn(&format!(
                            "toolchain.{name}: package limit of {MAX_SYNTH_PACKAGES} reached; remaining entries ignored"
                        ));
                    }
                    warning_sent = true;
                    break;
                }
                if seen.insert(package.clone()) {
                    out.push(package);
                }
            }
            Err(reason) => {
                if !warning_sent {
                    let shown = raw.chars().take(48).collect::<String>();
                    config_warn(&format!(
                        "toolchain.{name}: rejected package value {shown:?}: {reason}"
                    ));
                    warning_sent = true;
                }
            }
        }
    }
    if values.len() > MAX_SYNTH_PACKAGE_SCAN && !warning_sent {
        config_warn(&format!(
            "toolchain.{name}: scanned only the first {MAX_SYNTH_PACKAGE_SCAN} entries"
        ));
    }
}

fn language_name(language: &Language) -> &'static str {
    match language {
        Language::Python => "python",
        Language::Node => "node",
        Language::Deno => "deno",
        Language::Jvm => "jvm",
        Language::Go => "go",
        Language::Ruby => "ruby",
        Language::Rust => "rust",
    }
}

fn admit_synth_packages(packages: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(packages.len().min(MAX_SYNTH_PACKAGES));
    let mut seen = HashSet::with_capacity(packages.len().min(MAX_SYNTH_PACKAGES));
    for raw in packages.iter().take(MAX_SYNTH_PACKAGE_SCAN) {
        if let Ok(package) = parse_nix_attr_path(raw)
            && seen.insert(package.clone())
        {
            out.push(package);
            if out.len() == MAX_SYNTH_PACKAGES {
                break;
            }
        }
    }
    out
}

/// The nixpkgs packages to synthesize into the devShell for the detected
/// `languages` under config `tc`: built-in defaults per language (Rust is empty
/// — rustup ships in the base image), replaced wholesale by the matching
/// non-empty override list; deduped, stable order; `extra` appended last.
pub fn packages_for(languages: &[Language], tc: &ToolchainConfig) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for lang in languages {
        let (defaults, over): (&[&str], &[String]) = match lang {
            Language::Python => (&["python3", "uv"], &tc.python),
            Language::Node => (&["nodejs_22"], &tc.node),
            Language::Deno => (&["deno"], &tc.deno),
            Language::Jvm => (&["jdk21", "sbt", "scala-cli"], &tc.jvm),
            Language::Go => (&["go"], &tc.go),
            Language::Ruby => (&["ruby"], &tc.ruby),
            // rustup ships in the base image; nothing to synthesize.
            Language::Rust => (&[], &[]),
        };
        if over.is_empty() {
            let defaults = defaults
                .iter()
                .map(|p| (*p).to_string())
                .collect::<Vec<_>>();
            push_package_list(language_name(lang), &defaults, &mut out, &mut seen);
        } else {
            push_package_list(language_name(lang), over, &mut out, &mut seen);
        }
    }
    push_package_list("extra", &tc.extra, &mut out, &mut seen);
    out
}

/// Render the self-contained `flake.nix` exposing `devShells.<system>.default`
/// = a `mkShell` with `packages`. Deterministic over its input (golden-tested):
/// the same package list always renders byte-identical output, so the
/// idempotence `cmp` in [`synth_dir_script`] holds across re-provisions.
/// Invalid paths and entries beyond the bounded admission limit are omitted.
pub fn synth_flake(packages: &[String]) -> String {
    let admitted = admit_synth_packages(packages);
    let list = admitted
        .iter()
        .map(|p| format!("pkgs.{p}"))
        .collect::<Vec<_>>()
        .join(" ");
    let flake = format!(
        "{{\n  \
           description = \"thegn synthesized toolchain\";\n  \
           inputs.nixpkgs.url = \"github:NixOS/nixpkgs/nixos-unstable\";\n  \
           outputs = {{ self, nixpkgs }}:\n    \
             let\n      \
               systems = [ \"x86_64-linux\" \"aarch64-linux\" \"x86_64-darwin\" \"aarch64-darwin\" ];\n      \
               forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${{system}});\n    \
             in {{\n      \
               devShells = forAll (pkgs: {{\n        \
                 default = pkgs.mkShell {{ packages = [ {list} ]; }};\n      \
               }});\n    \
             }};\n\
         }}\n"
    );
    debug_assert!(flake.len() <= MAX_SYNTH_FLAKE_BYTES);
    flake
}

/// The stable synth directory for this generated flake: a versioned SHA-256
/// digest of its exact bytes at `$HOME/.thegn/synth/v2-<digest>`. A literal sh
/// string — `$HOME` expands in the sandbox (used verbatim by the pane-entry
/// hook). The flake bytes include the nixpkgs input and system matrix.
pub fn synth_dir(packages: &[String]) -> String {
    let admitted = admit_synth_packages(packages);
    format!("$HOME/.thegn/synth/{}", synth_hash(&admitted))
}

fn synth_hash(packages: &[String]) -> String {
    let flake = synth_flake(packages);
    let mut digest = Sha256::new();
    digest.update(b"thegn-synth-v2\0");
    digest.update(flake.as_bytes());
    format!("v2-{:x}", digest.finalize())
}

/// The in-sandbox `/bin/sh` script that materializes + warms the synthesized
/// devShell: write the flake to [`synth_dir`] (skipped via `cmp` when an
/// identical one already exists — idempotent, and a content change invalidates
/// the stale `flake.lock`), `nix flake lock` it if needed, then warm with
/// `nix develop path:<dir> --command true`. Materialization and warm failures
/// are returned to the provisioning caller as nonzero script outcomes.
pub fn synth_dir_script(packages: &[String]) -> String {
    let dir = synth_dir(packages);
    let flake = synth_flake(packages);
    let digest = synth_hash(&admit_synth_packages(packages));
    format!(
        "{prelude}[ -r \"$HOME/.nix-profile/etc/profile.d/nix.sh\" ] && . \"$HOME/.nix-profile/etc/profile.d/nix.sh\" 2>/dev/null || true; \
         [ -r /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh ] && . /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh 2>/dev/null || true; \
         export PATH=\"$HOME/.nix-profile/bin:/nix/var/nix/profiles/default/bin:$PATH\"; \
         fail() {{ echo \"$1\"; echo \"$1\" >&2; exit 1; }}; \
         d=\"{dir}\"; digest=\"{digest}\"; \
         [ ! -L \"$HOME/.thegn\" ] && [ ! -L \"$HOME/.thegn/synth\" ] && [ ! -L \"$d\" ] || fail 'thegn-synth: symlinked cache path'; \
         mkdir -p \"$d\" || fail 'thegn-synth: mkdir failed'; \
         [ ! -L \"$HOME/.thegn\" ] && [ ! -L \"$HOME/.thegn/synth\" ] && [ ! -L \"$d\" ] || fail 'thegn-synth: cache path changed'; \
         lk=\"$HOME/.thegn/synth/.{digest}.lock\"; \
         if command -v flock >/dev/null 2>&1; then \
           [ ! -L \"$lk\" ] || fail 'thegn-synth: symlinked lock'; \
           exec 9>>\"$lk\" || fail 'thegn-synth: lock open failed'; \
           [ ! -L \"$lk\" ] && [ \"$lk\" -ef /dev/fd/9 ] || fail 'thegn-synth: lock path changed'; \
           flock -w 900 9 || fail 'thegn-synth: lock busy'; \
         else echo 'thegn-synth: flock unavailable; running unserialized' >&2; fi; \
         ready=\"$d/.thegn-ready\"; \
         manifest=\"$d/thegn-synth.digest\"; \
         if [ -e \"$manifest\" ] || [ -L \"$manifest\" ]; then \
           [ ! -L \"$manifest\" ] && [ -f \"$manifest\" ] && [ \"$(cat \"$manifest\")\" = \"$digest\" ] || fail 'thegn-synth: manifest mismatch'; \
         fi; \
         tmp=$(mktemp \"$d/.flake.nix.XXXXXX\") || fail 'thegn-synth: staging failed'; \
         cat > \"$tmp\" <<'THEGN_SYNTH_FLAKE' || fail 'thegn-synth: flake write failed'\n{flake}THEGN_SYNTH_FLAKE\n\
         if [ -f \"$d/flake.nix\" ] && cmp -s \"$tmp\" \"$d/flake.nix\"; then \
           rm -f \"$tmp\" || fail 'thegn-synth: staging cleanup failed'; \
         else \
           rm -f \"$ready\" || fail 'thegn-synth: ready reset failed'; \
           mv \"$tmp\" \"$d/flake.nix\" || fail 'thegn-synth: flake publish failed'; \
           rm -f \"$d/flake.lock\" || fail 'thegn-synth: stale lock removal failed'; \
         fi; \
         [ -f \"$d/flake.lock\" ] || {{ rm -f \"$ready\" || fail 'thegn-synth: ready reset failed'; nix flake lock \"$d\" 2>/dev/null || fail 'thegn-synth: lock failed'; }}; \
         manifest_tmp=$(mktemp \"$d/.thegn-synth.digest.XXXXXX\") || fail 'thegn-synth: manifest staging failed'; \
         printf '%s' \"$digest\" > \"$manifest_tmp\" || fail 'thegn-synth: manifest write failed'; \
         mv \"$manifest_tmp\" \"$manifest\" || fail 'thegn-synth: manifest publish failed'; \
         nix develop \"path:$d\" --command true 2>/dev/null || fail 'thegn-synth: warm failed'; \
         ready_tmp=$(mktemp \"$d/.thegn-ready.XXXXXX\") || fail 'thegn-synth: ready staging failed'; \
         printf '%s' \"$digest\" > \"$ready_tmp\" || fail 'thegn-synth: ready write failed'; \
         mv \"$ready_tmp\" \"$ready\" || fail 'thegn-synth: ready publish failed'",
        digest = digest,
        prelude = crate::envplan::nix_runtime_prelude(),
    )
}

/// Shell condition used by pane entry to accept only a completed generation.
/// No marker (or a different digest) means cold.
pub fn synth_ready_test(packages: &[String]) -> String {
    let admitted = admit_synth_packages(packages);
    let dir = synth_dir(&admitted);
    let digest = synth_hash(&admitted);
    format!(
        "[ -f {dir}/.thegn-ready ] && [ ! -L {dir}/.thegn-ready ] && [ \"$(cat {dir}/.thegn-ready)\" = \"{digest}\" ]"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn mode_parses_canonically_and_defaults_to_auto() {
        assert_eq!(ToolchainMode::default(), ToolchainMode::Auto);
        assert_eq!(
            ToolchainMode::from_str_validated("mise").unwrap(),
            ToolchainMode::Mise
        );
        assert_eq!(
            ToolchainMode::from_str_validated("NIX").unwrap(),
            ToolchainMode::Nix
        );
        assert_eq!(
            ToolchainMode::from_str_validated("off").unwrap(),
            ToolchainMode::Off
        );
        assert!(ToolchainMode::from_str_validated("bogus").is_err());
        assert_eq!(ToolchainMode::Auto.as_str(), "auto");
    }

    #[test]
    fn config_default_is_all_empty_auto() {
        let tc = ToolchainConfig::default();
        assert_eq!(tc.mode, ToolchainMode::Auto);
        assert_eq!(tc.mise.inject, MiseInject::Auto);
        assert!(tc.python.is_empty() && tc.node.is_empty() && tc.deno.is_empty());
        assert!(tc.jvm.is_empty() && tc.go.is_empty() && tc.ruby.is_empty());
        assert!(tc.extra.is_empty());
    }

    #[test]
    fn config_deserializes_from_toml() {
        let tc: ToolchainConfig = toml::from_str(
            r#"
            mode = "mise"
            python = ["python312"]
            extra = ["just"]
            [mise]
            inject = "env"
            "#,
        )
        .unwrap();
        assert_eq!(tc.mode, ToolchainMode::Mise);
        assert_eq!(tc.python, v(&["python312"]));
        assert_eq!(tc.extra, v(&["just"]));
        assert_eq!(tc.mise.inject, MiseInject::Env);
        assert!(tc.node.is_empty());
    }

    #[test]
    fn mise_inject_modes_resolve_trust_without_an_ambient_override() {
        let policy = |inject, approved| MiseConfig { inject }.activation_policy(approved);
        assert_eq!(policy(MiseInject::Auto, false), ActivationPolicy::Shims);
        assert_eq!(
            policy(MiseInject::Auto, true),
            ActivationPolicy::Environment
        );
        assert_eq!(policy(MiseInject::Shims, true), ActivationPolicy::Shims);
        assert_eq!(policy(MiseInject::Env, false), ActivationPolicy::Shims);
        assert_eq!(policy(MiseInject::Env, true), ActivationPolicy::Environment);
        assert_eq!(policy(MiseInject::Off, true), ActivationPolicy::Off);
        assert_eq!(MiseInject::default(), MiseInject::Auto);
        assert!(MiseInject::from_str_validated("bogus").is_err());
    }

    #[test]
    fn packages_for_default_map_per_language() {
        let tc = ToolchainConfig::default();
        assert_eq!(
            packages_for(&[Language::Python], &tc),
            v(&["python3", "uv"])
        );
        assert_eq!(packages_for(&[Language::Node], &tc), v(&["nodejs_22"]));
        assert_eq!(packages_for(&[Language::Deno], &tc), v(&["deno"]));
        assert_eq!(
            packages_for(&[Language::Jvm], &tc),
            v(&["jdk21", "sbt", "scala-cli"])
        );
        assert_eq!(packages_for(&[Language::Go], &tc), v(&["go"]));
        assert_eq!(packages_for(&[Language::Ruby], &tc), v(&["ruby"]));
        // Rust ships rustup in the base image — nothing synthesized.
        assert!(packages_for(&[Language::Rust], &tc).is_empty());
    }

    #[test]
    fn packages_for_stable_order_and_dedup() {
        let tc = ToolchainConfig {
            extra: v(&["uv", "just"]), // `uv` dups the python default
            ..Default::default()
        };
        let got = packages_for(&[Language::Python, Language::Node, Language::Rust], &tc);
        assert_eq!(got, v(&["python3", "uv", "nodejs_22", "just"]));
    }

    #[test]
    fn packages_for_override_replaces_defaults() {
        let tc = ToolchainConfig {
            node: v(&["nodejs_20", "pnpm"]),
            ..Default::default()
        };
        let got = packages_for(&[Language::Node], &tc);
        assert_eq!(got, v(&["nodejs_20", "pnpm"]));
        // Other languages keep their defaults alongside the override.
        let mix = packages_for(&[Language::Node, Language::Go], &tc);
        assert_eq!(mix, v(&["nodejs_20", "pnpm", "go"]));
    }

    #[test]
    fn packages_for_skips_blank_entries() {
        let tc = ToolchainConfig {
            extra: v(&["", "  ", "just"]),
            ..Default::default()
        };
        let (packages, warnings, _) =
            crate::config_diagnostics::capture_quiet(|| packages_for(&[], &tc));
        assert_eq!(packages, v(&["just"]));
        assert!(warnings.is_empty());
    }

    #[test]
    fn nix_attr_path_accepts_bounded_package_identifiers_and_dotted_sets() {
        assert_eq!(
            parse_nix_attr_path("  nodePackages.typescript  ").unwrap(),
            "nodePackages.typescript"
        );
        assert_eq!(
            parse_nix_attr_path("python3Packages.pip").unwrap(),
            "python3Packages.pip"
        );
        assert_eq!(parse_nix_attr_path("scala-cli").unwrap(), "scala-cli");
        assert_eq!(parse_nix_attr_path("foo'bar_2").unwrap(), "foo'bar_2");
        assert!(parse_nix_attr_path("").is_err());
        assert!(parse_nix_attr_path(".a").is_err());
        assert!(parse_nix_attr_path("a.").is_err());
        assert!(parse_nix_attr_path("a..b").is_err());
        assert!(parse_nix_attr_path("a.b.c.d.e.f.g.h").is_ok());
        assert!(parse_nix_attr_path("a.b.c.d.e.f.g.h.i").is_err());
        assert!(parse_nix_attr_path(&"a".repeat(MAX_NIX_ATTR_PATH_BYTES)).is_ok());
        assert!(parse_nix_attr_path(&"a".repeat(MAX_NIX_ATTR_PATH_BYTES + 1)).is_err());
        let padded = format!("{}a{}", " ".repeat(MAX_NIX_ATTR_PATH_BYTES), " ");
        assert_eq!(parse_nix_attr_path(&padded).unwrap(), "a");
        for keyword in [
            "if", "then", "else", "assert", "with", "let", "in", "rec", "inherit", "or",
        ] {
            assert!(parse_nix_attr_path(keyword).is_err(), "{keyword}");
        }
        assert!(parse_nix_attr_path("a.or").is_err());
        for invalid in [
            "a b", "a;b", "a]", "a${x}", "\"a\"", "a#c", "a/*", "1a", "a+", "é",
        ] {
            assert!(parse_nix_attr_path(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn packages_for_rejects_expression_syntax_and_keeps_valid_dotted_paths() {
        let tc = ToolchainConfig {
            extra: v(&[
                "a b",
                "a;b",
                "a]",
                "a${x}",
                "\"a\"",
                "a#c",
                "a/*",
                "a..b",
                ".a",
                "a.",
                "if",
                "a.or",
                "é",
                "nodePackages.typescript",
                "python3Packages.pip",
            ]),
            ..Default::default()
        };
        let got = packages_for(&[], &tc);
        assert_eq!(got, v(&["nodePackages.typescript", "python3Packages.pip"]));
        let flake = synth_flake(&tc.extra);
        assert!(flake.contains("pkgs.nodePackages.typescript"));
        assert!(flake.contains("pkgs.python3Packages.pip"));
        for bad in ["pkgs.a b", "pkgs.a;", "pkgs.a${x}", "pkgs.\"a\""] {
            assert!(!flake.contains(bad), "{bad}");
        }
    }

    #[test]
    fn package_and_render_limits_are_enforced_at_both_boundaries() {
        let mut items = (0..MAX_SYNTH_PACKAGES)
            .map(|i| format!("pkg_{i}"))
            .collect::<Vec<_>>();
        let config = ToolchainConfig {
            extra: items.clone(),
            ..Default::default()
        };
        assert_eq!(packages_for(&[], &config).len(), MAX_SYNTH_PACKAGES);
        items.push("one_too_many".to_string());
        let config = ToolchainConfig {
            extra: items.clone(),
            ..Default::default()
        };
        assert_eq!(packages_for(&[], &config).len(), MAX_SYNTH_PACKAGES);
        assert_eq!(admit_synth_packages(&items).len(), MAX_SYNTH_PACKAGES);
        let rendered = synth_flake(&items);
        assert!(rendered.len() <= MAX_SYNTH_FLAKE_BYTES);
        assert!(rendered.contains("pkgs.pkg_255"));
        assert!(!rendered.contains("pkgs.one_too_many"));
    }

    #[test]
    fn package_scan_stops_at_the_hard_input_bound() {
        let mut items = vec!["bad;value".to_string(); MAX_SYNTH_PACKAGE_SCAN];
        items.push("after_scan_limit".to_string());
        let config = ToolchainConfig {
            extra: items.clone(),
            ..Default::default()
        };
        assert!(packages_for(&[], &config).is_empty());
        assert!(!synth_flake(&items).contains("pkgs.after_scan_limit"));
    }

    #[test]
    fn package_limit_warning_suppresses_scan_warning() {
        let items = (0..=MAX_SYNTH_PACKAGE_SCAN)
            .map(|i| format!("scan_pkg_{i}"))
            .collect::<Vec<_>>();
        let config = ToolchainConfig {
            extra: items,
            ..Default::default()
        };
        let (admitted, warnings, _) =
            crate::config_diagnostics::capture_quiet(|| packages_for(&[], &config));
        assert_eq!(admitted.len(), MAX_SYNTH_PACKAGES);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("package limit"));
    }

    #[test]
    fn synth_directory_and_flake_use_the_same_admitted_package_list() {
        let raw = v(&["safe.path", "evil; injected", "safe.path"]);
        let admitted = v(&["safe.path"]);
        assert_eq!(synth_dir(&raw), synth_dir(&admitted));
        assert_eq!(synth_flake(&raw), synth_flake(&admitted));
    }

    #[test]
    fn synth_flake_golden() {
        let flake = synth_flake(&v(&["python3", "uv"]));
        assert_eq!(
            flake,
            "{\n  description = \"thegn synthesized toolchain\";\n  \
             inputs.nixpkgs.url = \"github:NixOS/nixpkgs/nixos-unstable\";\n  \
             outputs = { self, nixpkgs }:\n    let\n      \
             systems = [ \"x86_64-linux\" \"aarch64-linux\" \"x86_64-darwin\" \"aarch64-darwin\" ];\n      \
             forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});\n    \
             in {\n      devShells = forAll (pkgs: {\n        \
             default = pkgs.mkShell { packages = [ pkgs.python3 pkgs.uv ]; };\n      \
             });\n    };\n}\n"
        );
        // Deterministic: same input, identical output.
        assert_eq!(flake, synth_flake(&v(&["python3", "uv"])));
    }

    #[test]
    fn synth_flake_empty_packages_is_still_valid_shape() {
        let flake = synth_flake(&[]);
        assert!(flake.contains("packages = [  ];"), "{flake}");
        assert!(flake.contains("devShells"));
    }

    #[test]
    fn synth_dir_is_versioned_sha256_of_exact_flake_bytes() {
        let pkgs = v(&["python3", "uv"]);
        let dir = synth_dir(&pkgs);
        let mut hasher = Sha256::new();
        hasher.update(b"thegn-synth-v2\0");
        hasher.update(synth_flake(&pkgs).as_bytes());
        let expected = format!("v2-{:x}", hasher.finalize());
        assert_eq!(dir, format!("$HOME/.thegn/synth/{expected}"));
        let digest = dir.rsplit('/').next().unwrap();
        assert_eq!(digest.len(), 67);
        assert!(digest.starts_with("v2-"));
        assert!(
            digest[3..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
        // A different set lands in a different dir.
        assert_ne!(dir, synth_dir(&v(&["go"])));
    }

    #[test]
    fn synth_dir_script_writes_locks_and_warms_idempotently() {
        let pkgs = v(&["nodejs_22"]);
        let s = synth_dir_script(&pkgs);
        // Targets the hashed synth dir.
        assert!(s.contains(&synth_dir(&pkgs)), "{s}");
        // Embeds the exact flake via a quoted heredoc (no shell expansion).
        assert!(s.contains("<<'THEGN_SYNTH_FLAKE'"), "{s}");
        assert!(s.contains("pkgs.nodejs_22"), "{s}");
        // Idempotent write: identical content is skipped via cmp; a change
        // replaces the flake AND drops the stale lock.
        assert!(s.contains("cmp -s"), "{s}");
        assert!(s.contains("rm -f \"$d/flake.lock\""), "{s}");
        // Lock only when needed, then warm the devShell; failures are visible.
        assert!(
            s.contains("[ -f \"$d/flake.lock\" ] || { rm -f \"$ready\""),
            "{s}"
        );
        assert!(s.contains("nix develop \"path:$d\" --command true"), "{s}");
        assert!(s.contains("mktemp \"$d/.flake.nix.XXXXXX\""), "{s}");
        assert!(s.contains("thegn-synth.digest"), "{s}");
        assert!(s.contains("echo \"$1\"; echo \"$1\" >&2"), "{s}");
        assert!(s.contains("[ ! -L \"$HOME/.thegn\" ]"), "{s}");
        assert!(s.contains("[ ! -L \"$HOME/.thegn/synth\" ]"), "{s}");
        assert!(s.contains("[ ! -L \"$d\" ]"), "{s}");
        assert!(
            s.contains("thegn-synth: mkdir failed") && s.contains("exit 1"),
            "{s}"
        );
        assert!(
            s.contains("thegn-synth: lock failed") && s.contains("exit 1"),
            "{s}"
        );
        assert!(s.contains("thegn-synth: manifest mismatch"), "{s}");
        assert!(
            s.find("nix flake lock").unwrap()
                < s.find("mv \"$manifest_tmp\" \"$manifest\"").unwrap(),
            "{s}"
        );
        assert!(
            s.contains("thegn-synth: warm failed") && s.contains("exit 1"),
            "{s}"
        );
        assert!(
            !s.contains("nix develop \"path:$d\" --command true 2>/dev/null || true"),
            "{s}"
        );
        assert!(!s.trim_end().ends_with("true"), "{s}");
        // Carries the nix runtime prelude (private-input tokens + shelter purge).
        assert!(
            s.contains("NIX_CONFIG") && s.contains("/homeless-shelter"),
            "{s}"
        );
    }

    #[test]
    fn synth_dir_script_serializes_and_marks_ready_last() {
        let s = synth_dir_script(&v(&["nodejs_22"]));
        let mkdir = s.find("mkdir -p \"$d\"").expect("cache mkdir");
        let lock_open = s.find("exec 9>>\"$lk\"").expect("lock open");
        let flake_stage = s.find("mktemp \"$d/.flake.nix").expect("flake staging");
        let ready_reset = s.find("rm -f \"$ready\"").expect("ready reset");
        let flake_publish = s
            .find("mv \"$tmp\" \"$d/flake.nix\"")
            .expect("flake publish");
        let warm = s
            .find("nix develop \"path:$d\" --command true")
            .expect("devshell warm");
        let ready_publish = s
            .find("mv \"$ready_tmp\" \"$ready\"")
            .expect("ready publish");
        assert!(s.contains("flock -w 900 9"), "{s}");
        assert!(mkdir < lock_open && lock_open < flake_stage, "{s}");
        assert!(ready_reset < flake_publish, "{s}");
        assert!(warm < ready_publish, "{s}");
        assert!(s.contains("thegn-synth: flock unavailable"), "{s}");
    }

    #[test]
    fn synth_ready_test_requires_the_exact_digest() {
        let first = v(&["nodejs_22"]);
        let second = v(&["python312"]);
        let first_test = synth_ready_test(&first);
        let second_test = synth_ready_test(&second);
        assert_ne!(first_test, second_test);
        for (packages, condition) in [(&first, &first_test), (&second, &second_test)] {
            let dir = synth_dir(packages);
            let script = synth_dir_script(packages);
            let digest = script
                .split_once("digest=\"")
                .and_then(|(_, tail)| tail.split_once('"'))
                .map(|(digest, _)| digest)
                .expect("script digest assignment");
            assert!(condition.contains(".thegn-ready"), "{condition}");
            assert!(condition.contains(&dir), "{condition}");
            assert!(
                condition.contains(&format!("= \"{digest}\"")),
                "{condition}"
            );
        }
    }
}
