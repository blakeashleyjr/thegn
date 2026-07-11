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

use crate::config::{config_enum, config_warn};
use crate::envplan::Language;

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

/// `[toolchain]` — the batteries-included toolchain for languages-only repos.
/// Per-language lists override the built-in nixpkgs package defaults (empty ⇒
/// defaults); `extra` is always appended.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ToolchainConfig {
    pub mode: ToolchainMode,
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

/// The nixpkgs packages to synthesize into the devShell for the detected
/// `languages` under config `tc`: built-in defaults per language (Rust is empty
/// — rustup ships in the base image), replaced wholesale by the matching
/// non-empty override list; deduped, stable order; `extra` appended last.
pub fn packages_for(languages: &[Language], tc: &ToolchainConfig) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |p: &str| {
        let p = p.trim();
        if !p.is_empty() && !out.iter().any(|x| x == p) {
            out.push(p.to_string());
        }
    };
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
            defaults.iter().for_each(|p| push(p));
        } else {
            over.iter().for_each(|p| push(p));
        }
    }
    for p in &tc.extra {
        push(p);
    }
    out
}

/// Render the self-contained `flake.nix` exposing `devShells.<system>.default`
/// = a `mkShell` with `packages`. Deterministic over its input (golden-tested):
/// the same package list always renders byte-identical output, so the
/// idempotence `cmp` in [`synth_dir_script`] holds across re-provisions.
pub fn synth_flake(packages: &[String]) -> String {
    let list = packages
        .iter()
        .map(|p| format!("pkgs.{p}"))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
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
    )
}

/// The stable synth directory for this package set (12-char [`crate::util::short_hash`]
/// of the joined list): `$HOME/.thegn/synth/<hash>`. A literal sh string —
/// `$HOME` expands in the sandbox (used verbatim by the pane-entry hook).
pub fn synth_dir(packages: &[String]) -> String {
    format!("$HOME/.thegn/synth/{}", synth_hash(packages))
}

fn synth_hash(packages: &[String]) -> String {
    crate::util::short_hash(&packages.join("\n"), 12)
}

/// The in-sandbox `/bin/sh` script that materializes + warms the synthesized
/// devShell: write the flake to [`synth_dir`] (skipped via `cmp` when an
/// identical one already exists — idempotent, and a content change invalidates
/// the stale `flake.lock`), `nix flake lock` it if needed, then warm with
/// `nix develop path:<dir> --command true`. Best-effort like the other warm
/// scripts: the shell still comes up if the warm fails (it builds lazily).
pub fn synth_dir_script(packages: &[String]) -> String {
    let dir = synth_dir(packages);
    let flake = synth_flake(packages);
    format!(
        "{prelude}[ -r \"$HOME/.nix-profile/etc/profile.d/nix.sh\" ] && . \"$HOME/.nix-profile/etc/profile.d/nix.sh\" 2>/dev/null || true; \
         [ -r /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh ] && . /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh 2>/dev/null || true; \
         export PATH=\"$HOME/.nix-profile/bin:/nix/var/nix/profiles/default/bin:$PATH\"; \
         d=\"{dir}\"; mkdir -p \"$d\"; \
         tmp=\"$d/.flake.nix.tmp\"; \
         cat > \"$tmp\" <<'THEGN_SYNTH_FLAKE'\n{flake}THEGN_SYNTH_FLAKE\n\
         if [ -f \"$d/flake.nix\" ] && cmp -s \"$tmp\" \"$d/flake.nix\"; then rm -f \"$tmp\"; \
         else mv \"$tmp\" \"$d/flake.nix\"; rm -f \"$d/flake.lock\"; fi; \
         [ -f \"$d/flake.lock\" ] || nix flake lock \"$d\" 2>/dev/null || true; \
         nix develop \"path:$d\" --command true 2>/dev/null || true; true",
        prelude = crate::envplan::nix_runtime_prelude(),
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
            "#,
        )
        .unwrap();
        assert_eq!(tc.mode, ToolchainMode::Mise);
        assert_eq!(tc.python, v(&["python312"]));
        assert_eq!(tc.extra, v(&["just"]));
        assert!(tc.node.is_empty());
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
        assert_eq!(packages_for(&[], &tc), v(&["just"]));
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
    fn synth_dir_is_stable_hash_of_package_list() {
        let pkgs = v(&["python3", "uv"]);
        let dir = synth_dir(&pkgs);
        let hash = crate::util::short_hash("python3\nuv", 12);
        assert_eq!(dir, format!("$HOME/.thegn/synth/{hash}"));
        assert_eq!(hash.len(), 12);
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
        // Lock only when needed, then warm the devShell.
        assert!(
            s.contains("[ -f \"$d/flake.lock\" ] || nix flake lock"),
            "{s}"
        );
        assert!(s.contains("nix develop \"path:$d\" --command true"), "{s}");
        // Carries the nix runtime prelude (private-input tokens + shelter purge).
        assert!(
            s.contains("NIX_CONFIG") && s.contains("/homeless-shelter"),
            "{s}"
        );
        // Best-effort: the step never hard-fails provisioning.
        assert!(s.trim_end().ends_with("true"), "{s}");
    }
}
