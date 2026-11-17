//! A pure, hand-rolled ustar writer for the Vercel `fs/write` upload (the
//! `bundle.rs` precedent: no tar crate).

use std::fmt;

/// One file to archive. `path` is relative; `mode` is the unix permission bits.
#[derive(Debug, Clone, Copy)]
pub struct TarEntry<'a> {
    pub path: &'a str,
    pub data: &'a [u8],
    pub mode: u32,
}

/// Why an archive was refused (a fixed reason, never the offending path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UstarError(pub &'static str);

impl fmt::Display for UstarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for UstarError {}

/// Build a ustar archive. Rejects absolute paths, `..`, NUL, and names longer
/// than 100 bytes that cannot be split into a ustar prefix. Spine stub, filled
/// by lane 2.
pub fn ustar(_entries: &[TarEntry<'_>]) -> Result<Vec<u8>, UstarError> {
    Err(UstarError("vercel: not implemented in this lane"))
}
