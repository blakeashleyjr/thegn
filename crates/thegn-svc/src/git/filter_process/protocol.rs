//! Client side of Git's long-running filter protocol (version 2).
//!
//! This is the transport/protocol half of THE-700's owned filter process. It is
//! intentionally not wired into gix yet; the next slice can own and reuse one
//! client per worktree and driver.

use std::fmt;
use std::io::{self, Read, Write};

pub const MAX_PKT_DATA: usize = 65_516;
const MAX_PKT_LENGTH: usize = MAX_PKT_DATA + 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pkt {
    Data(Vec<u8>),
    Flush,
}

pub fn write_pkt<W: Write>(writer: &mut W, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_PKT_DATA {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "pkt-line payload too large",
        ));
    }
    if payload.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty pkt-line data is forbidden",
        ));
    }
    write_header(writer, payload.len() + 4)?;
    writer.write_all(payload)
}

pub fn write_flush<W: Write>(writer: &mut W) -> io::Result<()> {
    writer.write_all(b"0000")?;
    writer.flush()
}

fn write_header<W: Write>(writer: &mut W, length: usize) -> io::Result<()> {
    let header = format!("{length:04x}");
    writer.write_all(header.as_bytes())
}

pub fn read_pkt<R: Read>(reader: &mut R) -> io::Result<Pkt> {
    let mut header = [0_u8; 4];
    read_exact_packet(reader, &mut header)?;
    let length = header
        .iter()
        .try_fold(0_usize, |value, byte| {
            byte.to_digit_hex().map(|digit| value * 16 + digit)
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "non-hex pkt-line header"))?;
    match length {
        0 => Ok(Pkt::Flush),
        1..=3 => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "reserved pkt-line control packet",
        )),
        4..=MAX_PKT_LENGTH => {
            let mut data = vec![0; length - 4];
            read_exact_packet(reader, &mut data)?;
            Ok(Pkt::Data(data))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "pkt-line length too large",
        )),
    }
}

trait HexDigit {
    fn to_digit_hex(self) -> Option<usize>;
}

impl HexDigit for u8 {
    fn to_digit_hex(self) -> Option<usize> {
        match self {
            b'0'..=b'9' => Some((self - b'0') as usize),
            b'a'..=b'f' => Some((self - b'a' + 10) as usize),
            b'A'..=b'F' => Some((self - b'A' + 10) as usize),
            _ => None,
        }
    }
}

fn read_exact_packet<R: Read>(reader: &mut R, bytes: &mut [u8]) -> io::Result<()> {
    let mut filled = 0;
    while filled < bytes.len() {
        match reader.read(&mut bytes[filled..])? {
            0 => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated pkt-line",
                ));
            }
            count => filled += count,
        }
    }
    Ok(())
}

fn write_line<W: Write>(writer: &mut W, line: &[u8]) -> io::Result<()> {
    let mut payload = Vec::with_capacity(line.len() + 1);
    payload.extend_from_slice(line);
    payload.push(b'\n');
    write_pkt(writer, &payload)
}

fn read_line<R: Read>(reader: &mut R) -> Result<Option<Vec<u8>>, ProtocolError> {
    match read_pkt(reader)? {
        Pkt::Flush => Ok(None),
        Pkt::Data(mut data) => {
            if data.last() == Some(&b'\n') {
                data.pop();
            }
            Ok(Some(data))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub clean: bool,
    pub smudge: bool,
}

impl Capabilities {
    pub const fn new(clean: bool, smudge: bool) -> Self {
        Self { clean, smudge }
    }

    fn has(self, command: Command) -> bool {
        match command {
            Command::Clean => self.clean,
            Command::Smudge => self.smudge,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Clean,
    Smudge,
}

impl Command {
    fn as_bytes(self) -> &'static [u8] {
        match self {
            Self::Clean => b"clean",
            Self::Smudge => b"smudge",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Success(Vec<u8>),
    Error,
    Abort,
}

#[derive(Debug)]
pub enum ProtocolError {
    Io(io::Error),
    Protocol(String),
    Unsupported(Command),
    OutputLimit,
    InvalidPath,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "filter protocol I/O: {error}"),
            Self::Protocol(message) => write!(formatter, "filter protocol error: {message}"),
            Self::Unsupported(command) => {
                write!(formatter, "filter command {command:?} was not negotiated")
            }
            Self::OutputLimit => formatter.write_str("filter output exceeded its limit"),
            Self::InvalidPath => formatter.write_str("invalid filter pathname"),
        }
    }
}

impl std::error::Error for ProtocolError {}

impl From<io::Error> for ProtocolError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub struct FilterClient {
    negotiated: Capabilities,
    aborted_clean: bool,
    aborted_smudge: bool,
}

impl FilterClient {
    pub fn handshake<R: Read, W: Write>(
        reader: &mut R,
        writer: &mut W,
        want: Capabilities,
    ) -> Result<Self, ProtocolError> {
        write_line(writer, b"git-filter-client")?;
        write_line(writer, b"version=2")?;
        write_flush(writer)?;

        if read_line(reader)?.as_deref() != Some(b"git-filter-server") {
            return Err(ProtocolError::Protocol("missing server welcome".into()));
        }
        let version = read_line(reader)?;
        if version.as_deref() != Some(b"version=2") {
            return Err(ProtocolError::Protocol(
                "server must offer exactly version=2".into(),
            ));
        }
        if read_line(reader)?.is_some() {
            return Err(ProtocolError::Protocol("unexpected handshake field".into()));
        }

        if want.clean {
            write_line(writer, b"capability=clean")?;
        }
        if want.smudge {
            write_line(writer, b"capability=smudge")?;
        }
        write_flush(writer)?;

        let mut negotiated = Capabilities::new(false, false);
        while let Some(line) = read_line(reader)? {
            let offered = match line.as_slice() {
                b"capability=clean" if want.clean => Command::Clean,
                b"capability=smudge" if want.smudge => Command::Smudge,
                b"capability=clean" | b"capability=smudge" | b"capability=delay" => {
                    return Err(ProtocolError::Protocol(
                        "server offered an unrequested capability".into(),
                    ));
                }
                _ => return Err(ProtocolError::Protocol("unknown server capability".into())),
            };
            match offered {
                Command::Clean => negotiated.clean = true,
                Command::Smudge => negotiated.smudge = true,
            }
        }
        Ok(Self {
            negotiated,
            aborted_clean: false,
            aborted_smudge: false,
        })
    }

    pub fn request<R: Read, W: Write>(
        &mut self,
        reader: &mut R,
        writer: &mut W,
        command: Command,
        pathname: &str,
        content: &[u8],
        max_output: usize,
    ) -> Result<Outcome, ProtocolError> {
        if !self.negotiated.has(command) {
            return Err(ProtocolError::Unsupported(command));
        }
        if pathname.is_empty() || pathname.bytes().any(|byte| byte == b'\n' || byte == 0) {
            return Err(ProtocolError::InvalidPath);
        }
        let was_aborted = match command {
            Command::Clean => self.aborted_clean,
            Command::Smudge => self.aborted_smudge,
        };
        if was_aborted {
            return Ok(Outcome::Abort);
        }

        let mut command_line = b"command=".to_vec();
        command_line.extend_from_slice(command.as_bytes());
        write_line(writer, &command_line)?;
        let mut path_line = b"pathname=".to_vec();
        path_line.extend_from_slice(pathname.as_bytes());
        write_line(writer, &path_line)?;
        write_flush(writer)?;
        for chunk in content.chunks(MAX_PKT_DATA) {
            write_pkt(writer, chunk)?;
        }
        write_flush(writer)?;

        let first_status = read_status_list(reader)?;
        match first_status.as_deref() {
            Some(b"error") => return Ok(Outcome::Error),
            Some(b"abort") => {
                self.mark_aborted(command);
                return Ok(Outcome::Abort);
            }
            Some(b"success") => {}
            Some(b"delayed") => {
                return Err(ProtocolError::Protocol(
                    "delayed status is unsupported".into(),
                ));
            }
            None => return Err(ProtocolError::Protocol("missing status".into())),
            Some(_) => return Err(ProtocolError::Protocol("unknown status".into())),
        }

        let mut output = Vec::new();
        loop {
            match read_pkt(reader)? {
                Pkt::Flush => break,
                Pkt::Data(data) => {
                    if output.len().saturating_add(data.len()) > max_output {
                        return Err(ProtocolError::OutputLimit);
                    }
                    output.extend_from_slice(&data);
                }
            }
        }
        match read_status_list(reader)?.as_deref() {
            None => Ok(Outcome::Success(output)),
            Some(b"error") => Ok(Outcome::Error),
            Some(b"abort") => {
                self.mark_aborted(command);
                Ok(Outcome::Abort)
            }
            Some(b"success") => Ok(Outcome::Success(output)),
            Some(b"delayed") => Err(ProtocolError::Protocol(
                "delayed status is unsupported".into(),
            )),
            Some(_) => Err(ProtocolError::Protocol("unknown status".into())),
        }
    }

    fn mark_aborted(&mut self, command: Command) {
        match command {
            Command::Clean => self.aborted_clean = true,
            Command::Smudge => self.aborted_smudge = true,
        }
    }
}

fn read_status_list<R: Read>(reader: &mut R) -> Result<Option<Vec<u8>>, ProtocolError> {
    let mut status = None;
    while let Some(line) = read_line(reader)? {
        if let Some(value) = line.strip_prefix(b"status=") {
            if status.replace(value.to_vec()).is_some() {
                return Err(ProtocolError::Protocol("duplicate status".into()));
            }
        }
    }
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const SERVER: &[u8] = b"0016git-filter-server\n000eversion=2\n0000";
    const CAPS: &[u8] = b"0015capability=clean\n0016capability=smudge\n0000";

    fn client(server: &[u8], want: Capabilities) -> (Result<FilterClient, ProtocolError>, Vec<u8>) {
        let mut reader = Cursor::new(server.to_vec());
        let mut writer = Vec::new();
        let result = FilterClient::handshake(&mut reader, &mut writer, want);
        (result, writer)
    }

    fn full_server(caps: &[u8], response: &[u8]) -> Vec<u8> {
        [SERVER, caps, response].concat()
    }

    #[test]
    fn handshake_bytes_are_exact_and_never_request_delay() {
        let (result, written) = client(&[SERVER, CAPS].concat(), Capabilities::new(true, true));
        assert!(result.is_ok());
        assert_eq!(written, b"0016git-filter-client\n000eversion=2\n00000015capability=clean\n0016capability=smudge\n0000");
        assert!(
            !written
                .windows(b"capability=delay".len())
                .any(|window| window == b"capability=delay")
        );
    }

    #[test]
    fn handshake_accepts_server_lines_with_or_without_lf() {
        let server = b"0015git-filter-server000dversion=200000015capability=clean\n0000";
        let (result, _) = client(server, Capabilities::new(true, false));
        assert!(result.is_ok());
    }

    #[test]
    fn handshake_rejects_wrong_welcome_version_extra_version_and_missing_flush() {
        for server in [
            b"0010not-a-server\n000eversion=2\n0000".as_slice(),
            b"0016git-filter-server\n000eversion=3\n0000".as_slice(),
            b"0016git-filter-server\n000eversion=2\n000eversion=2\n0000".as_slice(),
            b"0016git-filter-server\n000eversion=2\n".as_slice(),
        ] {
            assert!(client(server, Capabilities::new(true, false)).0.is_err());
        }
    }

    #[test]
    fn handshake_rejects_unasked_and_delay_capabilities() {
        for offered in [
            b"0016capability=smudge\n0000".as_slice(),
            b"0015capability=delay\n0000".as_slice(),
        ] {
            let (result, _) = client(&[SERVER, offered].concat(), Capabilities::new(true, false));
            assert!(matches!(result, Err(ProtocolError::Protocol(_))));
        }
    }

    #[test]
    fn clean_round_trip_writes_exact_request_and_reads_response() {
        let response = b"0013status=success\n0009HELLO00000000";
        let (client, _) = client(
            &full_server(b"0015capability=clean\n0000", response),
            Capabilities::new(true, false),
        );
        let mut client = client.unwrap();
        let mut reader = Cursor::new(response.to_vec());
        let mut writer = Vec::new();
        assert_eq!(
            client
                .request(
                    &mut reader,
                    &mut writer,
                    Command::Clean,
                    "a.bin",
                    b"hello",
                    100
                )
                .unwrap(),
            Outcome::Success(b"HELLO".to_vec())
        );
        assert_eq!(
            writer,
            b"0012command=clean\n0013pathname=a.bin\n00000009hello0000"
        );
    }

    #[test]
    fn empty_content_and_empty_success_output_are_valid() {
        let response = b"0013status=success\n00000000";
        let (client, _) = client(
            &full_server(b"0015capability=clean\n0000", response),
            Capabilities::new(true, false),
        );
        let mut client = client.unwrap();
        let mut reader = Cursor::new(response.to_vec());
        let mut writer = Vec::new();
        assert_eq!(
            client
                .request(&mut reader, &mut writer, Command::Clean, "x", &[], 0)
                .unwrap(),
            Outcome::Success(Vec::new())
        );
        assert_eq!(writer, b"0012command=clean\n000epathname=x\n00000000");
    }

    #[test]
    fn pkt_writer_splits_maximum_payload_and_refuses_oversize() {
        let (client, _) = client(
            &[SERVER, b"0015capability=clean\n0000"].concat(),
            Capabilities::new(true, false),
        );
        let mut client = client.unwrap();
        let content = vec![b'x'; MAX_PKT_DATA + 3];
        let mut reader = Cursor::new(b"0013status=error\n0000".to_vec());
        let mut writer = Vec::new();
        assert_eq!(
            client
                .request(&mut reader, &mut writer, Command::Clean, "x", &content, 0)
                .unwrap(),
            Outcome::Error
        );
        assert_eq!(
            &writer[writer.len() - (MAX_PKT_DATA + 3 + 16)..writer.len() - 4],
            &[b"fff0".as_slice(), &vec![b'x'; MAX_PKT_DATA], b"0007xxx"].concat()
        );
        assert!(write_pkt(&mut Vec::new(), &vec![b'x'; MAX_PKT_DATA + 1]).is_err());
    }

    #[test]
    fn error_before_content_and_error_after_partial_content_are_handled() {
        let (client, _) = client(
            &[SERVER, b"0015capability=clean\n0000"].concat(),
            Capabilities::new(true, false),
        );
        let mut client = client.unwrap();
        let mut writer = Vec::new();
        let mut before = Cursor::new(b"0011status=error\n0000".to_vec());
        assert_eq!(
            client
                .request(&mut before, &mut writer, Command::Clean, "x", &[], 10)
                .unwrap(),
            Outcome::Error
        );
        let mut after =
            Cursor::new(b"0013status=success\n0007abc00000011status=error\n0000".to_vec());
        assert_eq!(
            client
                .request(&mut after, &mut writer, Command::Clean, "x", &[], 10)
                .unwrap(),
            Outcome::Error
        );
    }

    #[test]
    fn abort_is_sticky_per_command_but_other_command_remains_available() {
        let capbytes = b"0015capability=clean\n0016capability=smudge\n0000";
        let (client, _) = client(&[SERVER, capbytes].concat(), Capabilities::new(true, true));
        let mut client = client.unwrap();
        let mut response = Cursor::new(b"0011status=abort\n0000".to_vec());
        let mut writer = Vec::new();
        assert_eq!(
            client
                .request(&mut response, &mut writer, Command::Clean, "x", &[], 10)
                .unwrap(),
            Outcome::Abort
        );
        let previous = writer.len();
        assert_eq!(
            client
                .request(&mut response, &mut writer, Command::Clean, "x", &[], 10)
                .unwrap(),
            Outcome::Abort
        );
        assert_eq!(writer.len(), previous);
        let mut smudge_response = Cursor::new(b"0013status=success\n00000000".to_vec());
        assert_eq!(
            client
                .request(
                    &mut smudge_response,
                    &mut writer,
                    Command::Smudge,
                    "x",
                    &[],
                    10
                )
                .unwrap(),
            Outcome::Success(Vec::new())
        );
    }

    #[test]
    fn invalid_requests_write_nothing_and_delayed_is_a_protocol_error() {
        let (client, _) = client(
            &[SERVER, b"0015capability=clean\n0000"].concat(),
            Capabilities::new(true, false),
        );
        let mut client = client.unwrap();
        let mut writer = Vec::new();
        let mut reader = Cursor::new(Vec::new());
        assert!(matches!(
            client.request(&mut reader, &mut writer, Command::Smudge, "x", &[], 1),
            Err(ProtocolError::Unsupported(_))
        ));
        assert!(matches!(
            client.request(&mut reader, &mut writer, Command::Clean, "", &[], 1),
            Err(ProtocolError::InvalidPath)
        ));
        assert!(matches!(
            client.request(&mut reader, &mut writer, Command::Clean, "a\nb", &[], 1),
            Err(ProtocolError::InvalidPath)
        ));
        assert!(matches!(
            client.request(&mut reader, &mut writer, Command::Clean, "a\0b", &[], 1),
            Err(ProtocolError::InvalidPath)
        ));
        assert!(writer.is_empty());
        let mut delayed = Cursor::new(b"0013status=delayed\n0000".to_vec());
        assert!(matches!(
            client.request(&mut delayed, &mut writer, Command::Clean, "x", &[], 1),
            Err(ProtocolError::Protocol(_))
        ));
    }

    #[test]
    fn output_limit_is_enforced_and_unknown_keys_are_ignored() {
        let (client, _) = client(
            &[SERVER, b"0015capability=clean\n0000"].concat(),
            Capabilities::new(true, false),
        );
        let mut client = client.unwrap();
        let mut reader =
            Cursor::new(b"000bfoo=bar\n0013status=success\n0009HELLO00000000".to_vec());
        let mut writer = Vec::new();
        assert!(matches!(
            client.request(&mut reader, &mut writer, Command::Clean, "x", &[], 4),
            Err(ProtocolError::OutputLimit)
        ));
    }

    #[test]
    fn missing_status_is_a_protocol_error() {
        let (client, _) = client(
            &[SERVER, b"0015capability=clean\n0000"].concat(),
            Capabilities::new(true, false),
        );
        let mut client = client.unwrap();
        let mut reader = Cursor::new(b"0000".to_vec());
        let mut writer = Vec::new();
        assert!(matches!(
            client.request(&mut reader, &mut writer, Command::Clean, "x", &[], 8),
            Err(ProtocolError::Protocol(_))
        ));
    }

    #[test]
    fn malformed_pkt_lines_and_truncation_are_rejected() {
        for bytes in [
            b"0001".as_slice(),
            b"0002".as_slice(),
            b"0003".as_slice(),
            b"zzzz".as_slice(),
            b"fff1".as_slice(),
        ] {
            assert!(read_pkt(&mut Cursor::new(bytes)).is_err());
        }
        assert!(
            matches!(read_pkt(&mut Cursor::new(b"0007ab")), Err(error) if error.kind() == io::ErrorKind::UnexpectedEof)
        );
        assert_eq!(
            read_pkt(&mut Cursor::new(b"0007ABc")).unwrap(),
            Pkt::Data(b"ABc".to_vec())
        );
        assert_eq!(
            read_pkt(&mut Cursor::new(b"000AabcDEF")).unwrap(),
            Pkt::Data(b"abcDEF".to_vec())
        );
        assert_eq!(
            read_pkt(&mut Cursor::new(b"0004")).unwrap(),
            Pkt::Data(Vec::new())
        );
    }
}
