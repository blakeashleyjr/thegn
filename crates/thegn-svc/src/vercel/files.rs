//! The Vercel file API: `fs/write` takes a gzipped ustar (built by
//! `thegn_core::vercel::ustar`), `fs/read` returns one file.
//!
//! Every call here resolves the session with [`Wake::Allow`]: file transfer is
//! reached only from explicit open/provision/pull paths (the generic
//! `upload_dir`/`download_dir` defaults ride `write`/`list_dir`/`read`), never
//! from chrome reads, the reaper or the fleet inventory.

use std::io::Write;

use anyhow::{Context, Result, anyhow, bail, ensure};
use futures_util::future::BoxFuture;
use thegn_core::vercel::ustar::{TarEntry, ustar};

use super::{VercelProvider, Wake};
use crate::provider::{FileEntry, ProviderFiles};

/// Body cap for one file read or write. Mirrors provider.rs's private
/// `FILE_TRANSFER_BODY_LIMIT` (the same bound every provider's file API uses).
pub(crate) const FILE_TRANSFER_BODY_LIMIT: usize = 256 * 1024 * 1024;

/// Regular-file mode for [`ProviderFiles::write`].
const MODE_FILE: u32 = 0o644;
/// Executable mode for [`ProviderFiles::write_exec`].
const MODE_EXEC: u32 = 0o755;

/// The fixed `list_dir` script. `$1` is the directory (passed as argv, never
/// interpolated). Prints `type\tsize\tname` records, NUL-terminated; `type` is
/// `d` (a real directory) or `f` (a regular file). Symlinks and special files
/// are skipped, so a download never follows a link out of the tree.
const LIST_DIR_SCRIPT: &str = r#"cd -- "$1" || exit 2
for f in * .[!.]* ..?*; do
  if [ -L "$f" ]; then continue; fi
  if [ -d "$f" ]; then
    printf 'd\t0\t%s\0' "$f"
  elif [ -f "$f" ]; then
    s=$(wc -c < "$f" 2>/dev/null) || continue
    printf 'f\t%s\t%s\0' "$s" "$f"
  fi
done
"#;

/// Gzip `data` for the `fs/write` upload body.
pub(crate) fn gzip(data: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data)?;
    let packed = encoder.finish()?;
    Ok(packed)
}

/// The same rules as provider.rs's private `valid_remote_component`: a single
/// path component that cannot escape or alias its parent on any host.
pub(crate) fn valid_remote_component(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.chars().any(|ch| matches!(ch, '/' | '\\' | '\0'))
        && !name.chars().any(char::is_control)
        && !(name.len() >= 2
            && name.as_bytes()[0].is_ascii_alphabetic()
            && name.as_bytes()[1] == b':')
}

/// An absolute, NUL-free sandbox path (the precondition for every call that
/// hands `path` to the sandbox).
fn check_abs_path(path: &str) -> Result<()> {
    ensure!(
        path.starts_with('/'),
        "vercel: sandbox file path must be absolute"
    );
    ensure!(
        !path.contains('\0'),
        "vercel: sandbox file path contains NUL"
    );
    Ok(())
}

/// Split an absolute file path into `(parent dir, basename)`. The basename
/// must be a valid single component; `..` segments anywhere are refused.
pub(crate) fn split_parent(path: &str) -> Result<(String, String)> {
    check_abs_path(path)?;
    let trimmed = path.trim_end_matches('/');
    ensure!(
        !trimmed.split('/').any(|seg| seg == ".."),
        "vercel: sandbox file path must not contain `..`"
    );
    let (parent, base) = trimmed
        .rsplit_once('/')
        .ok_or_else(|| anyhow!("vercel: sandbox file path has no parent"))?;
    ensure!(
        valid_remote_component(base),
        "vercel: sandbox file name is not a valid path component"
    );
    let parent = if parent.is_empty() { "/" } else { parent };
    Ok((parent.to_string(), base.to_string()))
}

/// The gzipped one-entry ustar `fs/write` takes: `base` with `mode`, to be
/// extracted with cwd = the parent dir.
pub(crate) fn single_file_targz(base: &str, data: &[u8], mode: u32) -> Result<Vec<u8>> {
    ensure!(
        data.len() <= FILE_TRANSFER_BODY_LIMIT,
        "vercel file write exceeded the {FILE_TRANSFER_BODY_LIMIT}-byte limit"
    );
    let tar = ustar(&[TarEntry {
        path: base,
        data,
        mode,
    }])
    .map_err(|e| anyhow!("vercel: build upload archive: {e}"))?;
    gzip(&tar)
}

/// Parse the [`LIST_DIR_SCRIPT`] output. Any unsafe name, unknown type or
/// malformed record fails the whole listing (the provider.rs download walk
/// contract: an unsafe component is an error, never a silent skip).
pub(crate) fn parse_list_output(out: &str) -> Result<Vec<FileEntry>> {
    let mut entries = Vec::new();
    for record in out.split('\0') {
        if record.is_empty() {
            continue;
        }
        let mut parts = record.splitn(3, '\t');
        let (Some(kind), Some(size), Some(name)) = (parts.next(), parts.next(), parts.next())
        else {
            bail!("vercel: malformed directory listing record");
        };
        ensure!(
            valid_remote_component(name),
            "provider returned an unsafe path component"
        );
        let is_dir = match kind {
            "d" => true,
            "f" => false,
            _ => bail!("vercel: unknown directory entry type"),
        };
        let size = size
            .trim()
            .parse::<u64>()
            .map_err(|_| anyhow!("vercel: malformed directory entry size"))?;
        entries.push(FileEntry {
            name: name.to_string(),
            is_dir,
            size,
        });
    }
    Ok(entries)
}

impl VercelProvider {
    /// Upload one file with `mode`. Creates the parent dir and retries once if
    /// the first extract fails (a tar extract needs its cwd to exist).
    async fn write_mode(&self, id: &str, path: &str, data: &[u8], mode: u32) -> Result<()> {
        let (parent, base) = split_parent(path)?;
        let targz = single_file_targz(&base, data, mode)?;
        let session = self.session_for(id, Wake::Allow).await?;
        match self
            .client
            .fs_write_targz(&session, &parent, targz.clone())
            .await
        {
            Ok(()) => Ok(()),
            Err(first) => {
                let argv = ["mkdir", "-p", "--", parent.as_str()].map(String::from);
                let (code, _) = self
                    .run_exec(id, &argv, None, &[], Wake::Allow)
                    .await
                    .context("vercel: create upload parent dir")?;
                if code != 0 {
                    return Err(first.context("vercel: upload parent dir is not creatable"));
                }
                let session = self.session_for(id, Wake::Allow).await?;
                self.client.fs_write_targz(&session, &parent, targz).await
            }
        }
    }
}

impl ProviderFiles for VercelProvider {
    fn read<'a>(&'a self, id: &'a str, path: &'a str) -> BoxFuture<'a, Result<Vec<u8>>> {
        Box::pin(async move {
            check_abs_path(path)?;
            let session = self.session_for(id, Wake::Allow).await?;
            self.client
                .fs_read(&session, path, FILE_TRANSFER_BODY_LIMIT)
                .await?
                .ok_or_else(|| anyhow!("vercel: sandbox file not found"))
        })
    }

    fn write<'a>(
        &'a self,
        id: &'a str,
        path: &'a str,
        data: &'a [u8],
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.write_mode(id, path, data, MODE_FILE).await })
    }

    fn write_exec<'a>(
        &'a self,
        id: &'a str,
        path: &'a str,
        data: &'a [u8],
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.write_mode(id, path, data, MODE_EXEC).await })
    }

    fn list_dir<'a>(&'a self, id: &'a str, path: &'a str) -> BoxFuture<'a, Result<Vec<FileEntry>>> {
        Box::pin(async move {
            check_abs_path(path)?;
            let argv = ["/bin/sh", "-c", LIST_DIR_SCRIPT, "sh", path].map(String::from);
            let (code, out) = self.run_exec(id, &argv, None, &[], Wake::Allow).await?;
            ensure!(
                code == 0,
                "vercel: list sandbox directory failed (exit {code})"
            );
            parse_list_output(&out)
        })
    }

    fn delete<'a>(&'a self, id: &'a str, path: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            check_abs_path(path)?;
            let argv = ["rm", "-f", "--", path].map(String::from);
            let (code, _) = self.run_exec(id, &argv, None, &[], Wake::Allow).await?;
            ensure!(
                code == 0,
                "vercel: delete sandbox file failed (exit {code})"
            );
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn gzip_round_trips() {
        let packed = super::gzip(b"hello vercel").expect("gzip");
        let mut out = String::new();
        flate2::read::GzDecoder::new(packed.as_slice())
            .read_to_string(&mut out)
            .expect("gunzip");
        assert_eq!(out, "hello vercel");
    }

    /// Parse a NUL-terminated octal ustar header field.
    fn octal(field: &[u8]) -> u64 {
        let text: String = field
            .iter()
            .take_while(|b| **b != 0 && **b != b' ')
            .map(|b| *b as char)
            .collect();
        u64::from_str_radix(text.trim(), 8).expect("octal field")
    }

    fn decode_single(targz: &[u8]) -> (String, u32, Vec<u8>) {
        let mut tar = Vec::new();
        flate2::read::GzDecoder::new(targz)
            .read_to_end(&mut tar)
            .expect("gunzip");
        assert!(
            tar.len() >= 512 && tar.len() % 512 == 0,
            "ustar is 512-blocked"
        );
        let header = &tar[..512];
        assert_eq!(&header[257..262], b"ustar", "ustar magic");
        let name: String = header[..100]
            .iter()
            .take_while(|b| **b != 0)
            .map(|b| *b as char)
            .collect();
        let mode = octal(&header[100..108]) as u32;
        let size = octal(&header[124..136]) as usize;
        assert_eq!(header[156], b'0', "regular file typeflag");
        let data = tar[512..512 + size].to_vec();
        (name, mode, data)
    }

    #[test]
    fn write_archive_has_the_basename_and_file_mode() {
        let (parent, base) = split_parent("/vercel/repo/notes.txt").expect("split");
        assert_eq!(parent, "/vercel/repo");
        assert_eq!(base, "notes.txt");
        let targz = single_file_targz(&base, b"hello", MODE_FILE).expect("targz");
        let (name, mode, data) = decode_single(&targz);
        assert_eq!(name, "notes.txt");
        assert_eq!(mode & 0o7777, 0o644);
        assert_eq!(data, b"hello");
    }

    #[test]
    fn write_exec_archive_has_the_exec_mode() {
        let targz = single_file_targz("run.sh", b"#!/bin/sh\n", MODE_EXEC).expect("targz");
        let (name, mode, data) = decode_single(&targz);
        assert_eq!(name, "run.sh");
        assert_eq!(mode & 0o7777, 0o755);
        assert_eq!(data, b"#!/bin/sh\n");
    }

    #[test]
    fn split_parent_refuses_relative_dotdot_nul_and_root() {
        assert_eq!(
            split_parent("/top.txt").expect("root child"),
            ("/".to_string(), "top.txt".to_string())
        );
        for bad in ["rel/x", "/a/../b", "/a/..", "/a/b\0c", "/", ""] {
            assert!(split_parent(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn list_output_parses_files_and_dirs() {
        let out = "d\t0\tsrc\0f\t12\tREADME.md\0f\t   7\t.env.example\0";
        let entries = parse_list_output(out).expect("parse");
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].name, "src");
        assert!(entries[0].is_dir);
        assert_eq!(entries[1].name, "README.md");
        assert_eq!(entries[1].size, 12);
        assert!(!entries[1].is_dir);
        assert_eq!(entries[2].size, 7);
        assert!(parse_list_output("").expect("empty").is_empty());
    }

    #[test]
    fn list_output_rejects_unsafe_names() {
        for out in [
            "f\t1\t../escape\0",
            "f\t1\t..\0",
            "d\t0\t.\0",
            "f\t1\ta/b\0",
            "f\t1\ta\\b\0",
            "f\t1\tbad\nname\0",
            "f\t1\tC:evil\0",
            "x\t1\tok\0",
            "f\tnope\tok\0",
            "f\t1\0",
        ] {
            assert!(parse_list_output(out).is_err(), "accepted {out:?}");
        }
    }
}
