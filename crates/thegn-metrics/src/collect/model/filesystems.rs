//! Per-superblock filesystem usage readings.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilesystemKind {
    Local,
    Network,
    Pseudo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesystemRow {
    pub mount_point: String,
    pub fstype: String,
    pub source: String,
    pub superblock_dev: String,
    /// Further mount points sharing this superblock that were folded into this row.
    pub extra_mounts: u32,
    pub kind: FilesystemKind,
    pub total_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
    pub avail_bytes: Option<u64>,
    pub total_inodes: Option<u64>,
    pub free_inodes: Option<u64>,
    pub avail_inodes: Option<u64>,
    pub btrfs_unallocated: Option<u64>,
    pub read_only: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilesystemsPart {
    pub rows: Vec<FilesystemRow>,
    pub truncated: bool,
}

impl crate::collect::frame::Part for FilesystemsPart {
    fn clear(&mut self) {
        self.rows.clear();
        self.truncated = false;
    }
}
