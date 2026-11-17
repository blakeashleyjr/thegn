//! Bounded, pure entry listing for zip and tar archives (preview pane).
//!
//! Parses untrusted bytes through `Read`/`Seek` generics and executes nothing.
//! Every offset is bounds-checked with checked arithmetic; memory and scan
//! work are capped by the caller-supplied limits.

use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};

/// What an archive entry is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    /// Symlink with its link target.
    Symlink(String),
    Other,
}

/// One archive entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub size: Option<u64>,
    pub kind: EntryKind,
    /// Zip general-purpose flag bit 0 (encrypted entry).
    pub encrypted: bool,
}

/// Why a listing stopped early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Truncation {
    MaxEntries,
    ScanLimit,
    Cancelled,
}

/// A bounded listing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Listing {
    pub entries: Vec<Entry>,
    /// Total entry count when known up front (zip EOCD).
    pub total_hint: Option<u64>,
    pub truncated: Option<Truncation>,
}

const EOCD_SIG: u32 = 0x0605_4b50;
const CDH_SIG: u32 = 0x0201_4b50;
const EOCD_MIN: usize = 22;
const EOCD_MAX_SEARCH: u64 = 65_557;
const CD_CAP: u64 = 4 * 1024 * 1024;
const CDH_FIXED: usize = 46;
const TAR_LONGNAME_MAX: u64 = 4096;

fn le16(b: &[u8], at: usize) -> Option<u16> {
    let s = b.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// Read the zip central directory: `(directory bytes, declared entry count)`.
fn zip_read_cd<R: Read + Seek>(r: &mut R, file_len: u64) -> Result<(Vec<u8>, u16), String> {
    if file_len < EOCD_MIN as u64 {
        return Err("not a zip archive: too short".into());
    }
    let tail_len = file_len.min(EOCD_MAX_SEARCH);
    let tail_start = file_len - tail_len;
    r.seek(SeekFrom::Start(tail_start))
        .map_err(|e| format!("zip seek failed: {e}"))?;
    let mut tail = vec![0u8; usize::try_from(tail_len).map_err(|_| "zip tail too large")?];
    r.read_exact(&mut tail)
        .map_err(|e| format!("zip read failed: {e}"))?;
    let mut pos = None;
    let mut i = tail.len().saturating_sub(EOCD_MIN);
    loop {
        if le32(&tail, i) == Some(EOCD_SIG) {
            pos = Some(i);
            break;
        }
        if i == 0 {
            break;
        }
        i -= 1;
    }
    let eocd = pos.ok_or("zip end-of-central-directory not found")?;
    let total_entries = le16(&tail, eocd + 10).ok_or("zip EOCD truncated")?;
    let cd_size = le32(&tail, eocd + 12).ok_or("zip EOCD truncated")?;
    let cd_offset = le32(&tail, eocd + 16).ok_or("zip EOCD truncated")?;
    if total_entries == 0xFFFF || cd_size == 0xFFFF_FFFF || cd_offset == 0xFFFF_FFFF {
        return Err("zip64 archive: listing not supported".into());
    }
    let (cd_size, cd_offset) = (u64::from(cd_size), u64::from(cd_offset));
    let cd_end = cd_offset
        .checked_add(cd_size)
        .ok_or("zip central directory out of range")?;
    if cd_end > file_len {
        return Err("zip central directory out of range".into());
    }
    if cd_size > CD_CAP {
        return Err("zip central directory too large".into());
    }
    r.seek(SeekFrom::Start(cd_offset))
        .map_err(|e| format!("zip seek failed: {e}"))?;
    let mut cd = vec![0u8; usize::try_from(cd_size).map_err(|_| "zip directory too large")?];
    r.read_exact(&mut cd)
        .map_err(|e| format!("zip read failed: {e}"))?;
    Ok((cd, total_entries))
}

/// List a zip archive from its central directory.
pub fn zip_listing<R: Read + Seek>(
    r: &mut R,
    file_len: u64,
    max_entries: usize,
) -> Result<Listing, String> {
    let (cd, total_entries) = zip_read_cd(r, file_len)?;
    let mut listing = Listing {
        total_hint: Some(u64::from(total_entries)),
        ..Listing::default()
    };
    let mut at = 0usize;
    for _ in 0..total_entries {
        if listing.entries.len() >= max_entries {
            listing.truncated = Some(Truncation::MaxEntries);
            break;
        }
        if le32(&cd, at) != Some(CDH_SIG) {
            return Err("zip central directory corrupt".into());
        }
        let flags = le16(&cd, at + 8).ok_or("zip central directory truncated")?;
        let usize_ = le32(&cd, at + 24).ok_or("zip central directory truncated")?;
        let nlen = usize::from(le16(&cd, at + 28).ok_or("zip central directory truncated")?);
        let xlen = usize::from(le16(&cd, at + 30).ok_or("zip central directory truncated")?);
        let clen = usize::from(le16(&cd, at + 32).ok_or("zip central directory truncated")?);
        let name_start = at
            .checked_add(CDH_FIXED)
            .ok_or("zip central directory corrupt")?;
        let name_end = name_start
            .checked_add(nlen)
            .ok_or("zip central directory corrupt")?;
        let next = name_end
            .checked_add(xlen)
            .and_then(|v| v.checked_add(clen))
            .ok_or("zip central directory corrupt")?;
        let raw = cd
            .get(name_start..name_end)
            .ok_or("zip central directory truncated")?;
        if next > cd.len() {
            return Err("zip central directory truncated".into());
        }
        let name = String::from_utf8_lossy(raw).into_owned();
        let is_dir = name.ends_with('/');
        listing.entries.push(Entry {
            kind: if is_dir {
                EntryKind::Dir
            } else {
                EntryKind::File
            },
            size: if is_dir {
                None
            } else {
                Some(u64::from(usize_))
            },
            name,
            encrypted: flags & 1 != 0,
        });
        at = next;
    }
    Ok(listing)
}

/// Where one zip entry lives, from its central-directory record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZipEntryLoc {
    pub method: u16,
    pub flags: u16,
    pub compressed: u64,
    pub uncompressed: u64,
    pub local_header_offset: u64,
}

/// Find the entry called `name` in a zip's central directory.
pub fn zip_find_entry<R: Read + Seek>(
    r: &mut R,
    file_len: u64,
    name: &str,
) -> Result<Option<ZipEntryLoc>, String> {
    const TRUNC: &str = "zip central directory truncated";
    let (cd, total_entries) = zip_read_cd(r, file_len)?;
    let mut at = 0usize;
    for _ in 0..total_entries {
        if le32(&cd, at) != Some(CDH_SIG) {
            return Err("zip central directory corrupt".into());
        }
        let method = le16(&cd, at + 10).ok_or(TRUNC)?;
        let flags = le16(&cd, at + 8).ok_or(TRUNC)?;
        let compressed = le32(&cd, at + 20).ok_or(TRUNC)?;
        let uncompressed = le32(&cd, at + 24).ok_or(TRUNC)?;
        let nlen = usize::from(le16(&cd, at + 28).ok_or(TRUNC)?);
        let xlen = usize::from(le16(&cd, at + 30).ok_or(TRUNC)?);
        let clen = usize::from(le16(&cd, at + 32).ok_or(TRUNC)?);
        let off = le32(&cd, at + 42).ok_or(TRUNC)?;
        let name_start = at
            .checked_add(CDH_FIXED)
            .ok_or("zip central directory corrupt")?;
        let name_end = name_start
            .checked_add(nlen)
            .ok_or("zip central directory corrupt")?;
        let next = name_end
            .checked_add(xlen)
            .and_then(|v| v.checked_add(clen))
            .ok_or("zip central directory corrupt")?;
        let raw = cd.get(name_start..name_end).ok_or(TRUNC)?;
        if next > cd.len() {
            return Err(TRUNC.into());
        }
        if raw == name.as_bytes() {
            if compressed == 0xFFFF_FFFF || uncompressed == 0xFFFF_FFFF || off == 0xFFFF_FFFF {
                return Err("zip64 entry: not supported".into());
            }
            return Ok(Some(ZipEntryLoc {
                method,
                flags,
                compressed: u64::from(compressed),
                uncompressed: u64::from(uncompressed),
                local_header_offset: u64::from(off),
            }));
        }
        at = next;
    }
    Ok(None)
}

/// Absolute offset of an entry's data, after validating its local header.
pub fn zip_entry_data_offset<R: Read + Seek>(
    r: &mut R,
    file_len: u64,
    loc: &ZipEntryLoc,
) -> Result<u64, String> {
    let hdr_end = loc
        .local_header_offset
        .checked_add(30)
        .ok_or("zip local header out of range")?;
    if hdr_end > file_len {
        return Err("zip local header out of range".into());
    }
    r.seek(SeekFrom::Start(loc.local_header_offset))
        .map_err(|e| format!("zip seek failed: {e}"))?;
    let mut h = [0u8; 30];
    r.read_exact(&mut h)
        .map_err(|e| format!("zip read failed: {e}"))?;
    if le32(&h, 0) != Some(0x0403_4b50) {
        return Err("zip local header corrupt".into());
    }
    let nlen = u64::from(le16(&h, 26).ok_or("zip local header truncated")?);
    let xlen = u64::from(le16(&h, 28).ok_or("zip local header truncated")?);
    let data = hdr_end
        .checked_add(nlen)
        .and_then(|v| v.checked_add(xlen))
        .ok_or("zip local header out of range")?;
    let data_end = data
        .checked_add(loc.compressed)
        .ok_or("zip entry out of range")?;
    if data_end > file_len {
        return Err("zip entry out of range".into());
    }
    Ok(data)
}

fn tar_str(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

fn tar_size(field: &[u8]) -> Option<u64> {
    if field.first().is_some_and(|b| b & 0x80 != 0) {
        // GNU base-256: big-endian, high bit of first byte is the marker.
        let mut v: u64 = u64::from(field[0] & 0x7f);
        for &b in &field[1..] {
            v = v.checked_mul(256)?.checked_add(u64::from(b))?;
        }
        return Some(v);
    }
    let mut v: u64 = 0;
    let mut seen = false;
    for &b in field {
        match b {
            b'0'..=b'7' => {
                v = v.checked_mul(8)?.checked_add(u64::from(b - b'0'))?;
                seen = true;
            }
            b' ' | 0 if seen || b == 0 => break,
            b' ' => {}
            _ => return None,
        }
    }
    Some(v)
}

fn tar_checksum_ok(block: &[u8; 512]) -> bool {
    let stored = block[148..156]
        .iter()
        .take_while(|b| **b != 0 && **b != b' ')
        .try_fold(0u64, |acc, b| match b {
            b'0'..=b'7' => Some(acc * 8 + u64::from(b - b'0')),
            _ => None,
        });
    let Some(stored) = stored else { return false };
    let sum: u64 = block
        .iter()
        .enumerate()
        .map(|(i, b)| {
            if (148..156).contains(&i) {
                32
            } else {
                u64::from(*b)
            }
        })
        .sum();
    sum == stored
}

/// Read up to `n` bytes into a sink; returns bytes actually skipped.
fn skip_bytes<R: Read>(r: &mut R, n: u64, cancel: &AtomicBool) -> std::io::Result<Option<u64>> {
    let mut left = n;
    let mut buf = [0u8; 8192];
    while left > 0 {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let want = usize::try_from(left.min(buf.len() as u64)).unwrap_or(buf.len());
        let got = r.read(&mut buf[..want])?;
        if got == 0 {
            return Ok(Some(n - left));
        }
        left -= got as u64;
    }
    Ok(Some(n))
}

fn pad512(n: u64) -> u64 {
    n.div_ceil(512).saturating_mul(512)
}

/// List a (decompressed) tar stream.
pub fn tar_listing<R: Read>(
    r: &mut R,
    max_entries: usize,
    max_scan_bytes: u64,
    cancel: &AtomicBool,
) -> Result<Listing, String> {
    let mut listing = Listing::default();
    let mut scanned: u64 = 0;
    let mut long_name: Option<String> = None;
    let mut zero_blocks = 0u8;
    let mut first = true;
    loop {
        if cancel.load(Ordering::Relaxed) {
            listing.truncated = Some(Truncation::Cancelled);
            return Ok(listing);
        }
        if scanned >= max_scan_bytes {
            listing.truncated = Some(Truncation::ScanLimit);
            return Ok(listing);
        }
        if listing.entries.len() >= max_entries {
            listing.truncated = Some(Truncation::MaxEntries);
            return Ok(listing);
        }
        let mut block = [0u8; 512];
        match read_full(r, &mut block) {
            Ok(512) => {}
            Ok(0) if !first => return Ok(listing),
            Ok(_) => return Err("tar header truncated".into()),
            Err(e) => return Err(format!("tar read failed: {e}")),
        }
        scanned = scanned.saturating_add(512);
        if block.iter().all(|b| *b == 0) {
            zero_blocks += 1;
            if zero_blocks >= 2 {
                return Ok(listing);
            }
            first = false;
            continue;
        }
        zero_blocks = 0;
        first = false;
        if !tar_checksum_ok(&block) {
            return Err("tar header checksum mismatch".into());
        }
        let size = tar_size(&block[124..136]).ok_or("tar size field invalid")?;
        let flag = block[156];
        match flag {
            b'L' => {
                if size > TAR_LONGNAME_MAX {
                    return Err("tar long name too large".into());
                }
                let padded = pad512(size);
                let mut buf = vec![0u8; usize::try_from(padded).map_err(|_| "tar long name")?];
                let got = read_full(r, &mut buf).map_err(|e| format!("tar read failed: {e}"))?;
                if got != buf.len() {
                    return Err("tar long name truncated".into());
                }
                scanned = scanned.saturating_add(padded);
                let n = usize::try_from(size).map_err(|_| "tar long name")?;
                long_name = Some(tar_str(&buf[..n]));
                continue;
            }
            b'x' | b'g' | b'K' => {
                match skip_bytes(r, pad512(size), cancel)
                    .map_err(|e| format!("tar read failed: {e}"))?
                {
                    None => {
                        listing.truncated = Some(Truncation::Cancelled);
                        return Ok(listing);
                    }
                    Some(got) if got != pad512(size) => {
                        return Err("tar data truncated".into());
                    }
                    Some(got) => scanned = scanned.saturating_add(got),
                }
                continue;
            }
            _ => {}
        }
        let name = match long_name.take() {
            Some(n) => n,
            None => {
                let base = tar_str(&block[0..100]);
                let prefix = if &block[257..262] == b"ustar" {
                    tar_str(&block[345..500])
                } else {
                    String::new()
                };
                if prefix.is_empty() {
                    base
                } else {
                    format!("{prefix}/{base}")
                }
            }
        };
        let (kind, size_shown) = match flag {
            0 | b'0' => (EntryKind::File, Some(size)),
            b'5' => (EntryKind::Dir, None),
            b'2' => (EntryKind::Symlink(tar_str(&block[157..257])), None),
            _ => (EntryKind::Other, None),
        };
        // Data follows only for regular-file-like entries.
        let data = if matches!(flag, 0 | b'0' | b'7')
            || !matches!(flag, b'1' | b'2' | b'3' | b'4' | b'5' | b'6')
        {
            pad512(size)
        } else {
            0
        };
        listing.entries.push(Entry {
            name,
            size: size_shown,
            kind,
            encrypted: false,
        });
        let room = max_scan_bytes.saturating_sub(scanned);
        let to_skip = data.min(room);
        match skip_bytes(r, to_skip, cancel).map_err(|e| format!("tar read failed: {e}"))? {
            None => {
                listing.truncated = Some(Truncation::Cancelled);
                return Ok(listing);
            }
            Some(got) => {
                scanned = scanned.saturating_add(got);
                if got < to_skip {
                    // Stream ended mid-data: a clean EOF after the last entry.
                    return Ok(listing);
                }
            }
        }
        if data > to_skip {
            listing.truncated = Some(Truncation::ScanLimit);
            return Ok(listing);
        }
    }
}

fn read_full<R: Read>(r: &mut R, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

#[cfg(test)]
pub(crate) mod testutil {
    pub fn zip_bytes(entries: &[(&str, u32)], comment: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut cd = Vec::new();
        for (name, size) in entries {
            let off = out.len() as u32;
            out.extend_from_slice(b"PK\x03\x04");
            out.extend_from_slice(&[0u8; 26]);
            out.extend_from_slice(name.as_bytes());
            cd.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            cd.extend_from_slice(&[0u8; 4]); // versions
            cd.extend_from_slice(&0u16.to_le_bytes()); // flags
            cd.extend_from_slice(&[0u8; 10]); // method..crc
            cd.extend_from_slice(&size.to_le_bytes()); // csize
            cd.extend_from_slice(&size.to_le_bytes()); // usize
            cd.extend_from_slice(&(name.len() as u16).to_le_bytes());
            cd.extend_from_slice(&[0u8; 8]); // xlen clen disk iattr
            cd.extend_from_slice(&[0u8; 4]); // eattr
            cd.extend_from_slice(&off.to_le_bytes());
            cd.extend_from_slice(name.as_bytes());
        }
        let cd_off = out.len() as u32;
        out.extend_from_slice(&cd);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&[0u8; 4]);
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(cd.len() as u32).to_le_bytes());
        out.extend_from_slice(&cd_off.to_le_bytes());
        out.extend_from_slice(&(comment.len() as u16).to_le_bytes());
        out.extend_from_slice(comment);
        out
    }

    pub fn tar_header(
        name: &str,
        size_field: &[u8],
        flag: u8,
        link: &str,
        prefix: &str,
    ) -> [u8; 512] {
        let mut b = [0u8; 512];
        b[..name.len()].copy_from_slice(name.as_bytes());
        b[124..124 + size_field.len()].copy_from_slice(size_field);
        b[156] = flag;
        b[157..157 + link.len()].copy_from_slice(link.as_bytes());
        b[257..262].copy_from_slice(b"ustar");
        b[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
        b[148..156].fill(b' ');
        let sum: u64 = b.iter().map(|x| u64::from(*x)).sum();
        let s = format!("{sum:06o}\0 ");
        b[148..156].copy_from_slice(s.as_bytes());
        b
    }

    pub fn tar_file(name: &str, content: &[u8]) -> Vec<u8> {
        let mut v = tar_header(
            name,
            format!("{:011o}\0", content.len()).as_bytes(),
            b'0',
            "",
            "",
        )
        .to_vec();
        v.extend_from_slice(content);
        v.resize(v.len().div_ceil(512) * 512, 0);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;
    use std::io::Cursor;

    fn zl(b: &[u8], max: usize) -> Result<Listing, String> {
        zip_listing(&mut Cursor::new(b), b.len() as u64, max)
    }
    fn tl(b: &[u8], max: usize, scan: u64) -> Result<Listing, String> {
        tar_listing(&mut Cursor::new(b), max, scan, &AtomicBool::new(false))
    }

    #[test]
    fn zip_lists_files_and_dirs() {
        let z = zip_bytes(&[("a.txt", 5), ("d/", 0), ("d/b.bin", 99)], b"");
        let l = zl(&z, 100).unwrap();
        assert_eq!(l.total_hint, Some(3));
        assert_eq!(l.entries.len(), 3);
        assert_eq!(l.entries[0].name, "a.txt");
        assert_eq!(l.entries[0].size, Some(5));
        assert_eq!(l.entries[1].kind, EntryKind::Dir);
        assert_eq!(l.entries[2].size, Some(99));
        assert!(l.truncated.is_none());
    }

    #[test]
    fn zip_comment_shifts_eocd_search_and_max_entries_truncates() {
        let z = zip_bytes(&[("a", 1), ("b", 2)], b"hello comment");
        assert_eq!(zl(&z, 10).unwrap().entries.len(), 2);
        let l = zl(&z, 1).unwrap();
        assert_eq!(l.entries.len(), 1);
        assert_eq!(l.truncated, Some(Truncation::MaxEntries));
    }

    #[test]
    fn zip_errors_never_panic_on_any_prefix() {
        let z = zip_bytes(&[("a.txt", 5), ("d/", 0)], b"");
        for n in 0..z.len() {
            assert!(zl(&z[..n], 10).is_err(), "prefix {n}");
        }
        // Corrupt every byte in turn: must not panic.
        for i in 0..z.len() {
            let mut c = z.clone();
            c[i] ^= 0xff;
            drop(zl(&c, 10));
        }
    }

    #[test]
    fn zip_out_of_range_and_zip64_rejected() {
        let mut z = zip_bytes(&[("a", 1)], b"");
        let eocd = z.len() - 22;
        z[eocd + 16..eocd + 20].copy_from_slice(&0x7fff_0000u32.to_le_bytes());
        assert!(zl(&z, 10).unwrap_err().contains("out of range"));
        let mut z64 = zip_bytes(&[("a", 1)], b"");
        let eocd = z64.len() - 22;
        z64[eocd + 16..eocd + 20].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(zl(&z64, 10).unwrap_err().contains("zip64"));
        let mut e = zip_bytes(&[("a", 1)], b"");
        let eocd = e.len() - 22;
        e[eocd + 10..eocd + 12].copy_from_slice(&0xFFFFu16.to_le_bytes());
        assert!(zl(&e, 10).unwrap_err().contains("zip64"));
    }

    #[test]
    fn zip_flags_encrypted_and_lossy_names() {
        let mut z = zip_bytes(&[("x\u{1b}[31m", 1)], b"");
        let cd = z.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
        z[cd + 8] = 1;
        let l = zl(&z, 10).unwrap();
        assert!(l.entries[0].encrypted);
        assert!(l.entries[0].name.contains('\u{1b}'));
    }

    #[test]
    fn tar_file_dir_symlink_prefix_and_base256() {
        let mut t = Vec::new();
        t.extend(tar_file("hello.txt", b"hi"));
        t.extend_from_slice(&tar_header("sub", b"00000000000\0", b'5', "", ""));
        t.extend_from_slice(&tar_header("ln", b"00000000000\0", b'2', "target", ""));
        let mut h = tar_header("deep.txt", b"00000000000\0", b'0', "", "some/prefix");
        h[124..136].fill(0);
        h[124] = 0x80;
        h[135] = 3; // base-256 size 3
        h[148..156].fill(b' ');
        let sum: u64 = h.iter().map(|x| u64::from(*x)).sum();
        h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        t.extend_from_slice(&h);
        t.extend_from_slice(&[b'x'; 3]);
        t.resize(t.len().div_ceil(512) * 512, 0);
        t.extend_from_slice(&[0u8; 1024]);
        let l = tl(&t, 100, 1 << 20).unwrap();
        let names: Vec<_> = l.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["hello.txt", "sub", "ln", "some/prefix/deep.txt"]);
        assert_eq!(l.entries[0].size, Some(2));
        assert_eq!(l.entries[1].kind, EntryKind::Dir);
        assert_eq!(l.entries[2].kind, EntryKind::Symlink("target".into()));
        assert_eq!(l.entries[3].size, Some(3));
        assert!(l.truncated.is_none());
    }

    #[test]
    fn tar_gnu_long_name_and_pax_skipped() {
        let long = "n".repeat(150);
        let mut t = Vec::new();
        t.extend_from_slice(&tar_header("pax", b"00000000004\0", b'x', "", ""));
        t.extend_from_slice(&[b'p'; 4]);
        t.resize(t.len().div_ceil(512) * 512, 0);
        let mut body = long.clone().into_bytes();
        body.push(0);
        t.extend_from_slice(&tar_header(
            "././@LongLink",
            format!("{:011o}\0", body.len()).as_bytes(),
            b'L',
            "",
            "",
        ));
        t.extend_from_slice(&body);
        t.resize(t.len().div_ceil(512) * 512, 0);
        t.extend(tar_file("short", b""));
        let l = tl(&t, 10, 1 << 20).unwrap();
        assert_eq!(l.entries.len(), 1);
        assert_eq!(l.entries[0].name, long);
    }

    #[test]
    fn tar_bad_checksum_and_garbage_are_errors() {
        let mut t = tar_file("a", b"x");
        t[0] = b'b';
        assert!(tl(&t, 10, 1 << 20).unwrap_err().contains("checksum"));
        assert!(tl(&[1u8; 100], 10, 1 << 20).is_err());
        let ok = tar_file("a", b"xyz");
        for n in 1..ok.len() {
            drop(tl(&ok[..n], 10, 1 << 20)); // never panics
        }
        let mut bad_size = tar_header("a", b"99999999999\0", b'0', "", "").to_vec();
        bad_size[124] = b'z';
        assert!(tl(&bad_size, 10, 1 << 20).is_err());
    }

    #[test]
    fn tar_truncation_and_cancel() {
        let mut t = Vec::new();
        for i in 0..5 {
            t.extend(tar_file(&format!("f{i}"), &[b'a'; 600]));
        }
        t.extend_from_slice(&[0u8; 1024]);
        let l = tl(&t, 2, 1 << 20).unwrap();
        assert_eq!(l.entries.len(), 2);
        assert_eq!(l.truncated, Some(Truncation::MaxEntries));
        let l = tl(&t, 100, 2000).unwrap();
        assert_eq!(l.truncated, Some(Truncation::ScanLimit));
        assert!(l.entries.len() < 5);
        let cancel = AtomicBool::new(true);
        let l = tar_listing(&mut Cursor::new(&t), 100, 1 << 20, &cancel).unwrap();
        assert_eq!(l.truncated, Some(Truncation::Cancelled));
        assert!(l.entries.is_empty());
    }

    #[test]
    fn tar_size_parsing() {
        assert_eq!(tar_size(b"00000000017\0"), Some(15));
        assert_eq!(tar_size(b"  17 \0"), Some(15));
        assert_eq!(tar_size(b"\0"), Some(0));
        assert_eq!(tar_size(b"zz"), None);
        assert_eq!(tar_size(&[0xff; 12]), None);
    }
    /// One-entry zip with explicit method, flags and sizes.
    fn zip_one(name: &str, method: u16, flags: u16, data: &[u8], local_off: u32) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"PK\x03\x04");
        out.extend_from_slice(&[0u8; 22]);
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
        let mut cd = Vec::new();
        cd.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        cd.extend_from_slice(&[0u8; 4]);
        cd.extend_from_slice(&flags.to_le_bytes());
        cd.extend_from_slice(&method.to_le_bytes());
        cd.extend_from_slice(&[0u8; 8]);
        cd.extend_from_slice(&(data.len() as u32).to_le_bytes());
        cd.extend_from_slice(&(data.len() as u32).to_le_bytes());
        cd.extend_from_slice(&(name.len() as u16).to_le_bytes());
        cd.extend_from_slice(&[0u8; 12]);
        cd.extend_from_slice(&local_off.to_le_bytes());
        cd.extend_from_slice(name.as_bytes());
        let cd_off = out.len() as u32;
        out.extend_from_slice(&cd);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&[0u8; 4]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&(cd.len() as u32).to_le_bytes());
        out.extend_from_slice(&cd_off.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    #[test]
    fn zip_find_entry_finds_and_locates_data() {
        let z = zip_one("word/document.xml", 8, 1, b"abcde", 0);
        let mut c = Cursor::new(&z[..]);
        let loc = zip_find_entry(&mut c, z.len() as u64, "word/document.xml")
            .unwrap()
            .unwrap();
        assert_eq!((loc.method, loc.flags, loc.compressed), (8, 1, 5));
        assert_eq!(loc.uncompressed, 5);
        let off = zip_entry_data_offset(&mut c, z.len() as u64, &loc).unwrap();
        assert_eq!(&z[off as usize..off as usize + 5], b"abcde");
        assert_eq!(
            zip_find_entry(&mut c, z.len() as u64, "nope").unwrap(),
            None
        );
    }

    #[test]
    fn zip_entry_offsets_out_of_range_are_errors() {
        let z = zip_one("a", 0, 0, b"abc", 0xFFFF_FF00);
        let mut c = Cursor::new(&z[..]);
        let loc = zip_find_entry(&mut c, z.len() as u64, "a")
            .unwrap()
            .unwrap();
        assert!(zip_entry_data_offset(&mut c, z.len() as u64, &loc).is_err());
        let z = zip_one("a", 0, 0, b"abc", 3); // not a local header
        let mut c = Cursor::new(&z[..]);
        let loc = zip_find_entry(&mut c, z.len() as u64, "a")
            .unwrap()
            .unwrap();
        assert!(zip_entry_data_offset(&mut c, z.len() as u64, &loc).is_err());
        let mut loc = loc;
        loc.local_header_offset = u64::MAX - 5;
        assert!(zip_entry_data_offset(&mut c, z.len() as u64, &loc).is_err());
        loc.local_header_offset = 0;
        loc.compressed = u64::MAX;
        assert!(zip_entry_data_offset(&mut c, z.len() as u64, &loc).is_err());
    }

    #[test]
    fn zip_find_entry_corrupt_inputs_never_panic() {
        let z = zip_one("a", 0, 0, b"abc", 0);
        for n in 0..z.len() {
            let mut c = Cursor::new(&z[..n]);
            assert!(zip_find_entry(&mut c, n as u64, "a").is_err(), "prefix {n}");
        }
        assert!(zip_find_entry(&mut Cursor::new(&b"x"[..]), 1, "a").is_err());
    }
}
