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

const BLOCK: usize = 512;

fn put_octal(buf: &mut [u8], value: u64) -> Result<(), UstarError> {
    // `len-1` octal digits plus a NUL terminator.
    let digits = buf.len() - 1;
    let text = format!("{value:0digits$o}");
    if text.len() > digits {
        return Err(UstarError("vercel: tar field overflow"));
    }
    buf[..digits].copy_from_slice(text.as_bytes());
    buf[digits] = 0;
    Ok(())
}

/// Split `path` into a (prefix, name) pair fitting ustar's 155 + 100 bytes.
fn split_path(path: &str) -> Result<(&str, &str), UstarError> {
    if path.len() <= 100 {
        return Ok(("", path));
    }
    for (i, _) in path.match_indices('/') {
        let (prefix, name) = (&path[..i], &path[i + 1..]);
        if prefix.len() <= 155 && !name.is_empty() && name.len() <= 100 {
            return Ok((prefix, name));
        }
    }
    Err(UstarError("vercel: tar path too long"))
}

fn check_path(path: &str) -> Result<(), UstarError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\0')
        || path.ends_with('/')
        || path.split('/').any(|c| c == ".." || c.is_empty())
    {
        return Err(UstarError("vercel: unsafe tar path"));
    }
    Ok(())
}

/// Build a ustar archive. Rejects absolute paths, `..`, NUL, and names longer
/// than 100 bytes that cannot be split into a ustar prefix.
pub fn ustar(entries: &[TarEntry<'_>]) -> Result<Vec<u8>, UstarError> {
    let mut out = Vec::new();
    for e in entries {
        check_path(e.path)?;
        let (prefix, name) = split_path(e.path)?;
        let mut h = [0u8; BLOCK];
        h[..name.len()].copy_from_slice(name.as_bytes());
        put_octal(&mut h[100..108], u64::from(e.mode & 0o7777))?;
        put_octal(&mut h[108..116], 0)?; // uid
        put_octal(&mut h[116..124], 0)?; // gid
        put_octal(&mut h[124..136], e.data.len() as u64)?;
        put_octal(&mut h[136..148], 0)?; // mtime: reproducible
        h[148..156].fill(b' '); // checksum placeholder
        h[156] = b'0';
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        h[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
        let sum: u32 = h.iter().map(|b| u32::from(*b)).sum();
        // Six octal digits, NUL, space.
        h[148..154].copy_from_slice(format!("{sum:06o}").as_bytes());
        h[154] = 0;
        h[155] = b' ';
        out.extend_from_slice(&h);
        out.extend_from_slice(e.data);
        out.resize(out.len().div_ceil(BLOCK) * BLOCK, 0);
    }
    out.resize(out.len() + 2 * BLOCK, 0);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn octal(field: &[u8]) -> u64 {
        let s: String = field
            .iter()
            .take_while(|b| **b != 0 && **b != b' ')
            .map(|b| *b as char)
            .collect();
        u64::from_str_radix(&s, 8).unwrap()
    }

    #[test]
    fn round_trip_header_and_checksum() {
        let data = b"hello";
        let tar = ustar(&[TarEntry {
            path: "a.txt",
            data,
            mode: 0o644,
        }])
        .unwrap();
        // header + 1 data block + 2 zero blocks
        assert_eq!(tar.len(), 4 * 512);
        let h = &tar[..512];
        assert_eq!(&h[..5], b"a.txt");
        assert_eq!(octal(&h[100..108]), 0o644);
        assert_eq!(octal(&h[124..136]), 5);
        assert_eq!(h[156], b'0');
        assert_eq!(&h[257..263], b"ustar\0");
        assert_eq!(&h[263..265], b"00");
        // Hand-computed checksum: sum of all header bytes with the checksum
        // field read as eight spaces.
        let mut want: u32 = 8 * 32;
        for (i, b) in h.iter().enumerate() {
            if !(148..156).contains(&i) {
                want += u32::from(*b);
            }
        }
        // Independent expectation: "a.txt" + mode/uid/gid/size/mtime digits +
        // typeflag '0' + "ustar\0" + "00".
        let mut manual: u32 = 8 * 32;
        manual += b"a.txt".iter().map(|b| u32::from(*b)).sum::<u32>();
        manual += b"0000644".iter().map(|b| u32::from(*b)).sum::<u32>(); // mode
        manual += b"0000000".iter().map(|b| u32::from(*b)).sum::<u32>() * 2; // uid, gid
        manual += b"00000000005".iter().map(|b| u32::from(*b)).sum::<u32>(); // size
        manual += b"00000000000".iter().map(|b| u32::from(*b)).sum::<u32>(); // mtime
        manual += u32::from(b'0');
        manual += b"ustar".iter().map(|b| u32::from(*b)).sum::<u32>();
        manual += b"00".iter().map(|b| u32::from(*b)).sum::<u32>();
        assert_eq!(want, manual);
        assert_eq!(octal(&h[148..156]), u64::from(want));
        assert_eq!(&tar[512..517], data);
        assert!(tar[517..1024].iter().all(|b| *b == 0));
        assert!(tar[1024..].iter().all(|b| *b == 0));
    }

    #[test]
    fn empty_archive_is_two_zero_blocks() {
        assert_eq!(ustar(&[]).unwrap(), vec![0u8; 1024]);
    }

    #[test]
    fn exact_block_data_and_multiple_entries() {
        let big = vec![7u8; 512];
        let tar = ustar(&[
            TarEntry {
                path: "x",
                data: &big,
                mode: 0o755,
            },
            TarEntry {
                path: "d/y",
                data: b"",
                mode: 0o600,
            },
        ])
        .unwrap();
        assert_eq!(tar.len(), 512 + 512 + 512 + 1024);
        assert_eq!(octal(&tar[100..108]), 0o755);
        assert_eq!(&tar[1024..1027], b"d/y");
        assert_eq!(octal(&tar[1024 + 124..1024 + 136]), 0);
    }

    #[test]
    fn long_path_splits_into_prefix() {
        let dir = "d".repeat(120);
        let path = format!("{dir}/{}", "f".repeat(50));
        let tar = ustar(&[TarEntry {
            path: &path,
            data: b"z",
            mode: 0o644,
        }])
        .unwrap();
        assert_eq!(&tar[345..345 + 120], dir.as_bytes());
        assert_eq!(&tar[..50], "f".repeat(50).as_bytes());
    }

    #[test]
    fn unsafe_and_overlong_paths_refused() {
        let e = |p: &str| {
            ustar(&[TarEntry {
                path: p,
                data: b"",
                mode: 0,
            }])
            .is_err()
        };
        assert!(e(""));
        assert!(e("/abs"));
        assert!(e("../x"));
        assert!(e("a/../x"));
        assert!(e("a//b"));
        assert!(e("a/"));
        assert!(e("a\0b"));
        assert!(e(&"n".repeat(101))); // no slash to split on
        assert!(e(&format!("{}/{}", "p".repeat(156), "n".repeat(10))));
        assert!(e(&format!("a/{}", "n".repeat(101))));
        let msg = ustar(&[TarEntry {
            path: "/secret-path",
            data: b"",
            mode: 0,
        }])
        .unwrap_err()
        .to_string();
        assert!(!msg.contains("secret-path"));
    }

    #[test]
    fn mode_overflow_masked_and_octal_overflow_errors() {
        let tar = ustar(&[TarEntry {
            path: "m",
            data: b"",
            mode: 0o100644,
        }])
        .unwrap();
        assert_eq!(octal(&tar[100..108]), 0o644);
        let mut buf = [0u8; 3];
        assert!(put_octal(&mut buf, 0o777).is_err());
        assert!(put_octal(&mut buf, 0o77).is_ok());
    }
}
