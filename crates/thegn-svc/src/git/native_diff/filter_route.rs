//! Routes worktree clean filters through the owned filter-process registry.
//!
//! gix's persistent-filter client is not suitable for repeated worktree diff
//! reads: the process lifecycle is not owned by thegn. This route captures
//! process drivers before the cache's one-shot reroute, runs clean once through
//! the registry, then applies the remaining encoding/eol/ident pipeline. If a
//! process cannot be used, `None` preserves gix's existing one-shot fallback.
//! Status walks still use gix's internally built pipeline. Idle children remain
//! blocked on their protocol read until the next diff sweeps them or thegn exits.
//! A hung filter can delay one diff by about 30 seconds: a 10-second request,
//! a 10-second respawn handshake, and a 10-second retry, followed by 60 seconds
//! of backoff.

use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use crate::git::filter_process::protocol::Outcome;
use crate::git::filter_process::registry::{self, FilterKey, Routed};
use anyhow::{Context, Result, bail};
use gix::bstr::ByteSlice;

struct ProcessDriver {
    name: String,
    process: String,
    required: bool,
    has_clean: bool,
}

pub(super) struct ProcessDrivers {
    drivers: Vec<ProcessDriver>,
    post_clean: Option<gix::filter::plumbing::Pipeline>,
}

impl ProcessDrivers {
    pub(super) fn capture(platform: &mut gix::diff::blob::Platform) -> Self {
        let drivers = platform
            .filter
            .worktree_filter
            .options_mut()
            .drivers
            .iter()
            .filter_map(|driver| {
                driver.process.as_ref().map(|process| ProcessDriver {
                    name: driver.name.to_string(),
                    process: process.to_string(),
                    required: driver.required,
                    has_clean: driver.clean.is_some(),
                })
            })
            .collect::<Vec<_>>();
        let post_clean = if drivers.is_empty() {
            None
        } else {
            let mut pipeline = platform.filter.worktree_filter.clone();
            for driver in &mut pipeline.options_mut().drivers {
                if driver.process.is_some() {
                    driver.process = None;
                    driver.clean = None;
                }
            }
            Some(pipeline)
        };
        Self {
            drivers,
            post_clean,
        }
    }

    pub(super) fn route_worktree_side(
        &mut self,
        repo: &gix::Repository,
        workdir: &Path,
        rela: &gix::bstr::BStr,
        path: &str,
        platform: &mut gix::diff::blob::Platform,
    ) -> Result<Option<Vec<u8>>> {
        if self.drivers.is_empty() {
            return Ok(None);
        }
        let mut attributes = platform.attr_stack.selected_attribute_matches(["filter"]);
        platform
            .attr_stack
            .at_entry(rela, None, &repo.objects)?
            .matching_attributes(&mut attributes);
        let Some(matched) = attributes.iter_selected().next() else {
            return Ok(None);
        };
        let Some(driver_name) = matched
            .assignment
            .state
            .as_bstr()
            .and_then(|name| name.to_str().ok())
        else {
            return Ok(None);
        };
        let Some(driver) = self
            .drivers
            .iter()
            .find(|driver| driver.name == driver_name)
        else {
            return Ok(None);
        };
        let pathname = workdir.join(path);
        let metadata = match std::fs::metadata(&pathname) {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => return Ok(None),
        };
        let limit = platform.filter.options.large_file_threshold_bytes;
        if limit > 0 && metadata.len() > limit {
            return Ok(None);
        }
        let content: Arc<[u8]> = Arc::from(
            std::fs::read(&pathname)
                .with_context(|| format!("read filter input {}", pathname.display()))?,
        );
        let process_command = driver.process.clone();
        let spawn_workdir = workdir.to_owned();
        let routed = registry::global().clean(
            FilterKey {
                workdir: workdir.to_owned(),
                process: driver.process.clone(),
            },
            || {
                let command_context = repo.command_context().map_err(std::io::Error::other)?;
                let mut command: std::process::Command = gix::command::prepare(&process_command)
                    .command_may_be_shell_script()
                    .with_context(command_context)
                    .into();
                command.current_dir(&spawn_workdir);
                Ok(command)
            },
            path,
            Arc::clone(&content),
        );
        match routed {
            Routed::Unavailable => Ok(None),
            Routed::Done(Outcome::Success(bytes)) => self
                .convert_post_clean(repo, platform, path, &bytes)
                .map(Some),
            Routed::Done(Outcome::Error | Outcome::Abort) if driver.required => {
                bail!("required clean filter {:?} failed for {path}", driver.name)
            }
            Routed::Done(Outcome::Error | Outcome::Abort) => {
                tracing::debug!(target: "thegn::git", driver = %driver.name, has_clean = driver.has_clean, "non-required clean filter passed through");
                self.convert_post_clean(repo, platform, path, &content[..])
                    .map(Some)
            }
        }
    }

    fn convert_post_clean(
        &mut self,
        repo: &gix::Repository,
        platform: &mut gix::diff::blob::Platform,
        path: &str,
        bytes: &[u8],
    ) -> Result<Vec<u8>> {
        let Some(pipeline) = &mut self.post_clean else {
            return Ok(bytes.to_vec());
        };
        let rela = Path::new(path);
        let mut lookup_error = None;
        let outcome = pipeline.convert_to_git(
            bytes,
            rela,
            &mut |rela, attrs| match platform.attr_stack.at_entry(rela, None, &repo.objects) {
                Ok(entry) => entry.matching_attributes(attrs),
                Err(error) => {
                    if lookup_error.is_none() {
                        lookup_error = Some(error);
                    }
                }
            },
            &mut |_| Ok(None),
        )?;
        if let Some(error) = lookup_error {
            return Err(error.into());
        }
        match outcome {
            gix::filter::plumbing::pipeline::convert::ToGitOutcome::Unchanged(mut reader) => {
                let mut output = Vec::new();
                reader.read_to_end(&mut output)?;
                Ok(output)
            }
            gix::filter::plumbing::pipeline::convert::ToGitOutcome::Buffer(output) => {
                Ok(output.to_vec())
            }
            gix::filter::plumbing::pipeline::convert::ToGitOutcome::Process(mut reader) => {
                let mut output = Vec::new();
                reader.read_to_end(&mut output)?;
                Ok(output)
            }
        }
    }
}

pub(super) struct Overlay<'a> {
    pub(super) id: gix::ObjectId,
    pub(super) bytes: &'a [u8],
    pub(super) inner: &'a gix::OdbHandle,
}

impl gix::objs::Find for Overlay<'_> {
    fn try_find<'a>(
        &self,
        id: &gix::hash::oid,
        buffer: &'a mut Vec<u8>,
    ) -> Result<Option<gix::objs::Data<'a>>, gix::objs::find::Error> {
        if id == self.id.as_ref() {
            buffer.clear();
            buffer.extend_from_slice(self.bytes);
            return Ok(Some(gix::objs::Data {
                kind: gix::objs::Kind::Blob,
                object_hash: id.kind(),
                data: buffer.as_slice(),
            }));
        }
        self.inner.try_find(id, buffer)
    }
}

impl gix::objs::FindHeader for Overlay<'_> {
    fn try_header(
        &self,
        id: &gix::hash::oid,
    ) -> Result<Option<gix::objs::Header>, gix::objs::find::Error> {
        if id == self.id.as_ref() {
            return Ok(Some(gix::objs::Header {
                kind: gix::objs::Kind::Blob,
                size: self.bytes.len() as u64,
            }));
        }
        self.inner.try_header(id)
    }
}
