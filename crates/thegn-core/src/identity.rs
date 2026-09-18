//! Named, decoupled per-tool **identities** (roadmap H/AU) — the mix-and-match
//! credential primitive referenced by profiles and bundles.
//!
//! A profile (`[profiles.<p>] identity = "…"`) or a bundle (`[bundle.<n>]
//! identity = "…"`) names an identity; each tool it sets — git config, git SSH
//! key, `gh` config, GnuPG home, agent accounts — resolves **independently**, and
//! any tool it leaves unset falls through to a less specific scope and finally to
//! the profile-root default (which [`crate::profile::reroot`] pins into the
//! process env and the pane allowlist carries). The pure lookup + `~`-expanded
//! resolution live here; folding an identity into a pane's
//! [`crate::bundle::ResolvedEnv`] lives in [`crate::bundle`].

use crate::bundle::Bind;
use crate::config::{Config, IdentityConfig};
use crate::db::Db;
use crate::store::WorkspaceStore;
use crate::util;
use sha2::{Digest, Sha256};
use std::fmt;
use std::path::{Path, PathBuf};

/// Version of the byte encoding used by all canonical identity digests.
/// Changing this is an intentional identity migration, not a formatting tweak.
pub const IDENTITY_ENCODING_VERSION: u8 = 1;
pub const MAX_IDENTITY_BYTES: usize = 16 * 1024;
pub const MAX_IDENTITY_COMPONENT_BYTES: usize = 16 * 1024;
pub const MAX_DISPLAY_LABEL_BYTES: usize = 64;
pub const REPOSITORY_ID_BYTES: usize = 32;
pub const WORKTREE_ID_BYTES: usize = 32;
pub const WORKTREE_GENERATION_BYTES: usize = 16;

/// A bounded, exact byte component in a canonical identity encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BytePart<'a>(&'a [u8]);

impl<'a> BytePart<'a> {
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(self) -> &'a [u8] {
        self.0
    }
}

/// Encode domain-separated, length-delimited identity inputs.
///
/// The encoding deliberately includes the version and part count. A delimiter
/// based encoding would make `(ab, c)` and `(a, bc)` indistinguishable and
/// would make future additions silently change the meaning of old IDs.
pub fn encode_parts<D: AsRef<[u8]>>(
    domain: D,
    parts: &[BytePart<'_>],
) -> Result<Vec<u8>, IdentityError> {
    let domain = domain.as_ref();
    if domain.len() > MAX_IDENTITY_COMPONENT_BYTES {
        return Err(IdentityError::InputTooLong {
            kind: "identity domain",
        });
    }
    let mut total = 1usize
        .checked_add(8)
        .and_then(|size| size.checked_add(domain.len()))
        .and_then(|size| size.checked_add(8))
        .ok_or(IdentityError::EncodingTooLong {
            actual: usize::MAX,
            limit: MAX_IDENTITY_BYTES,
        })?;
    for part in parts {
        if part.0.len() > MAX_IDENTITY_COMPONENT_BYTES {
            return Err(IdentityError::InputTooLong {
                kind: "identity component",
            });
        }
        total = total
            .checked_add(8)
            .and_then(|size| size.checked_add(part.0.len()))
            .ok_or(IdentityError::EncodingTooLong {
                actual: usize::MAX,
                limit: MAX_IDENTITY_BYTES,
            })?;
    }
    if total > MAX_IDENTITY_BYTES {
        return Err(IdentityError::EncodingTooLong {
            actual: total,
            limit: MAX_IDENTITY_BYTES,
        });
    }
    let mut out = Vec::with_capacity(total);
    out.push(IDENTITY_ENCODING_VERSION);
    push_len(&mut out, domain.len());
    out.extend_from_slice(domain);
    push_len(&mut out, parts.len());
    for part in parts {
        push_len(&mut out, part.0.len());
        out.extend_from_slice(part.0);
    }
    Ok(out)
}

fn push_len(out: &mut Vec<u8>, len: usize) {
    out.extend_from_slice(&(len as u64).to_be_bytes());
}

/// Hash canonical identity inputs with SHA-256, retaining the complete digest.
pub fn digest_parts<D: AsRef<[u8]>>(
    domain: D,
    parts: &[BytePart<'_>],
) -> Result<[u8; 32], IdentityError> {
    Ok(Sha256::digest(encode_parts(domain, parts)?).into())
}

/// A display-only label derived from exact bytes. It is bounded and must never
/// be parsed back into an authority key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DisplayLabel(String);

impl DisplayLabel {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DisplayLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Render raw bytes without making lossy text the identity. Separators and
/// controls are escaped because callers may place this label in a path.
pub fn display_label(raw: &[u8]) -> DisplayLabel {
    let mut rendered = String::new();
    let mut push = |text: &str| {
        let remaining = MAX_DISPLAY_LABEL_BYTES.saturating_sub(rendered.len());
        if remaining == 0 {
            return false;
        }
        let end = text
            .char_indices()
            .take_while(|(index, ch)| index + ch.len_utf8() <= remaining)
            .last()
            .map_or(0, |(index, ch)| index + ch.len_utf8());
        if end == 0 {
            return false;
        }
        let text = &text[..end];
        rendered.push_str(text);
        rendered.len() < MAX_DISPLAY_LABEL_BYTES
    };
    // Validate only the bounded prefix. A public identity/display boundary may
    // receive an arbitrarily large byte slice; validating the whole slice
    // before projecting it made display work proportional to hostile input.
    let bounded = &raw[..raw.len().min(MAX_DISPLAY_LABEL_BYTES)];
    let utf8_prefix = match std::str::from_utf8(bounded) {
        Ok(text) => Some(text),
        Err(error) if error.error_len().is_none() => {
            std::str::from_utf8(&bounded[..error.valid_up_to()]).ok()
        }
        Err(_) => None,
    };
    if let Some(text) = utf8_prefix {
        'input: for ch in text.chars() {
            match ch {
                '/' | '\\' => {
                    if !push("⁄") {
                        break;
                    }
                }
                c if c.is_control() => {
                    for byte in c.to_string().as_bytes() {
                        if !push(&format!("\\x{byte:02x}")) {
                            break 'input;
                        }
                    }
                }
                c => {
                    if !push(&c.to_string()) {
                        break;
                    }
                }
            }
        }
    } else {
        'input: for byte in bounded {
            if byte.is_ascii_graphic() && !matches!(*byte, b'/' | b'\\') {
                if !push(&char::from(*byte).to_string()) {
                    break;
                }
            } else {
                if !push(&format!("\\x{byte:02x}")) {
                    break 'input;
                }
            }
        }
    }
    if rendered.is_empty() {
        rendered.push_str("(unnamed)");
    }
    DisplayLabel(rendered)
}

/// An absolute path whose native representation has been captured before it
/// can participate in an authority key. The fields are private so display
/// text cannot be mutated into authority.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExactPath {
    path: PathBuf,
    bytes: Vec<u8>,
}

impl ExactPath {
    pub fn from_path(path: &Path) -> Result<Self, IdentityError> {
        if !path.is_absolute() {
            return Err(IdentityError::InvalidInput {
                kind: "path",
                reason: "path must be absolute",
            });
        }
        let bytes = util::native_path_bytes_bounded(path, MAX_IDENTITY_COMPONENT_BYTES).map_err(
            |error| match error {
                util::NativePathError::TooLong => IdentityError::InputTooLong { kind: "path" },
                util::NativePathError::Unsupported => {
                    IdentityError::UnsupportedEncoding { kind: "path" }
                }
            },
        )?;
        if bytes.is_empty() {
            return Err(IdentityError::InvalidInput {
                kind: "path",
                reason: "path must not be empty",
            });
        }
        Ok(Self {
            path: path.to_path_buf(),
            bytes,
        })
    }

    pub(crate) fn from_git_bytes(bytes: &[u8]) -> Result<Self, IdentityError> {
        if bytes.len() > MAX_IDENTITY_COMPONENT_BYTES {
            return Err(IdentityError::InputTooLong { kind: "git path" });
        }
        let path = util::path_from_git_bytes(bytes)
            .ok_or(IdentityError::UnsupportedEncoding { kind: "git path" })?;
        Self::from_path(&path)
    }

    pub fn as_path(&self) -> &Path {
        &self.path
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn display_label(&self) -> DisplayLabel {
        display_label(&self.bytes)
    }
}

/// An exact Git branch/ref name. The raw bytes are authoritative; `display`
/// is deliberately a separate bounded projection.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BranchRef {
    raw: Vec<u8>,
    display: DisplayLabel,
}

impl BranchRef {
    pub fn from_bytes(raw: &[u8]) -> Result<Self, IdentityError> {
        if raw.is_empty() {
            return Err(IdentityError::InvalidInput {
                kind: "branch ref",
                reason: "branch ref must not be empty",
            });
        }
        if raw.len() > MAX_IDENTITY_BYTES {
            return Err(IdentityError::InputTooLong { kind: "branch ref" });
        }
        if raw.contains(&0) {
            return Err(IdentityError::UnsupportedEncoding { kind: "branch ref" });
        }
        Ok(Self {
            raw: raw.to_vec(),
            display: display_label(raw),
        })
    }

    pub fn raw(&self) -> &[u8] {
        &self.raw
    }

    pub fn display(&self) -> &DisplayLabel {
        &self.display
    }
}

/// A full repository identity. It is the digest of the canonical Git common
/// directory, so moving that directory intentionally changes this identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RepositoryId([u8; REPOSITORY_ID_BYTES]);

impl RepositoryId {
    pub fn from_common_dir(common_dir: &ExactPath) -> Result<Self, IdentityError> {
        Ok(Self(digest_parts(
            b"thegn/repository-id",
            &[BytePart::new(common_dir.as_bytes())],
        )?))
    }

    pub fn from_bytes(bytes: [u8; REPOSITORY_ID_BYTES]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; REPOSITORY_ID_BYTES] {
        &self.0
    }

    pub fn hex(&self) -> String {
        hex_bytes(&self.0)
    }
}

/// Stable identity for one allocated Git worktree instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorktreeId([u8; WORKTREE_ID_BYTES]);

impl WorktreeId {
    pub fn from_parts(
        repository: RepositoryId,
        generation: WorktreeGeneration,
        branch: &BranchRef,
        path_mode: &[u8],
    ) -> Result<Self, IdentityError> {
        Ok(Self(digest_parts(
            b"thegn/worktree-id",
            &[
                BytePart::new(repository.as_bytes()),
                BytePart::new(generation.as_bytes()),
                BytePart::new(branch.raw()),
                BytePart::new(path_mode),
            ],
        )?))
    }

    pub fn from_bytes(bytes: [u8; WORKTREE_ID_BYTES]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; WORKTREE_ID_BYTES] {
        &self.0
    }

    pub fn hex(&self) -> String {
        hex_bytes(&self.0)
    }
}

/// Stable identity for one Git administrative worktree instance. It changes
/// when Git replaces the admin directory and remains stable across a rename.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorktreeGeneration([u8; WORKTREE_GENERATION_BYTES]);

impl WorktreeGeneration {
    pub(crate) fn from_bytes(bytes: [u8; WORKTREE_GENERATION_BYTES]) -> Self {
        Self(bytes)
    }

    /// Constructed only from the exact instance stamp captured by the Git
    /// inspection seam. The stamp is an OS identity proof, not caller-chosen
    /// randomness; unsupported platforms must refuse capture before effects.
    pub(crate) fn from_captured_instance_stamp(stamp: &[u8]) -> Result<Self, IdentityError> {
        let digest = digest_parts(b"thegn/worktree-generation", &[BytePart::new(stamp)])?;
        let mut bytes = [0; WORKTREE_GENERATION_BYTES];
        bytes.copy_from_slice(&digest[..WORKTREE_GENERATION_BYTES]);
        Ok(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; WORKTREE_GENERATION_BYTES] {
        &self.0
    }

    pub fn hex(&self) -> String {
        hex_bytes(&self.0)
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(out, "{byte:02x}").expect("writing to String cannot fail");
    }
    out
}

/// Typed failures used by canonical identity and Git inspection APIs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityError {
    InvalidInput {
        kind: &'static str,
        reason: &'static str,
    },
    InputTooLong {
        kind: &'static str,
    },
    EncodingTooLong {
        actual: usize,
        limit: usize,
    },
    UnsupportedEncoding {
        kind: &'static str,
    },
    GitProbeFailed {
        operation: &'static str,
    },
    NotRegistered,
    Ambiguous {
        kind: &'static str,
    },
    AuthorityUnavailable,
}

impl fmt::Display for IdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput { kind, reason } => write!(f, "invalid {kind}: {reason}"),
            Self::InputTooLong { kind } => write!(f, "{kind} exceeds the identity bound"),
            Self::EncodingTooLong { actual, limit } => {
                write!(f, "identity encoding is {actual} bytes; limit is {limit}")
            }
            Self::UnsupportedEncoding { kind } => {
                write!(f, "cannot preserve exact {kind} representation")
            }
            Self::GitProbeFailed { operation } => {
                write!(f, "Git identity probe failed: {operation}")
            }
            Self::NotRegistered => f.write_str("Git worktree is not registered"),
            Self::Ambiguous { kind } => write!(f, "ambiguous {kind} identity"),
            Self::AuthorityUnavailable => f.write_str("identity authority is unavailable"),
        }
    }
}

impl std::error::Error for IdentityError {}

/// Look up a named identity, or `None` if undefined (callers warn + skip).
pub fn resolve<'a>(cfg: &'a Config, name: &str) -> Option<&'a IdentityConfig> {
    cfg.identities.get(name)
}

// --- directly-bound identity (the identity switcher) -----------------------
//
// Separate from the `[profiles.<p>].identity` / `[bundle.<n>].identity` *config*
// references: the switcher pins an identity at a scope over the `ui_state` KV
// (worktree → workspace → global, most-specific wins), mirroring the bundle
// binding. `bundle::compose` folds these *after* the bundle chain, so an explicit
// switch wins over a bundle-referenced identity.

fn scope_global() -> String {
    "identity".to_string()
}
fn scope_ws(slug: &str) -> String {
    format!("identity:ws:{slug}")
}
fn scope_wt(worktree: &str) -> String {
    format!("identity:wt:{worktree}")
}

fn bound_global(db: &Db) -> Option<String> {
    db.get_ui_state(&scope_global(), "active").ok().flatten()
}
fn bound_ws(db: &Db, slug: &str) -> Option<String> {
    db.get_ui_state(&scope_ws(slug), "active").ok().flatten()
}
fn bound_wt(db: &Db, worktree: &str) -> Option<String> {
    db.get_ui_state(&scope_wt(worktree), "active")
        .ok()
        .flatten()
}

/// The single most-specific directly-bound identity (worktree → workspace →
/// global), for the switcher chip + display. `None` ⇒ none bound.
pub fn active_name(db: &Db, worktree: &str, slug: Option<&str>) -> Option<String> {
    bound_wt(db, worktree)
        .or_else(|| slug.and_then(|s| bound_ws(db, s)))
        .or_else(|| bound_global(db))
}

/// Directly-bound identities that apply to a scope, low→high (global, workspace,
/// worktree). `bundle::compose` folds these in order so the worktree binding
/// wins. Empty ⇒ none bound.
pub fn bound_in_order(db: &Db, worktree: &str, slug: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(g) = bound_global(db) {
        out.push(g);
    }
    if let Some(w) = slug.and_then(|s| bound_ws(db, s)) {
        out.push(w);
    }
    if let Some(t) = bound_wt(db, worktree) {
        out.push(t);
    }
    out
}

/// Bind `name` as the active identity at the given scope.
pub fn set_active(
    db: &Db,
    bind: Bind,
    worktree: &str,
    slug: Option<&str>,
    name: &str,
) -> anyhow::Result<()> {
    let scope = match bind {
        Bind::Global => scope_global(),
        Bind::Workspace => scope_ws(slug.unwrap_or_default()),
        Bind::Worktree => scope_wt(worktree),
    };
    db.set_ui_state(&scope, "active", name)?;
    Ok(())
}

/// Clear the active-identity binding at the given scope.
pub fn clear_active(db: &Db, bind: Bind, worktree: &str, slug: Option<&str>) -> anyhow::Result<()> {
    let scope = match bind {
        Bind::Global => scope_global(),
        Bind::Workspace => scope_ws(slug.unwrap_or_default()),
        Bind::Worktree => scope_wt(worktree),
    };
    db.del_ui_state(&scope, "active")?;
    Ok(())
}

/// An identity's per-tool credential locations, `~` expanded. Each field is
/// `None` when the identity leaves that tool unset (mix-and-match).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolved {
    /// `GIT_CONFIG_GLOBAL` target.
    pub git_config: Option<String>,
    /// SSH private key feeding `GIT_SSH_COMMAND`.
    pub git_ssh_key: Option<String>,
    /// `GH_CONFIG_DIR` target.
    pub gh_config: Option<String>,
    /// `GNUPGHOME` target.
    pub gpg_home: Option<String>,
    /// Commit-signing binding: `(gpg.format, user.signingKey)`. `None` when the
    /// identity sets no signing key (falls through the scope chain / repo git
    /// config). The key is `~`-expanded only for the ssh format (a path);
    /// openpgp key ids are left verbatim.
    pub git_signing: Option<(String, String)>,
}

impl Resolved {
    /// The `GIT_SSH_COMMAND` value forcing this identity's key with
    /// `IdentitiesOnly=yes` (so ambient agent keys can't leak), or `None` when the
    /// identity sets no key.
    pub fn git_ssh_command(&self) -> Option<String> {
        self.git_ssh_key
            .as_ref()
            .map(|k| format!("ssh -i {k} -o IdentitiesOnly=yes"))
    }

    /// The `git -c …` overrides that bind this identity's signing key, if set:
    /// `["-c", "gpg.format=<fmt>", "-c", "user.signingKey=<key>"]`. Empty when
    /// the identity sets no signing key, so a caller can unconditionally splice
    /// the result into a git argv. The per-operation controls (the commit
    /// overlay's `^S` cycle, `[git] override_gpg`) still layer above this.
    pub fn git_signing_args(&self) -> Vec<String> {
        match &self.git_signing {
            Some((fmt, key)) => vec![
                "-c".into(),
                format!("gpg.format={fmt}"),
                "-c".into(),
                format!("user.signingKey={key}"),
            ],
            None => Vec::new(),
        }
    }
}

/// Resolve an [`IdentityConfig`] into `~`-expanded per-tool paths.
pub fn resolved(id: &IdentityConfig) -> Resolved {
    let opt = |s: &str| (!s.is_empty()).then(|| util::expand_tilde(s));
    let git_signing = (!id.signing.key.is_empty()).then(|| {
        let fmt = id.signing.format.as_str().to_string();
        // An ssh signing key is a path (expand `~`); an openpgp key id is not.
        let key = if matches!(id.signing.format, crate::config::SigningFormat::Ssh) {
            util::expand_tilde(&id.signing.key)
        } else {
            id.signing.key.clone()
        };
        (fmt, key)
    });
    Resolved {
        git_config: opt(&id.git.config),
        git_ssh_key: opt(&id.git.ssh_key),
        gh_config: opt(&id.gh.config),
        gpg_home: opt(&id.gpg.home),
        git_signing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{IdentityConfig, IdentityGh, IdentityGit, IdentityGpg};

    #[test]
    fn resolve_finds_defined_and_misses_unknown() {
        let mut cfg = Config::default();
        cfg.identities
            .insert("washu".into(), IdentityConfig::default());
        assert!(resolve(&cfg, "washu").is_some());
        assert!(resolve(&cfg, "nope").is_none());
    }

    #[test]
    fn resolved_maps_set_tools_and_leaves_unset_none() {
        // A partial identity: git + gh set, gpg/ssh unset ⇒ those resolve None
        // (they fall through to a less specific scope / profile-root fallback).
        let id = IdentityConfig {
            git: IdentityGit {
                config: "/a/gitconfig".into(),
                ssh_key: String::new(),
            },
            gh: IdentityGh {
                config: "/a/gh".into(),
            },
            gpg: IdentityGpg::default(),
            signing: Default::default(),
            accounts: Default::default(),
        };
        let r = resolved(&id);
        assert_eq!(r.git_config.as_deref(), Some("/a/gitconfig"));
        assert_eq!(r.gh_config.as_deref(), Some("/a/gh"));
        assert_eq!(r.git_ssh_key, None);
        assert_eq!(r.gpg_home, None);
        assert_eq!(r.git_ssh_command(), None);
        // No signing set ⇒ no signing args (falls through to repo/global config).
        assert_eq!(r.git_signing, None);
        assert!(r.git_signing_args().is_empty());
    }

    #[test]
    fn signing_resolves_format_and_key_by_format() {
        use crate::config::{IdentitySigning, SigningFormat};
        // openpgp key id is left verbatim.
        let gpg = IdentityConfig {
            signing: IdentitySigning {
                format: SigningFormat::Openpgp,
                key: "ABCD1234".into(),
            },
            ..Default::default()
        };
        assert_eq!(
            resolved(&gpg).git_signing,
            Some(("openpgp".into(), "ABCD1234".into()))
        );
        assert_eq!(
            resolved(&gpg).git_signing_args(),
            vec!["-c", "gpg.format=openpgp", "-c", "user.signingKey=ABCD1234"]
        );
        // ssh key is a path ⇒ `~` expanded.
        let ssh = IdentityConfig {
            signing: IdentitySigning {
                format: SigningFormat::Ssh,
                key: "~/.ssh/id_sign.pub".into(),
            },
            ..Default::default()
        };
        let (fmt, key) = resolved(&ssh).git_signing.unwrap();
        assert_eq!(fmt, "ssh");
        assert!(
            !key.starts_with('~'),
            "ssh key path must be expanded: {key}"
        );
        assert!(key.ends_with("/.ssh/id_sign.pub"));
    }

    #[test]
    fn git_ssh_command_forces_identities_only() {
        let r = Resolved {
            git_ssh_key: Some("/keys/id_washu".into()),
            ..Default::default()
        };
        assert_eq!(
            r.git_ssh_command().as_deref(),
            Some("ssh -i /keys/id_washu -o IdentitiesOnly=yes")
        );
    }

    #[test]
    fn resolved_expands_leading_tilde() {
        let id = IdentityConfig {
            gpg: IdentityGpg {
                home: "~/.gnupg".into(),
            },
            ..Default::default()
        };
        let home = resolved(&id).gpg_home.unwrap();
        assert!(!home.starts_with('~'), "tilde must be expanded: {home}");
        assert!(home.ends_with("/.gnupg"));
    }

    #[test]
    fn binding_roundtrip_and_scope_precedence() {
        let db = crate::db::Db::open_memory().unwrap();
        assert_eq!(active_name(&db, "/wt", Some("repo")), None);
        assert!(bound_in_order(&db, "/wt", Some("repo")).is_empty());

        set_active(&db, Bind::Global, "/wt", Some("repo"), "g").unwrap();
        assert_eq!(active_name(&db, "/wt", Some("repo")).as_deref(), Some("g"));

        // A worktree binding is more specific than the global one.
        set_active(&db, Bind::Worktree, "/wt", Some("repo"), "w").unwrap();
        assert_eq!(active_name(&db, "/wt", Some("repo")).as_deref(), Some("w"));
        // Fold order is low→high (global first, worktree last).
        assert_eq!(
            bound_in_order(&db, "/wt", Some("repo")),
            vec!["g".to_string(), "w".to_string()]
        );

        clear_active(&db, Bind::Worktree, "/wt", Some("repo")).unwrap();
        assert_eq!(active_name(&db, "/wt", Some("repo")).as_deref(), Some("g"));
    }

    #[test]
    fn encoding_is_versioned_length_delimited_and_domain_separated() {
        let encoded = encode_parts("test", &[BytePart::new(b"a"), BytePart::new(b"bc")]).unwrap();
        assert_eq!(encoded[0], IDENTITY_ENCODING_VERSION);
        assert_eq!(&encoded[1..9], &(4u64.to_be_bytes()));
        assert_eq!(&encoded[9..13], b"test");
        assert_eq!(&encoded[13..21], &(2u64.to_be_bytes()));
        assert_eq!(&encoded[21..29], &(1u64.to_be_bytes()));
        assert_eq!(encoded[29], b'a');
        assert_eq!(
            hex_bytes(&digest_parts("test", &[BytePart::new(b"a"), BytePart::new(b"bc")]).unwrap()),
            "8210dc11675e74e4907461f6f77155f4a726d2d76d6a8db44cb98e0dc6a73fa5"
        );
        assert_ne!(
            encode_parts("test", &[BytePart::new(b"ab"), BytePart::new(b"c")]).unwrap(),
            encode_parts("test", &[BytePart::new(b"a"), BytePart::new(b"bc")]).unwrap()
        );
        assert_ne!(
            digest_parts("repo", &[BytePart::new(b"same")]).unwrap(),
            digest_parts("worktree", &[BytePart::new(b"same")]).unwrap()
        );
    }

    #[test]
    fn exact_branch_bytes_do_not_collapse_display_aliases() {
        let refs = [
            b"feat/a".as_slice(),
            b"feat-a".as_slice(),
            b"feat_a".as_slice(),
            b"FEAT-A".as_slice(),
            "修复".as_bytes(),
        ];
        let ids: Vec<_> = refs
            .iter()
            .map(|raw| {
                let branch = BranchRef::from_bytes(raw).unwrap();
                WorktreeId::from_parts(
                    RepositoryId::from_bytes([7; REPOSITORY_ID_BYTES]),
                    WorktreeGeneration::from_bytes([8; WORKTREE_GENERATION_BYTES]),
                    &branch,
                    b"in-repo",
                )
                .unwrap()
            })
            .collect();
        for (index, id) in ids.iter().enumerate() {
            assert!(ids[index + 1..].iter().all(|other| other != id));
        }
        let long = display_label(&vec![b'x'; MAX_DISPLAY_LABEL_BYTES * 4]);
        assert!(long.as_str().len() <= MAX_DISPLAY_LABEL_BYTES);
        assert_ne!(display_label(b"feat/a"), display_label(b"feat-a"));
    }

    #[test]
    fn exact_paths_refuse_relative_authority_inputs() {
        let error = ExactPath::from_path(Path::new("relative/worktree")).unwrap_err();
        assert_eq!(
            error,
            IdentityError::InvalidInput {
                kind: "path",
                reason: "path must be absolute"
            }
        );
        assert_eq!(
            BranchRef::from_bytes(b"feat\0ref").unwrap_err(),
            IdentityError::UnsupportedEncoding { kind: "branch ref" }
        );
    }

    #[test]
    fn identity_bounds_return_errors_without_panicking() {
        let component = vec![b'x'; MAX_IDENTITY_COMPONENT_BYTES];
        assert!(encode_parts("d", &[BytePart::new(&component)]).is_err());
        assert!(encode_parts("d", &[BytePart::new(&vec![b'x'; MAX_IDENTITY_BYTES - 26])]).is_ok());
        assert!(encode_parts("d", &[BytePart::new(&vec![b'x'; MAX_IDENTITY_BYTES - 25])]).is_err());
        assert!(BranchRef::from_bytes(&vec![b'x'; MAX_IDENTITY_COMPONENT_BYTES + 1]).is_err());
    }

    #[test]
    fn display_label_stops_before_rendering_unbounded_input() {
        let label = display_label(&[b'/'; MAX_DISPLAY_LABEL_BYTES * 100]);
        assert!(label.as_str().len() <= MAX_DISPLAY_LABEL_BYTES);
    }
}
