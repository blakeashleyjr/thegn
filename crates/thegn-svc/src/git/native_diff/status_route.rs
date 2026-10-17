//! Verify required persistent-filter status candidates through the owned client.
//!
//! gix's status iterator has no filter hook and hard-codes raw blob comparison.
//! Its repository clone therefore strips `process` and `clean` for the exact
//! required drivers handled here, then this route removes false positives by
//! cleaning candidate files through the shared process registry. Unavailable
//! registry routes fall back to the cache's waited one-shot pipeline. Diff
//! paths may be cleaned once during status verification and again during diff;
//! carrying bytes across that boundary is deferred. Non-required and clean-less
//! process drivers keep gix's existing behavior (THE-701). Because `clean` is
//! stripped, raw bytes matching the index blob are reported clean even when a
//! non-idempotent clean filter would change them, unlike Git. A hung routed
//! filter can block `is_dirty` for about 30 seconds across request, respawn,
//! and retry budgets; the read deadline cannot interrupt that wait.

use anyhow::{Context, Result};
use gix::bstr::ByteSlice;

use super::{ReapingCache, filter_route};

pub(in crate::git) struct StatusRoute {
    repo: gix::Repository,
    workdir: std::path::PathBuf,
    state: Option<(filter_route::ProcessDrivers, ReapingCache)>,
}

impl StatusRoute {
    pub(super) fn new(repo: gix::Repository, workdir: std::path::PathBuf) -> Self {
        Self {
            repo,
            workdir,
            state: None,
        }
    }

    pub(crate) fn keep(&mut self, item: &gix::status::Item) -> Result<bool> {
        let gix::status::Item::IndexWorktree(gix::status::index_worktree::Item::Modification {
            entry,
            rela_path,
            status:
                gix::status::plumbing::index_as_worktree::EntryStatus::Change(
                    gix::status::plumbing::index_as_worktree::Change::Modification {
                        executable_bit_changed: false,
                        content_change: Some(()),
                        ..
                    },
                ),
            ..
        }) = item
        else {
            return Ok(true);
        };
        let Some(path) = rela_path.to_str().ok() else {
            return Ok(true);
        };
        let pathname = self.workdir.join(path);
        let metadata = match std::fs::symlink_metadata(&pathname) {
            Ok(metadata) if metadata.file_type().is_file() => metadata,
            _ => return Ok(true),
        };
        if size_differs(entry.stat.size, entry.id.is_empty_blob(), metadata.len()) {
            return Ok(true);
        }

        if self.state.is_none() {
            let mut platform = self
                .repo
                .diff_resource_cache(
                    gix::diff::blob::pipeline::Mode::ToGit,
                    gix::diff::blob::pipeline::WorktreeRoots {
                        old_root: None,
                        new_root: Some(self.workdir.clone()),
                    },
                )
                .context("status filter resource cache")?;
            let mut drivers = filter_route::ProcessDrivers::capture(&mut platform);
            drivers.retain_rerouted();
            self.state = Some((drivers, super::new_cache(platform)));
        }

        let Some((drivers, cache)) = self.state.as_mut() else {
            return Ok(true);
        };
        let cleaned = match drivers.route_status_side(
            &self.repo,
            &self.workdir,
            rela_path,
            path,
            &mut cache.0,
        )? {
            filter_route::StatusClean::Cleaned(bytes) => bytes,
            filter_route::StatusClean::NotRouted => return Ok(true),
            filter_route::StatusClean::Unavailable => {
                let raw = std::fs::read(&pathname)
                    .with_context(|| format!("read status filter input {}", pathname.display()))?;
                let platform = &mut cache.0;
                filter_route::ProcessDrivers::convert_pipeline(
                    &self.repo,
                    &mut platform.filter.worktree_filter,
                    &mut platform.attr_stack,
                    path,
                    &raw,
                )?
            }
        };
        let id = gix::objs::compute_hash(entry.id.kind(), gix::objs::Kind::Blob, &cleaned)?;
        Ok(id != entry.id)
    }
}

pub(super) fn size_differs(entry_size: u32, empty_blob: bool, len: u64) -> bool {
    u64::from(entry_size) != len && (empty_blob || entry_size != 0)
}
