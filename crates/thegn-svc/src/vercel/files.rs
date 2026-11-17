//! The Vercel file API: `fs/write` takes a gzipped ustar (built by
//! `thegn_core::vercel::ustar`), `fs/read` returns one file.

use std::io::Write;

use anyhow::{Result, anyhow};
use futures_util::future::BoxFuture;

use super::VercelProvider;
use crate::provider::{FileEntry, ProviderFiles};

fn not_built() -> anyhow::Error {
    anyhow!("vercel: not implemented in this lane")
}

/// Gzip `data` for the `fs/write` upload body.
#[allow(dead_code)] // spine stub, filled by lane 5
pub(crate) fn gzip(data: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data)?;
    let packed = encoder.finish()?;
    Ok(packed)
}

impl ProviderFiles for VercelProvider {
    fn read<'a>(&'a self, _id: &'a str, _path: &'a str) -> BoxFuture<'a, Result<Vec<u8>>> {
        Box::pin(async move { Err(not_built()) })
    }

    fn write<'a>(
        &'a self,
        _id: &'a str,
        _path: &'a str,
        _data: &'a [u8],
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { Err(not_built()) })
    }

    fn list_dir<'a>(
        &'a self,
        _id: &'a str,
        _path: &'a str,
    ) -> BoxFuture<'a, Result<Vec<FileEntry>>> {
        Box::pin(async move { Err(not_built()) })
    }

    fn delete<'a>(&'a self, _id: &'a str, _path: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { Err(not_built()) })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn gzip_round_trips() {
        use std::io::Read;
        let packed = super::gzip(b"hello vercel").expect("gzip");
        let mut out = String::new();
        flate2::read::GzDecoder::new(packed.as_slice())
            .read_to_string(&mut out)
            .expect("gunzip");
        assert_eq!(out, "hello vercel");
    }
}
