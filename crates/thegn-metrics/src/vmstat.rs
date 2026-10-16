//! Small, allocation-free reads from Linux `/proc/vmstat`.

#[cfg(target_os = "linux")]
use std::fs::File;

/// Parse one whitespace-separated vmstat field by exact key.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_field(buf: &[u8], key: &[u8]) -> Option<u64> {
    buf.split(|byte| *byte == b'\n').find_map(|line| {
        let separator = line.iter().position(|byte| *byte == b' ')?;
        let (field, value) = line.split_at(separator);
        if field != key {
            return None;
        }
        std::str::from_utf8(&value[1..]).ok()?.trim().parse().ok()
    })
}

/// Reusable reader for the direct-reclaim counter.
pub(crate) struct VmstatReader {
    #[cfg(target_os = "linux")]
    path: std::path::PathBuf,
    #[cfg(target_os = "linux")]
    file: Option<File>,
    #[cfg(target_os = "linux")]
    buf: Vec<u8>,
}

impl VmstatReader {
    /// Construct without opening `/proc/vmstat`; sampling owns the I/O.
    pub(crate) fn new() -> Self {
        Self {
            #[cfg(target_os = "linux")]
            path: "/proc/vmstat".into(),
            #[cfg(target_os = "linux")]
            file: None,
            #[cfg(target_os = "linux")]
            buf: vec![0; 8 * 1024],
        }
    }

    #[cfg(all(test, target_os = "linux"))]
    fn at(path: std::path::PathBuf) -> Self {
        Self {
            path,
            file: None,
            buf: vec![0; 8 * 1024],
        }
    }

    /// Read the current direct-reclaim counter. Missing fields stay absent.
    #[cfg(target_os = "linux")]
    pub(crate) fn pgsteal_direct(&mut self) -> Option<u64> {
        use std::os::unix::fs::FileExt;

        if self.file.is_none() {
            self.file = Some(File::open(&self.path).ok()?);
        }
        let file = self.file.as_ref()?;
        let mut filled = 0;
        let mut failed = false;
        loop {
            match file.read_at(&mut self.buf[filled..], filled as u64) {
                Ok(0) => break,
                Ok(read) => filled += read,
                Err(_) => {
                    failed = true;
                    break;
                }
            }
            if filled == self.buf.len() {
                self.buf.resize(self.buf.len() * 2, 0);
                filled = 0;
                loop {
                    match file.read_at(&mut self.buf[filled..], filled as u64) {
                        Ok(0) => break,
                        Ok(read) => filled += read,
                        Err(_) => {
                            failed = true;
                            break;
                        }
                    }
                    if filled == self.buf.len() {
                        break;
                    }
                }
                break;
            }
        }
        if failed || filled == 0 {
            self.file = None;
            return None;
        }
        parse_field(&self.buf[..filled], b"pgsteal_direct")
    }

    /// This kernel interface is unavailable off Linux.
    #[cfg(not(target_os = "linux"))]
    pub(crate) fn pgsteal_direct(&mut self) -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::parse_field;

    #[test]
    fn parser_matches_exact_key_and_missing_fields_stay_absent() {
        assert_eq!(
            parse_field(
                b"pgsteal_direct_dma 7\npgsteal_direct 42\n",
                b"pgsteal_direct"
            ),
            Some(42)
        );
        assert_eq!(parse_field(b"pgsteal_kswapd 1\n", b"pgsteal_direct"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reader_reuses_buffer_tracks_rewrites_grows_once_and_reopens() {
        use super::VmstatReader;
        use std::fs;

        let dir = tempfile::tempdir().expect("private fixture directory");
        let path = dir.path().join("vmstat");
        fs::create_dir(&path).unwrap();
        let mut reader = VmstatReader::at(path.clone());
        assert_eq!(reader.pgsteal_direct(), None);
        assert!(reader.file.is_none(), "read errors drop the held fd");

        fs::remove_dir(&path).unwrap();
        fs::write(&path, b"pgsteal_direct_dma 7\npgsteal_direct 42\n").unwrap();
        assert_eq!(reader.pgsteal_direct(), Some(42));
        let ptr = reader.buf.as_ptr();
        let capacity = reader.buf.capacity();

        for (contents, expected) in [
            (&b"pgsteal_direct 8\n"[..], 8),
            (&b"pgsteal_direct 9\n"[..], 9),
            (&b"pgsteal_direct 10\n"[..], 10),
        ] {
            fs::write(&path, contents).unwrap();
            assert_eq!(reader.pgsteal_direct(), Some(expected));
            assert_eq!(reader.buf.as_ptr(), ptr);
            assert_eq!(reader.buf.capacity(), capacity);
        }

        fs::write(
            &path,
            [vec![b'x'; 9 * 1024], b"\npgsteal_direct 11\n".to_vec()].concat(),
        )
        .unwrap();
        assert_eq!(reader.pgsteal_direct(), Some(11));
        let grown_ptr = reader.buf.as_ptr();
        let grown_capacity = reader.buf.capacity();
        assert!(
            grown_capacity > capacity,
            "large input grows the read buffer"
        );
        assert_eq!(reader.pgsteal_direct(), Some(11));
        assert_eq!(reader.buf.as_ptr(), grown_ptr);
        assert_eq!(reader.buf.capacity(), grown_capacity);
    }
}
