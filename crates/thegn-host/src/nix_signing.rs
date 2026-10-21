//! Per-installation signing key for host-parity Nix closure pushes (THE-427).

use anyhow::{Context, anyhow};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub(crate) struct InstallKey {
    pub(crate) secret: PathBuf,
    pub(crate) public: String,
}

pub(crate) fn ensure_install_key() -> anyhow::Result<InstallKey> {
    let config = thegn_core::config::Config::path();
    let dir = config
        .parent()
        .map(|parent| parent.join("nix-signing"))
        .context("config path has no parent for Nix signing key")?;
    load_or_create_in(&dir, generate_key, convert_secret_to_public)
}

fn load_or_create_in(
    dir: &Path,
    mut generator: impl FnMut() -> anyhow::Result<(String, String)>,
    converter: impl Fn(&str) -> anyhow::Result<String>,
) -> anyhow::Result<InstallKey> {
    thegn_core::fsperm::ensure_private_dir(dir)
        .with_context(|| format!("prepare private Nix signing directory {}", dir.display()))?;
    let secret = dir.join("secret-key");
    let public = dir.join("public-key");
    if secret.exists() {
        if public.exists() {
            return load_pair(&secret, &public);
        }
        let key = load_secret(&secret)?;
        let public_line = converter(&key)?;
        validate_pair(&key, &public_line)?;
        thegn_core::fsperm::write_owner_only_new(&public, public_line.as_bytes())
            .with_context(|| format!("restore Nix signing public key {}", public.display()))?;
        return load_pair(&secret, &public);
    }
    if public.exists() {
        // A prior attempt may have stopped after publishing the public key.
        // Validate it without following links, then replace it with the pair
        // produced below. The directory is private and owner-validated above.
        let _existing_public = load_public(&public)?;
        std::fs::remove_file(&public).with_context(|| {
            format!(
                "remove incomplete Nix signing public key {}",
                public.display()
            )
        })?;
    }

    let (secret_line, public_line) = generator()?;
    validate_pair(&secret_line, &public_line)?;

    match thegn_core::fsperm::write_owner_only_new(&public, public_line.as_bytes()) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return load_pair(&secret, &public);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("write Nix signing public key {}", public.display()));
        }
    }
    write_secret_atomic(&secret, secret_line.as_bytes())?;
    load_pair(&secret, &public)
}

fn write_secret_atomic(secret: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random).context("generate temporary Nix signing key name")?;
    let suffix = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let temporary =
        secret.with_file_name(format!("secret-key.tmp-{}-{suffix}", std::process::id()));
    let mut file =
        thegn_core::fsperm::create_owner_only_exclusive(&temporary).with_context(|| {
            format!(
                "create temporary Nix signing secret {}",
                temporary.display()
            )
        })?;
    if let Err(error) =
        std::io::Write::write_all(&mut file, contents).and_then(|()| file.sync_all())
    {
        std::fs::remove_file(&temporary).with_context(|| {
            format!(
                "remove incomplete temporary Nix signing secret {}",
                temporary.display()
            )
        })?;
        return Err(error).with_context(|| {
            format!("write temporary Nix signing secret {}", temporary.display())
        });
    }
    std::fs::rename(&temporary, secret)
        .with_context(|| format!("atomically install Nix signing secret {}", secret.display()))
}

fn validate_pair(secret: &str, public: &str) -> anyhow::Result<()> {
    if parse_public_key(secret).is_none() {
        return Err(anyhow!("Nix generated a malformed secret signing key"));
    }
    if parse_public_key(public).is_none() {
        return Err(anyhow!("Nix generated a malformed public signing key"));
    }
    if key_name(secret) != key_name(public) {
        return Err(anyhow!("Nix generated signing keys with different names"));
    }
    Ok(())
}

fn load_secret(secret: &Path) -> anyhow::Result<String> {
    let mut file = thegn_core::fsperm::open_regular_file_nofollow(secret)
        .with_context(|| format!("open Nix signing secret {}", secret.display()))?;
    let metadata = file.metadata()?;
    if !thegn_core::fsperm::owned_by_current_user(&metadata) {
        return Err(anyhow!(
            "Nix signing secret is not owned by the current user"
        ));
    }
    thegn_core::fsperm::ensure_single_link_regular_file(&metadata)
        .context("Nix signing secret has multiple hardlinks")?;
    if thegn_core::fsperm::mode_bits(secret)?.is_some_and(|mode| mode & 0o077 != 0) {
        return Err(anyhow!("Nix signing secret is group- or other-readable"));
    }
    let mut secret_line = String::new();
    file.read_to_string(&mut secret_line)?;
    let secret_line = secret_line.trim_end_matches(['\r', '\n']);
    if parse_public_key(secret_line).is_none() {
        return Err(anyhow!("Nix signing secret has an invalid key line"));
    }
    Ok(secret_line.to_string())
}

fn load_public(public: &Path) -> anyhow::Result<String> {
    let mut file = thegn_core::fsperm::open_regular_file_nofollow(public)
        .with_context(|| format!("open Nix signing public key {}", public.display()))?;
    let mut public_line = String::new();
    file.read_to_string(&mut public_line)?;
    let public_line = public_line.trim_end_matches(['\r', '\n']);
    if parse_public_key(public_line).is_none() {
        return Err(anyhow!("Nix signing public key has an invalid key line"));
    }
    Ok(public_line.to_string())
}

fn load_pair(secret: &Path, public: &Path) -> anyhow::Result<InstallKey> {
    let secret_line = load_secret(secret)?;
    let public_line = load_public(public)?;
    if key_name(&secret_line) != key_name(&public_line) {
        return Err(anyhow!(
            "Nix signing secret and public keys do not share a key name"
        ));
    }
    Ok(InstallKey {
        secret: secret.to_path_buf(),
        public: public_line,
    })
}

#[expect(
    clippy::disallowed_methods,
    reason = "local Nix key generation must wait for its child"
)]
fn generate_key() -> anyhow::Result<(String, String)> {
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random).context("generate Nix signing key name")?;
    let suffix = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let name = format!("thegn-{suffix}-1");
    let output = Command::new("nix")
        .args([
            "--extra-experimental-features",
            "nix-command",
            "key",
            "generate-secret",
            "--key-name",
            &name,
        ])
        .output()
        .context("spawn nix key generate-secret")?;
    if !output.status.success() {
        return Err(anyhow!(
            "nix key generate-secret failed: {}",
            stderr_tail(&output.stderr)
        ));
    }
    let secret = String::from_utf8(output.stdout).context("Nix secret key output is not UTF-8")?;
    let public = convert_secret_to_public(&secret)?;
    Ok((secret.trim().to_string(), public.trim().to_string()))
}

#[expect(
    clippy::disallowed_methods,
    reason = "local Nix key conversion must wait for its child"
)]
fn convert_secret_to_public(secret: &str) -> anyhow::Result<String> {
    let mut child = Command::new("nix")
        .args([
            "--extra-experimental-features",
            "nix-command",
            "key",
            "convert-secret-to-public",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn nix key convert-secret-to-public")?;
    let input_result = match child.stdin.take() {
        Some(mut stdin) => std::io::Write::write_all(&mut stdin, secret.as_bytes()),
        None => Err(std::io::Error::other("open Nix key conversion stdin")),
    };
    let output = child.wait_with_output()?;
    input_result?;
    if !output.status.success() {
        return Err(anyhow!(
            "nix key convert-secret-to-public failed: {}",
            stderr_tail(&output.stderr)
        ));
    }
    String::from_utf8(output.stdout).context("Nix public key output is not UTF-8")
}

fn stderr_tail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    text.lines()
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn sign_argv(secret: &Path, roots: &[String]) -> Vec<String> {
    let mut argv = vec![
        "--extra-experimental-features".into(),
        "nix-command".into(),
        "store".into(),
        "sign".into(),
        "--key-file".into(),
        secret.to_string_lossy().into_owned(),
        "--recursive".into(),
    ];
    argv.extend(roots.iter().cloned());
    argv
}

fn trusted_key_conf_line(public: &str) -> String {
    format!("extra-trusted-public-keys = {public}")
}

pub(crate) fn trust_script(public: &str) -> String {
    let line = trusted_key_conf_line(public);
    let quoted = thegn_core::util::sh_quote(&line);
    format!(
        "mkdir -p \"$HOME/.config/nix\"\nconfig=\"$HOME/.config/nix/nix.conf\"\n[ -f \"$config\" ] || : > \"$config\"\ngrep -qxF -- {quoted} \"$config\" || printf '%s\\n' {quoted} >> \"$config\"\n"
    )
}

fn parse_public_key(line: &str) -> Option<&str> {
    let (name, key) = line.split_once(':')?;
    let suffix = key_name_suffix(name)?;
    if suffix.len() != 16
        || !suffix
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return None;
    }
    let padding = key.bytes().rev().take_while(|byte| *byte == b'=').count();
    if key.is_empty()
        || !key.len().is_multiple_of(4)
        || padding > 2
        || !key[..key.len().saturating_sub(padding)]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/'))
    {
        return None;
    }
    Some(key)
}

fn key_name(line: &str) -> Option<&str> {
    let (name, _) = line.split_once(':')?;
    key_name_suffix(name).map(|_| name)
}

pub(crate) fn install_key_name(key: &InstallKey) -> Option<String> {
    key_name(&key.public).map(str::to_owned)
}

fn key_name_suffix(name: &str) -> Option<&str> {
    name.strip_prefix("thegn-")?.strip_suffix("-1")
}

#[cfg(test)]
mod tests {
    use super::{
        InstallKey, install_key_name, load_or_create_in, parse_public_key, sign_argv, trust_script,
        trusted_key_conf_line,
    };
    use std::path::Path;

    const SECRET: &str = "thegn-0123456789abcdef-1:c2VjcmV0";
    const PUBLIC: &str = "thegn-0123456789abcdef-1:cHVibGlj";

    #[test]
    fn install_key_name_is_the_public_key_prefix() {
        let key = InstallKey {
            secret: Path::new("/private/secret-key").to_path_buf(),
            public: PUBLIC.to_string(),
        };
        assert_eq!(
            install_key_name(&key).as_deref(),
            Some("thegn-0123456789abcdef-1")
        );
    }

    fn private_dir(path: &Path) {
        thegn_core::fsperm::ensure_private_dir(path).unwrap();
    }

    fn write_pair(dir: &Path) {
        private_dir(dir);
        thegn_core::fsperm::write_owner_only_new(&dir.join("secret-key"), SECRET.as_bytes())
            .unwrap();
        thegn_core::fsperm::write_owner_only_new(&dir.join("public-key"), PUBLIC.as_bytes())
            .unwrap();
    }

    #[test]
    fn sign_argv_signs_recursively_with_the_key_file() {
        assert_eq!(
            sign_argv(
                Path::new("/private/secret-key"),
                &["/nix/store/a-foo".into()]
            ),
            vec![
                "--extra-experimental-features".to_string(),
                "nix-command".to_string(),
                "store".to_string(),
                "sign".to_string(),
                "--key-file".to_string(),
                "/private/secret-key".to_string(),
                "--recursive".to_string(),
                "/nix/store/a-foo".to_string(),
            ]
        );
    }

    #[test]
    fn trust_script_appends_the_key_once() {
        #[expect(
            clippy::disallowed_methods,
            reason = "exercise the generated shell trust script"
        )]
        fn run(script: &str) {
            let output = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(script)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let state = thegn_core::testenv::IsolatedState::new();
        let script = trust_script(PUBLIC);
        run(&script);
        run(&script);
        let contents = std::fs::read_to_string(state.root().join(".config/nix/nix.conf")).unwrap();
        assert_eq!(contents.lines().count(), 1);
        assert_eq!(contents.trim(), trusted_key_conf_line(PUBLIC));
    }

    #[test]
    fn parse_public_key_rejects_malformed_lines() {
        for line in [
            "other-0123456789abcdef-1:cHVibGlj",
            "thegn-0123456789abcde-1:cHVibGlj",
            "thegn-0123456789abcdeg-1:cHVibGlj",
            "thegn-0123456789abcdef-2:cHVibGlj",
            "thegn-0123456789abcdef-1:",
            "thegn-0123456789abcdef-1:not base64",
            "thegn-0123456789abcdef-1:cHVibGlj:extra",
        ] {
            assert!(parse_public_key(line).is_none(), "accepted {line}");
        }
        assert_eq!(parse_public_key(PUBLIC), Some("cHVibGlj"));
    }

    #[test]
    fn existing_key_files_are_reused() {
        let state = thegn_core::testenv::IsolatedState::new();
        let dir = state.root().join("keys");
        write_pair(&dir);
        let actual = load_or_create_in(
            &dir,
            || panic!("existing key must be reused"),
            |_| panic!("existing public key must be reused"),
        )
        .unwrap();
        assert_eq!(actual.secret, dir.join("secret-key"));
        assert_eq!(actual.public, PUBLIC);
    }

    #[test]
    fn symlinked_secret_is_refused() {
        let state = thegn_core::testenv::IsolatedState::new();
        let dir = state.root().join("keys");
        write_pair(&dir);
        let secret = dir.join("secret-key");
        std::fs::remove_file(&secret).unwrap();
        let target = state.root().join("outside-secret");
        std::fs::write(&target, SECRET).unwrap();
        if cfg!(unix) {
            #[expect(
                clippy::disallowed_methods,
                reason = "create the symlink fixture with the platform tool"
            )]
            fn make_symlink(target: &Path, link: &Path) {
                let status = std::process::Command::new("ln")
                    .arg("-s")
                    .arg(target)
                    .arg(link)
                    .status()
                    .unwrap();
                assert!(status.success());
            }
            make_symlink(&target, &secret);
            assert!(
                load_or_create_in(
                    &dir,
                    || Ok((SECRET.into(), PUBLIC.into())),
                    |_| Ok(PUBLIC.into()),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn group_readable_secret_is_refused() {
        let state = thegn_core::testenv::IsolatedState::new();
        let dir = state.root().join("keys");
        write_pair(&dir);
        let secret = dir.join("secret-key");
        if cfg!(unix) {
            #[expect(
                clippy::disallowed_methods,
                reason = "make the secret fixture group-readable"
            )]
            fn chmod_group_readable(path: &Path) {
                let status = std::process::Command::new("chmod")
                    .arg("0640")
                    .arg(path)
                    .status()
                    .unwrap();
                assert!(status.success());
            }
            chmod_group_readable(&secret);
            assert!(
                load_or_create_in(
                    &dir,
                    || Ok((SECRET.into(), PUBLIC.into())),
                    |_| Ok(PUBLIC.into()),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn missing_key_is_generated_once_and_kept() {
        let state = thegn_core::testenv::IsolatedState::new();
        let dir = state.root().join("keys");
        let mut calls = 0;
        let first = load_or_create_in(
            &dir,
            || {
                calls += 1;
                Ok((SECRET.into(), PUBLIC.into()))
            },
            |_| Ok(PUBLIC.into()),
        )
        .unwrap();
        let second = load_or_create_in(
            &dir,
            || {
                calls += 1;
                Ok((SECRET.into(), PUBLIC.into()))
            },
            |_| Ok(PUBLIC.into()),
        )
        .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(first.secret, second.secret);
        assert_eq!(first.public, second.public);
    }

    #[test]
    fn missing_public_key_is_rederived_or_named() {
        let state = thegn_core::testenv::IsolatedState::new();
        let dir = state.root().join("keys");
        private_dir(&dir);
        thegn_core::fsperm::write_owner_only_new(&dir.join("secret-key"), SECRET.as_bytes())
            .unwrap();
        let actual = load_or_create_in(
            &dir,
            || panic!("a valid existing secret must not be regenerated"),
            |secret| {
                assert_eq!(secret, SECRET);
                Ok(PUBLIC.into())
            },
        )
        .unwrap();
        assert_eq!(actual.public, PUBLIC);
        assert_eq!(
            std::fs::read_to_string(dir.join("public-key")).unwrap(),
            PUBLIC
        );
    }
}
