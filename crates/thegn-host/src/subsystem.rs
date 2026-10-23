//! **Subprofiles** — per-subsystem, in-process identity splits within a profile
//! (roadmap H, items 536–539). Where a *profile* firewalls the whole process
//! (separate OS process + reroot), a *subprofile* re-scopes ONE subsystem
//! (e.g. Comms work↔personal) while the rest of the profile — crucially the
//! `workspace` dev shell — stays put.
//!
//! A [`Subsystem`] owns three things explicitly (the design's requirement that
//! storage ownership is per-subsystem, NOT the cache-over-git model the
//! `workspace` uses — Comms mail/chat is authoritative):
//! - its **storage handle** (its own DB *file*, so teardown is a clean handle
//!   drop, never closing the shared profile DB),
//! - its **active subprofile**,
//! - its **pane-id set** — it does not own `&mut Panes`; [`Subsystem::teardown`]
//!   *returns* the pane ids for the event loop to reap, avoiding borrow fights.
//!
//! A subprofile switch prepares and validates the new storage before committing
//! it, then returns the old pane ids for the event loop to reap. It does **no
//! polling**; any periodic work a real subsystem needs rides the existing
//! `TerminalWaker` (0%-idle).
//!
//! Comms is the first real consumer (not built here); [`StubSubsystem`] exercises
//! the mechanism end-to-end so Comms can adopt it without further core changes.
//!
//! The subprofile *switch* surface (`bind`/`teardown`/`switch_subprofile`) is
//! exercised by the tests below and will be driven from the event loop by Comms
//! (H 540); until then it is intentionally not called from production paths, so
//! the module opts out of dead-code lints rather than shipping a half-wired UI.
#![allow(dead_code)]

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::{
    fmt,
    path::{Path, PathBuf},
};
use thegn_core::db::Db;

pub const SUBPROFILE_NAME_MAX_BYTES: usize = 128;
pub const RESERVED_SUBPROFILE_NAMES: &[&str] = &["default"];

/// Reject names that are empty, overlong, or contain control characters.
pub fn validate_subprofile_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        anyhow::bail!("subprofile name must not be empty or whitespace-only");
    }
    if name.len() > SUBPROFILE_NAME_MAX_BYTES {
        anyhow::bail!("subprofile name must be at most {SUBPROFILE_NAME_MAX_BYTES} bytes");
    }
    if name.chars().any(char::is_control) {
        anyhow::bail!("subprofile name must not contain control characters");
    }
    if name.trim() != name {
        anyhow::bail!("subprofile name must not start or end with whitespace");
    }
    if RESERVED_SUBPROFILE_NAMES
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved))
    {
        anyhow::bail!("`default` is reserved for the profile-level scope");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubprofileStorageRefusal {
    NotRegularFile { file: String },
    Symlink { file: String },
    ForeignOwner { file: String },
    HardLinked { file: String },
    LegacySlugDatabase { file: String },
}

impl fmt::Display for SubprofileStorageRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRegularFile { file } => write!(
                f,
                "subprofile database `{file}` is not a regular file; move it aside and retry"
            ),
            Self::Symlink { file } => write!(
                f,
                "subprofile database `{file}` is a symlink; replace it with a regular file and retry"
            ),
            Self::ForeignOwner { file } => write!(
                f,
                "subprofile database `{file}` is not owned by the current user; move it aside and retry"
            ),
            Self::HardLinked { file } => write!(
                f,
                "subprofile database `{file}` has multiple hard links; move it aside and retry"
            ),
            Self::LegacySlugDatabase { file } => write!(
                f,
                "a pre-digest subprofile database `{file}` exists in this root; its name collided across subprofiles, so it is not opened or migrated automatically: move it aside after checking which subprofile it belongs to"
            ),
        }
    }
}

impl std::error::Error for SubprofileStorageRefusal {}

fn check_db_leaf(dir: &Path, file: &str, subprofile: &str) -> Result<()> {
    let db_path = dir.join(file);
    match std::fs::symlink_metadata(&db_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("checking subprofile database `{file}`"));
        }
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(SubprofileStorageRefusal::Symlink {
                file: file.to_string(),
            }
            .into());
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(SubprofileStorageRefusal::NotRegularFile {
                file: file.to_string(),
            }
            .into());
        }
        Ok(metadata) if !thegn_core::fsperm::owned_by_current_user(&metadata) => {
            return Err(SubprofileStorageRefusal::ForeignOwner {
                file: file.to_string(),
            }
            .into());
        }
        Ok(metadata) if thegn_core::fsperm::ensure_single_link_regular_file(&metadata).is_err() => {
            return Err(SubprofileStorageRefusal::HardLinked {
                file: file.to_string(),
            }
            .into());
        }
        Ok(_) => {}
    }

    let legacy = format!("{}.db", thegn_core::util::slugify(subprofile));
    if legacy != file {
        match std::fs::symlink_metadata(dir.join(&legacy)) {
            Ok(_) => {
                return Err(SubprofileStorageRefusal::LegacySlugDatabase { file: legacy }.into());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("checking legacy subprofile database `{legacy}`"));
            }
        }
    }
    Ok(())
}

fn subprofile_db_file_name(name: &str) -> String {
    let slug = thegn_core::util::slugify(name);
    let prefix = slug
        .get(..slug.len().min(32))
        .unwrap_or("")
        .trim_end_matches('-');
    let prefix = if prefix.is_empty() { "sub" } else { prefix };
    let mut digest = Sha256::new();
    digest.update(b"thegn-subprofile-v1\0");
    digest.update(name.as_bytes());
    let hex = format!("{:x}", digest.finalize());
    format!("{prefix}--{hex}.db")
}

/// Storage opened and validated before a subprofile binding is committed.
pub struct PreparedBinding {
    db: Db,
}

/// What a subsystem's storage binding is right now. Never names a scope whose storage is closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubprofileStatus {
    /// The subsystem never splits (`workspace`): it always uses the profile-level scope.
    ProfileScope,
    /// Storage for this subprofile is open.
    Active(String),
    /// No storage is open (never bound, or torn down).
    Unbound,
}

/// A coherent feature area with its own storage + identity within a profile.
pub trait Subsystem {
    /// Stable id (`"workspace"`, `"comms"`, later `"ai"`).
    fn name(&self) -> &str;
    /// Whether this subsystem can split into named subprofiles. `workspace` is
    /// `false` — it always uses the profile-level (shared) scope.
    fn supports_subprofiles(&self) -> bool;
    /// The active subprofile name; `"default"` is the profile-level scope or, for a
    /// subprofile-capable subsystem, no binding (see [`Subsystem::status`]).
    fn active_subprofile(&self) -> &str;
    /// Truthful binding status; `Active` only while storage is open.
    fn status(&self) -> SubprofileStatus;
    /// Pane ids this subsystem currently owns.
    fn pane_ids(&self) -> Vec<u32>;
    #[cfg(test)]
    fn storage_is_open(&self) -> bool;
    /// Adopt a pane the subsystem spawned (default: ignored — `workspace` panes
    /// are owned by the event loop, not by a subsystem).
    fn adopt_pane(&mut self, _id: u32) {}
    /// Rebind to `subprofile`: (re)open its storage and reset the pane set. Must
    /// not poll. Errors if the subsystem does not support subprofiles.
    fn bind(&mut self, subprofile: &str) -> Result<()>;
    /// Open and validate storage for a prospective binding without changing
    /// this subsystem's current identity, storage, or panes.
    fn prepare(&self, subprofile: &str) -> Result<PreparedBinding>;
    /// Install a prepared binding and return the old pane ids for reaping.
    fn commit(&mut self, subprofile: &str, prepared: PreparedBinding) -> Vec<u32>;
    /// Relinquish this subsystem's live resources: drop its storage handle and
    /// return the pane ids the caller must reap (the loop kills them). Idempotent.
    fn teardown(&mut self) -> Vec<u32>;
}

/// The `workspace` subsystem — the existing IDE shell. It never splits into
/// subprofiles (unified dev) and its panes are owned by the event loop, so
/// teardown is a no-op: it is untouched by any other subsystem's subprofile
/// switch, by construction.
#[derive(Default)]
pub struct WorkspaceSubsystem;

impl Subsystem for WorkspaceSubsystem {
    fn name(&self) -> &str {
        "workspace"
    }
    fn supports_subprofiles(&self) -> bool {
        false
    }
    fn active_subprofile(&self) -> &str {
        "default"
    }
    fn status(&self) -> SubprofileStatus {
        SubprofileStatus::ProfileScope
    }
    fn pane_ids(&self) -> Vec<u32> {
        Vec::new()
    }
    #[cfg(test)]
    fn storage_is_open(&self) -> bool {
        false
    }
    fn bind(&mut self, _subprofile: &str) -> Result<()> {
        anyhow::bail!("workspace does not support subprofiles")
    }
    fn prepare(&self, _subprofile: &str) -> Result<PreparedBinding> {
        anyhow::bail!("workspace does not support subprofiles")
    }
    fn commit(&mut self, _subprofile: &str, _prepared: PreparedBinding) -> Vec<u32> {
        Vec::new()
    }
    fn teardown(&mut self) -> Vec<u32> {
        Vec::new()
    }
}

/// A subprofile-capable subsystem with its own per-subprofile DB file — the
/// shape Comms will take. Storage lives under the active profile at
/// `<profile-root>/<name>/<subprofile>.db`; `db_root = None` uses an in-memory
/// DB (tests). Exercises bind/teardown for the mechanism until Comms lands.
pub struct StubSubsystem {
    name: String,
    /// `None` ⇒ in-memory storage (tests); `Some(dir)` ⇒ `<dir>/<subprofile>.db`.
    db_root: Option<PathBuf>,
    active: String,
    db: Option<Db>,
    panes: Vec<u32>,
}

impl StubSubsystem {
    /// A stub rooted at the active profile's `<name>/` storage dir.
    pub fn new(name: &str) -> Self {
        let db_root = Some(thegn_core::profile::active().root.join(name));
        Self {
            name: name.to_string(),
            db_root,
            active: "default".to_string(),
            db: None,
            panes: Vec::new(),
        }
    }

    /// An in-memory stub (tests) — no filesystem storage.
    pub fn in_memory(name: &str) -> Self {
        Self {
            name: name.to_string(),
            db_root: None,
            active: "default".to_string(),
            db: None,
            panes: Vec::new(),
        }
    }

    /// Whether a storage handle is currently open (for tests).
    pub fn has_storage(&self) -> bool {
        self.db.is_some()
    }
}

impl Subsystem for StubSubsystem {
    fn name(&self) -> &str {
        &self.name
    }
    fn supports_subprofiles(&self) -> bool {
        true
    }
    fn active_subprofile(&self) -> &str {
        &self.active
    }
    fn status(&self) -> SubprofileStatus {
        if self.db.is_some() {
            SubprofileStatus::Active(self.active.clone())
        } else {
            SubprofileStatus::Unbound
        }
    }
    fn pane_ids(&self) -> Vec<u32> {
        self.panes.clone()
    }
    #[cfg(test)]
    fn storage_is_open(&self) -> bool {
        self.db.is_some()
    }
    fn adopt_pane(&mut self, id: u32) {
        if !self.panes.contains(&id) {
            self.panes.push(id);
        }
    }
    fn bind(&mut self, subprofile: &str) -> Result<()> {
        let prepared = self.prepare(subprofile)?;
        self.commit(subprofile, prepared);
        Ok(())
    }
    fn prepare(&self, subprofile: &str) -> Result<PreparedBinding> {
        validate_subprofile_name(subprofile)?;
        let db = match &self.db_root {
            Some(dir) => {
                thegn_core::fsperm::ensure_private_dir(dir)?;
                let file = subprofile_db_file_name(subprofile);
                check_db_leaf(dir, &file, subprofile)?;
                Db::open_at(&dir.join(file))?
            }
            None => Db::open_memory()?,
        };
        Ok(PreparedBinding { db })
    }
    fn commit(&mut self, subprofile: &str, prepared: PreparedBinding) -> Vec<u32> {
        self.db = Some(prepared.db);
        self.active = subprofile.to_string();
        std::mem::take(&mut self.panes)
    }
    fn teardown(&mut self) -> Vec<u32> {
        self.db = None; // clean handle drop — never touches the shared profile DB
        self.active = "default".to_string();
        std::mem::take(&mut self.panes)
    }
}

/// Holds the profile's subsystems for the event loop. Registered as a loop local
/// (see `run.rs`); Comms will register itself here.
pub struct Subsystems {
    subsystems: Vec<Box<dyn Subsystem>>,
}

impl Subsystems {
    /// Register the always-present `workspace` subsystem plus any subprofile-
    /// capable subsystems. Until Comms lands, a stub `comms` exercises the path.
    pub fn with_defaults() -> Self {
        Self {
            subsystems: vec![
                Box::new(WorkspaceSubsystem),
                Box::new(StubSubsystem::new("comms")),
            ],
        }
    }

    /// Registered subsystem names (for startup logging / diagnostics).
    pub fn names(&self) -> Vec<&str> {
        self.subsystems.iter().map(|s| s.name()).collect()
    }

    fn get_mut(&mut self, name: &str) -> Option<&mut Box<dyn Subsystem>> {
        self.subsystems.iter_mut().find(|s| s.name() == name)
    }

    /// Switch `name` to `subprofile`, in-process: prepare its storage, then
    /// commit (returning the old pane ids the caller must kill). Every OTHER
    /// subsystem — notably `workspace` — is untouched. Errors if the subsystem
    /// is unknown or does not support subprofiles.
    pub fn switch_subprofile(&mut self, name: &str, subprofile: &str) -> Result<Vec<u32>> {
        let sub = self
            .get_mut(name)
            .ok_or_else(|| anyhow::anyhow!("unknown subsystem {name:?}"))?;
        if !sub.supports_subprofiles() {
            anyhow::bail!("subsystem {name:?} does not support subprofiles");
        }
        let prepared = sub.prepare(subprofile)?;
        let doomed = sub.commit(subprofile, prepared);
        Ok(doomed)
    }
}

impl Default for Subsystems {
    fn default() -> Self {
        Self::with_defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn private_temp_root() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        thegn_core::fsperm::restrict_dir_to_owner(root.path()).unwrap();
        root
    }

    #[test]
    fn stub_bind_opens_storage_and_teardown_drops_it() {
        let mut s = StubSubsystem::in_memory("comms");
        assert!(!s.has_storage());
        s.bind("work").unwrap();
        assert!(s.has_storage());
        assert_eq!(s.active_subprofile(), "work");
        s.adopt_pane(7);
        s.adopt_pane(9);
        assert_eq!(s.pane_ids(), vec![7, 9]);
        // Teardown returns the panes for the loop to reap and drops storage.
        let doomed = s.teardown();
        assert_eq!(doomed, vec![7, 9]);
        assert!(!s.has_storage());
        assert!(s.pane_ids().is_empty());
    }

    #[test]
    fn teardown_leaves_no_active_subprofile() {
        let mut subsystem = StubSubsystem::in_memory("comms");
        subsystem.bind("work").unwrap();
        subsystem.adopt_pane(7);

        assert_eq!(subsystem.teardown(), vec![7]);
        assert_eq!(subsystem.active_subprofile(), "default");
        assert_eq!(subsystem.status(), SubprofileStatus::Unbound);
    }

    #[test]
    fn status_reports_active_only_with_open_storage() {
        let mut subsystem = StubSubsystem::in_memory("comms");
        assert_eq!(subsystem.status(), SubprofileStatus::Unbound);
        subsystem.bind("work").unwrap();
        // Positive control: a successful bind reports the open binding.
        assert_eq!(subsystem.status(), SubprofileStatus::Active("work".into()));

        let mut subsystems = Subsystems {
            subsystems: vec![Box::new(subsystem)],
        };
        assert!(subsystems.switch_subprofile("comms", "").is_err());
        assert_eq!(
            subsystems.get_mut("comms").unwrap().status(),
            SubprofileStatus::Active("work".into())
        );
        subsystems.switch_subprofile("comms", "home").unwrap();
        assert_eq!(
            subsystems.get_mut("comms").unwrap().status(),
            SubprofileStatus::Active("home".into())
        );
    }

    #[test]
    fn workspace_reports_the_profile_scope() {
        assert_eq!(WorkspaceSubsystem.status(), SubprofileStatus::ProfileScope);
    }

    #[test]
    fn switch_subprofile_reaps_panes_and_rebinds() {
        let mut subs = Subsystems {
            subsystems: vec![
                Box::new(WorkspaceSubsystem),
                Box::new(StubSubsystem::in_memory("comms")),
            ],
        };
        // Bind comms to work + give it panes.
        subs.switch_subprofile("comms", "work").unwrap();
        subs.get_mut("comms").unwrap().adopt_pane(3);
        subs.get_mut("comms").unwrap().adopt_pane(4);
        // Switching to personal reaps work's panes and rebinds.
        let doomed = subs.switch_subprofile("comms", "personal").unwrap();
        assert_eq!(
            doomed,
            vec![3, 4],
            "old subprofile's panes are returned for reaping"
        );
        assert_eq!(
            subs.get_mut("comms").unwrap().active_subprofile(),
            "personal"
        );
        assert!(subs.get_mut("comms").unwrap().pane_ids().is_empty());
    }

    #[test]
    fn workspace_is_untouched_and_rejects_subprofiles() {
        let mut subs = Subsystems::with_defaults();
        // workspace cannot be split…
        assert!(subs.switch_subprofile("workspace", "x").is_err());
        // …and unknown subsystems error rather than panic.
        assert!(subs.switch_subprofile("nope", "x").is_err());
        // The default registration exposes both names.
        assert!(subs.names().contains(&"workspace"));
        assert!(subs.names().contains(&"comms"));
    }

    #[test]
    fn failed_switch_keeps_old_binding_and_panes() {
        let root = private_temp_root();
        let mut subs = Subsystems {
            subsystems: vec![Box::new(StubSubsystem {
                name: "comms".into(),
                db_root: Some(root.path().join("comms")),
                active: "default".into(),
                db: None,
                panes: Vec::new(),
            })],
        };
        subs.switch_subprofile("comms", "work").unwrap();
        subs.get_mut("comms").unwrap().adopt_pane(3);
        subs.get_mut("comms").unwrap().adopt_pane(4);
        let broken = root
            .path()
            .join("comms")
            .join(subprofile_db_file_name("broken"));
        std::fs::create_dir(&broken).unwrap();

        assert!(subs.switch_subprofile("comms", "broken").is_err());
        let comms = subs.get_mut("comms").unwrap();
        assert_eq!(comms.active_subprofile(), "work");
        assert_eq!(comms.pane_ids(), vec![3, 4]);
        assert!(comms.storage_is_open());
    }

    #[test]
    fn alias_subprofile_names_get_distinct_files() {
        let names = ["work/a", "work-a", "Work-A", "work a"];
        let files = names.map(subprofile_db_file_name);
        let distinct: std::collections::HashSet<_> = files.iter().collect();
        assert_eq!(distinct.len(), names.len());
        assert!(files.iter().all(|file| file.ends_with(".db")));
        assert!(files.iter().all(|file| file.starts_with("work-a--")));

        let root = private_temp_root();
        let db_root = root.path().join("comms");
        let mut comms = StubSubsystem {
            name: "comms".into(),
            db_root: Some(db_root.clone()),
            active: "default".into(),
            db: None,
            panes: Vec::new(),
        };
        comms.bind(names[0]).unwrap();
        comms.bind(names[1]).unwrap();
        assert!(db_root.join(&files[0]).is_file());
        assert!(db_root.join(&files[1]).is_file());
        assert_ne!(files[0], files[1]);
    }

    #[test]
    fn invalid_subprofile_names_are_refused_before_teardown() {
        let mut subs = Subsystems {
            subsystems: vec![Box::new(StubSubsystem::in_memory("comms"))],
        };
        subs.switch_subprofile("comms", "work").unwrap();
        subs.get_mut("comms").unwrap().adopt_pane(3);
        subs.get_mut("comms").unwrap().adopt_pane(4);

        let overlong = "a".repeat(129);
        for invalid in ["", "   ", "a\u{7}b", overlong.as_str()] {
            assert!(subs.switch_subprofile("comms", invalid).is_err());
            let comms = subs.get_mut("comms").unwrap();
            assert_eq!(comms.active_subprofile(), "work");
            assert_eq!(comms.pane_ids(), vec![3, 4]);
            assert!(comms.storage_is_open());
        }
    }

    #[test]
    fn reserved_and_whitespace_alias_names_are_refused_before_teardown() {
        let mut subs = Subsystems {
            subsystems: vec![Box::new(StubSubsystem::in_memory("comms"))],
        };
        subs.switch_subprofile("comms", "work").unwrap();
        subs.get_mut("comms").unwrap().adopt_pane(3);
        subs.get_mut("comms").unwrap().adopt_pane(4);

        assert!(validate_subprofile_name("work a").is_ok());
        for invalid in ["default", "Default", " work", "work "] {
            assert!(subs.switch_subprofile("comms", invalid).is_err());
            let comms = subs.get_mut("comms").unwrap();
            assert_eq!(comms.active_subprofile(), "work");
            assert_eq!(comms.pane_ids(), vec![3, 4]);
            assert!(comms.storage_is_open());
        }
    }

    #[test]
    fn symlinked_database_leaf_is_refused() {
        let root = private_temp_root();
        let db_root = root.path().join("comms");
        std::fs::create_dir(&db_root).unwrap();
        thegn_core::fsperm::restrict_dir_to_owner(&db_root).unwrap();
        let target = root.path().join("target.db");
        std::fs::write(&target, b"target remains untouched").unwrap();
        let link = db_root.join(subprofile_db_file_name("work"));
        if crate::platform::symlink_file_for_test(&target, &link).is_err() {
            return;
        }

        let mut comms = StubSubsystem {
            name: "comms".into(),
            db_root: Some(db_root),
            active: "default".into(),
            db: None,
            panes: Vec::new(),
        };
        let result = comms.bind("work");
        assert!(matches!(
            result
                .as_ref()
                .err()
                .and_then(|error| error.downcast_ref::<SubprofileStorageRefusal>()),
            Some(SubprofileStorageRefusal::Symlink { .. })
        ));
        assert_eq!(std::fs::read(target).unwrap(), b"target remains untouched");
    }

    #[test]
    fn legacy_slug_database_is_refused() {
        let root = private_temp_root();
        let db_root = root.path().join("comms");
        std::fs::create_dir(&db_root).unwrap();
        thegn_core::fsperm::restrict_dir_to_owner(&db_root).unwrap();
        let legacy = db_root.join("work-a.db");
        std::fs::write(&legacy, b"legacy data").unwrap();
        crate::platform::restrict_file_owner_only(&legacy);

        let mut comms = StubSubsystem {
            name: "comms".into(),
            db_root: Some(db_root),
            active: "default".into(),
            db: None,
            panes: Vec::new(),
        };
        let result = comms.bind("work/a");
        assert!(matches!(
            result
                .as_ref()
                .err()
                .and_then(|error| error.downcast_ref::<SubprofileStorageRefusal>()),
            Some(SubprofileStorageRefusal::LegacySlugDatabase { file }) if file == "work-a.db"
        ));
        assert!(comms.bind("other").is_ok());
    }

    #[test]
    fn hard_linked_database_leaf_is_refused() {
        let root = private_temp_root();
        let db_root = root.path().join("comms");
        std::fs::create_dir(&db_root).unwrap();
        thegn_core::fsperm::restrict_dir_to_owner(&db_root).unwrap();
        let file = subprofile_db_file_name("work");
        let database = db_root.join(&file);
        std::fs::write(&database, b"database data").unwrap();
        std::fs::hard_link(&database, db_root.join("hard-link.db")).unwrap();
        let metadata = std::fs::symlink_metadata(&database).unwrap();
        if thegn_core::fsperm::ensure_single_link_regular_file(&metadata).is_ok() {
            return;
        }

        let mut comms = StubSubsystem {
            name: "comms".into(),
            db_root: Some(db_root),
            active: "default".into(),
            db: None,
            panes: Vec::new(),
        };
        let result = comms.bind("work");
        assert!(matches!(
            result.as_ref().err().and_then(|error| {
                error.downcast_ref::<SubprofileStorageRefusal>()
            }),
            Some(SubprofileStorageRefusal::HardLinked { file: refused }) if refused == &file
        ));
    }

    #[test]
    fn subprofile_root_is_private() {
        let root = private_temp_root();
        let db_root = root.path().join("comms");
        let mut comms = StubSubsystem {
            name: "comms".into(),
            db_root: Some(db_root.clone()),
            active: "default".into(),
            db: None,
            panes: Vec::new(),
        };
        comms.bind("work").unwrap();
        if let Some(mode) = thegn_core::fsperm::mode_bits(&db_root).unwrap() {
            assert_eq!(mode & 0o077, 0);
        }

        let target = root.path().join("target");
        std::fs::create_dir(&target).unwrap();
        thegn_core::fsperm::restrict_dir_to_owner(&target).unwrap();
        let link = root.path().join("comms-link");
        if crate::platform::symlink_file_for_test(&target, &link).is_ok() {
            let mut linked = StubSubsystem {
                name: "comms".into(),
                db_root: Some(link),
                active: "default".into(),
                db: None,
                panes: Vec::new(),
            };
            assert!(linked.bind("work").is_err());
        }
    }
}
