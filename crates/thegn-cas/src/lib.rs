//! SHA-256 CAS identities and canonical REAPI Directory encoding.

use anyhow::{Result, ensure};
use prost::Message;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "WireDigest", deny_unknown_fields, rename_all = "camelCase")]
pub struct Digest {
    #[schemars(regex(pattern = "^[0-9a-f]{64}$"))]
    hash: String,
    #[schemars(range(min = 0))]
    size_bytes: i64,
}

impl Digest {
    pub fn of(bytes: &[u8]) -> Self {
        Self {
            hash: hex::encode(Sha256::digest(bytes)),
            size_bytes: i64::try_from(bytes.len()).expect("slice length fits isize"),
        }
    }

    pub fn hash(&self) -> &str {
        &self.hash
    }

    pub fn size_bytes(&self) -> i64 {
        self.size_bytes
    }
}

// Protobuf field numbers follow build.bazel.remote.execution.v2.
// Job JSON uses the same hash and size, with a numeric sizeBytes.
#[derive(Clone, PartialEq, Message, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct WireDigest {
    #[prost(string, tag = "1")]
    hash: String,
    #[prost(int64, tag = "2")]
    size_bytes: i64,
}

impl TryFrom<WireDigest> for Digest {
    type Error = anyhow::Error;

    fn try_from(value: WireDigest) -> Result<Self> {
        ensure!(value.size_bytes >= 0, "sizeBytes must be nonnegative");
        ensure!(
            value.hash.len() == 64
                && value
                    .hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "hash must be a lowercase SHA-256 hex digest"
        );
        Ok(Self {
            hash: value.hash,
            size_bytes: value.size_bytes,
        })
    }
}

impl From<Digest> for WireDigest {
    fn from(digest: Digest) -> Self {
        Self {
            hash: digest.hash,
            size_bytes: digest.size_bytes,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    File { digest: Digest, executable: bool },
    Directory(Digest),
    Symlink { target: String },
}

#[derive(Clone, PartialEq, Message)]
struct FileNode {
    #[prost(string, tag = "1")]
    name: String,
    #[prost(message, required, tag = "2")]
    digest: WireDigest,
    #[prost(bool, tag = "4")]
    is_executable: bool,
}

#[derive(Clone, PartialEq, Message)]
struct DirectoryNode {
    #[prost(string, tag = "1")]
    name: String,
    #[prost(message, required, tag = "2")]
    digest: WireDigest,
}

#[derive(Clone, PartialEq, Message)]
struct SymlinkNode {
    #[prost(string, tag = "1")]
    name: String,
    #[prost(string, tag = "2")]
    target: String,
}

#[derive(Clone, PartialEq, Message)]
struct Directory {
    #[prost(message, repeated, tag = "1")]
    files: Vec<FileNode>,
    #[prost(message, repeated, tag = "2")]
    directories: Vec<DirectoryNode>,
    #[prost(message, repeated, tag = "3")]
    symlinks: Vec<SymlinkNode>,
}

/// Encodes one directory of an already captured tree. Capture and extraction must
/// check symlink resolution against the full tree, including intermediate links.
pub fn encode_directory(mut entries: Vec<(String, Entry)>) -> Result<Vec<u8>> {
    entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    ensure!(
        !entries.windows(2).any(|pair| pair[0].0 == pair[1].0),
        "duplicate directory entry"
    );
    let mut directory = Directory::default();
    for (name, entry) in entries {
        ensure!(
            !matches!(name.as_str(), "" | "." | "..") && !name.contains(['/', '\\', '\0', ':']),
            "invalid portable directory entry {name:?}"
        );
        match entry {
            Entry::File { digest, executable } => directory.files.push(FileNode {
                name,
                digest: digest.into(),
                is_executable: executable,
            }),
            Entry::Directory(digest) => directory.directories.push(DirectoryNode {
                name,
                digest: digest.into(),
            }),
            Entry::Symlink { target } => {
                ensure!(
                    !target.is_empty()
                        && !target.starts_with('/')
                        && !target.contains(['\\', '\0', ':']),
                    "expected a relative symlink target"
                );
                directory.symlinks.push(SymlinkNode { name, target });
            }
        }
    }
    Ok(directory.encode_to_vec())
}
