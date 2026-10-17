//! Held-fd Linux pseudo-file reader with offset-zero pread and a permanent
//! seek/read fallback for sources that do not support positioned reads.
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::FileExt;
use std::path::Path;

pub trait Source {
    fn read_at(&self, buffer: &mut [u8], offset: u64) -> io::Result<usize>;
    fn rewind_read(&mut self, buffer: &mut [u8]) -> io::Result<usize>;
}
impl Source for File {
    fn read_at(&self, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
        FileExt::read_at(self, buffer, offset)
    }
    fn rewind_read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.seek(SeekFrom::Start(0))?;
        Read::read(self, buffer)
    }
}

#[derive(Debug)]
pub struct ProcFile {
    file: File,
    buffer: Vec<u8>,
    fallback: bool,
    needs_reopen: bool,
}
impl ProcFile {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        Ok(Self {
            file: File::open(path)?,
            buffer: vec![0; 4096],
            fallback: false,
            needs_reopen: false,
        })
    }
    pub fn read_all(&mut self) -> io::Result<&[u8]> {
        let result = fill(&mut self.file, &mut self.buffer, &mut self.fallback);
        if let Err(error) = &result
            && should_reopen(error)
        {
            self.needs_reopen = true;
        }
        result.map(|length| &self.buffer[..length])
    }
    #[cfg(test)]
    fn read_from<S: Source>(&mut self, source: &mut S) -> io::Result<&[u8]> {
        let result = fill(source, &mut self.buffer, &mut self.fallback);
        if let Err(error) = &result
            && should_reopen(error)
        {
            self.needs_reopen = true;
        }
        result.map(|length| &self.buffer[..length])
    }
    pub const fn needs_reopen(&self) -> bool {
        self.needs_reopen
    }
}

pub fn fill<S: Source>(
    source: &mut S,
    buffer: &mut Vec<u8>,
    fallback: &mut bool,
) -> io::Result<usize> {
    if *fallback {
        return read_fallback(source, buffer);
    }
    let mut length = 0;
    loop {
        match source.read_at(&mut buffer[length..], length as u64) {
            Ok(0) => return Ok(length),
            Ok(read) => {
                length += read;
                if length < buffer.len() {
                    return Ok(length);
                }
                let capacity = buffer.len();
                let grown = capacity
                    .saturating_mul(2)
                    .saturating_add(capacity / 4)
                    .max(capacity.saturating_add(1));
                buffer.resize(grown, 0);
            }
            Err(error) if error.kind() == io::ErrorKind::NotSeekable => {
                *fallback = true;
                return read_fallback(source, buffer);
            }
            Err(error) => return Err(error),
        }
    }
}

fn read_fallback<S: Source>(source: &mut S, buffer: &mut Vec<u8>) -> io::Result<usize> {
    loop {
        let length = source.rewind_read(buffer)?;
        if length < buffer.len() {
            return Ok(length);
        }
        let capacity = buffer.len();
        let grown = capacity
            .saturating_mul(2)
            .saturating_add(capacity / 4)
            .max(capacity.saturating_add(1));
        buffer.resize(grown, 0);
    }
}

fn should_reopen(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::NotFound)
        || matches!(error.raw_os_error(), Some(code) if code == libc::ENODEV || code == libc::ESTALE)
}

#[cfg(test)]
mod tests {
    use super::{ProcFile, Source, fill};
    use std::fs::{self, File};
    use std::io;
    use std::os::unix::fs::FileExt;

    #[test]
    fn held_file_rereads_from_zero_and_grows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proc-like");
        fs::write(&path, b"first").unwrap();
        let mut reader = ProcFile::open(&path).unwrap();
        assert_eq!(reader.read_all().unwrap(), b"first");
        let writer = File::options().write(true).open(&path).unwrap();
        writer.set_len(0).unwrap();
        writer.write_at(b"second", 0).unwrap();
        assert_eq!(reader.read_all().unwrap(), b"second");
        writer.set_len(0).unwrap();
        let large = vec![b'x'; 4096 * 3];
        writer.write_at(&large, 0).unwrap();
        assert_eq!(reader.read_all().unwrap(), large);
    }

    #[test]
    fn missing_open_errors_and_device_error_marks_reopen() {
        let dir = tempfile::tempdir().unwrap();
        assert!(ProcFile::open(dir.path().join("missing")).is_err());
        let path = dir.path().join("device");
        fs::write(&path, b"x").unwrap();
        let mut reader = ProcFile::open(path).unwrap();
        let mut failing = ErrorSource { code: libc::ENODEV };
        let result = reader.read_from(&mut failing);
        assert!(matches!(result, Err(error) if error.raw_os_error() == Some(libc::ENODEV)));
        assert!(reader.needs_reopen());
    }

    #[derive(Debug)]
    struct ErrorSource {
        code: i32,
    }
    impl Source for ErrorSource {
        fn read_at(&self, _buffer: &mut [u8], _offset: u64) -> io::Result<usize> {
            Err(io::Error::from_raw_os_error(self.code))
        }
        fn rewind_read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Ok(0)
        }
    }

    #[derive(Debug)]
    struct FallbackSource {
        called: bool,
    }
    impl Source for FallbackSource {
        fn read_at(&self, _buffer: &mut [u8], _offset: u64) -> io::Result<usize> {
            Err(io::ErrorKind::NotSeekable.into())
        }
        fn rewind_read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.called = true;
            buffer[..4].copy_from_slice(b"fake");
            Ok(4)
        }
    }

    #[test]
    fn nonseekable_source_switches_permanently_to_rewind_read() {
        let mut source = FallbackSource { called: false };
        let mut buffer = vec![0; 16];
        let mut fallback = false;
        let count = fill(&mut source, &mut buffer, &mut fallback).unwrap();
        assert_eq!(&buffer[..count], b"fake");
        assert!(source.called);
        assert!(fallback);
    }
}
