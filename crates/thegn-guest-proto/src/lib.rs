//! Guest wire framing shared by nodes and control planes.

use prost::Message;
use std::io::{self, Read, Write};

include!(concat!(env!("OUT_DIR"), "/thegn.guest.v0.rs"));

pub const PROTOCOL_V0: u32 = 0;
pub const SPEC_V0: &str = "thegn.dev/jobs/v0";
/// Bootstrap bound before either peer has advertised a limit.
pub const MAX_FRAME_BYTES: u32 = 1024 * 1024;

impl Hello {
    /// Returns the frame limit agreed by a v0 host and this peer.
    pub fn negotiate_v0(&self, local_limit: u32) -> io::Result<u32> {
        if !self.protocol_versions.contains(&PROTOCOL_V0)
            || !self.spec_versions.iter().any(|version| version == SPEC_V0)
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "no common guest protocol and job spec version",
            ));
        }
        let limit = self.max_frame_bytes.min(local_limit).min(MAX_FRAME_BYTES);
        if limit == 0 || self.agent_version.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid guest hello",
            ));
        }
        Ok(limit)
    }
}

fn check_frame(size: usize, limit: u32) -> io::Result<()> {
    if size == 0 || size > limit.min(MAX_FRAME_BYTES) as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "guest frame length outside allowed range",
        ));
    }
    Ok(())
}

pub fn write_frame(writer: &mut impl Write, message: &Envelope, limit: u32) -> io::Result<()> {
    let size = message.encoded_len();
    check_frame(size, limit)?;
    writer.write_all(&(size as u32).to_be_bytes())?;
    writer.write_all(&message.encode_to_vec())
}

pub fn read_frame(reader: &mut impl Read, limit: u32) -> io::Result<Envelope> {
    let mut header = [0; 4];
    reader.read_exact(&mut header)?;
    let size = u32::from_be_bytes(header) as usize;
    check_frame(size, limit)?;
    let mut payload = vec![0; size];
    reader.read_exact(&mut payload)?;
    let message = Envelope::decode(&*payload)?;
    if message.body.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unknown or absent guest message",
        ));
    }
    Ok(message)
}
