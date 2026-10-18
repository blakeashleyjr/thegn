//! Bounded Linux netlink transport and length-checked wire iterators.
use std::io;
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Instant;

const NLMSG_HDRLEN: usize = 16;
const NLMSG_ALIGNTO: usize = 4;
const RTA_HDRLEN: usize = 4;
pub(crate) const NETLINK_RECV_BUFFER_LEN: usize = 32 * 1024;
const NLMSG_DONE: u16 = libc::NLMSG_DONE as u16;
const NLMSG_ERROR: u16 = libc::NLMSG_ERROR as u16;

/// The decoded fields in a native-endian `nlmsghdr`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetlinkHeader {
    pub len: u32,
    pub kind: u16,
    pub flags: u16,
    pub sequence: u32,
    pub pid: u32,
}

/// A checked view into a netlink message payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetlinkMessage<'a> {
    pub header: NetlinkHeader,
    pub payload: &'a [u8],
}

/// Iterates aligned netlink messages. Malformed messages are yielded once as errors.
pub struct MessageIter<'a> {
    bytes: &'a [u8],
    offset: usize,
    stopped: bool,
    done: bool,
    expected_sequence: Option<u32>,
}

impl<'a> MessageIter<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            offset: 0,
            stopped: false,
            done: false,
            expected_sequence: None,
        }
    }

    pub fn for_sequence(bytes: &'a [u8], sequence: u32) -> Self {
        Self {
            bytes,
            offset: 0,
            stopped: false,
            done: false,
            expected_sequence: Some(sequence),
        }
    }

    pub fn is_done(&self) -> bool {
        self.done
    }
}

impl<'a> Iterator for MessageIter<'a> {
    type Item = io::Result<NetlinkMessage<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.stopped || self.offset == self.bytes.len() {
            return None;
        }
        let remaining = &self.bytes[self.offset..];
        if remaining.len() < NLMSG_HDRLEN {
            self.stopped = true;
            return Some(Err(invalid("truncated netlink header")));
        }
        let len = read_u32(remaining, 0).map(|v| v as usize)?;
        if len < NLMSG_HDRLEN || len > remaining.len() {
            self.stopped = true;
            return Some(Err(invalid("invalid netlink message length")));
        }
        let header = NetlinkHeader {
            len: len as u32,
            kind: read_u16(remaining, 4)?,
            flags: read_u16(remaining, 6)?,
            sequence: read_u32(remaining, 8)?,
            pid: read_u32(remaining, 12)?,
        };
        let payload = &remaining[NLMSG_HDRLEN..len];
        let aligned = align(len, NLMSG_ALIGNTO);
        if aligned > remaining.len() && len != remaining.len() {
            self.stopped = true;
            return Some(Err(invalid("truncated netlink alignment padding")));
        }
        self.offset += aligned.min(remaining.len());
        if header.kind == NLMSG_DONE {
            if self.expected_sequence == Some(header.sequence) {
                self.done = true;
                self.stopped = true;
            }
            if self.expected_sequence.is_none() {
                self.stopped = true;
                self.done = true;
                return None;
            }
            return Some(Ok(NetlinkMessage { header, payload }));
        }
        if header.kind == NLMSG_ERROR
            && self.expected_sequence.is_some()
            && self.expected_sequence != Some(header.sequence)
        {
            return Some(Ok(NetlinkMessage { header, payload }));
        }
        if header.kind == NLMSG_ERROR {
            if payload.len() < 4 {
                self.stopped = true;
                return Some(Err(invalid("truncated NLMSG_ERROR")));
            }
            let code = i32::from_ne_bytes([payload[0], payload[1], payload[2], payload[3]]);
            if code == 0 {
                return Some(Ok(NetlinkMessage { header, payload }));
            }
            self.stopped = true;
            return Some(Err(io::Error::from_raw_os_error(code.saturating_neg())));
        }
        Some(Ok(NetlinkMessage { header, payload }))
    }
}

/// A checked rtattr/nlattr view. Both Linux attribute headers use `u16 len, u16 type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attribute<'a> {
    pub kind: u16,
    pub payload: &'a [u8],
}

pub struct AttributeIter<'a> {
    bytes: &'a [u8],
    offset: usize,
    stopped: bool,
}

impl<'a> AttributeIter<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            offset: 0,
            stopped: false,
        }
    }
}

impl<'a> Iterator for AttributeIter<'a> {
    type Item = io::Result<Attribute<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.stopped || self.offset == self.bytes.len() {
            return None;
        }
        let remaining = &self.bytes[self.offset..];
        if remaining.len() < RTA_HDRLEN {
            self.stopped = true;
            return Some(Err(invalid("truncated netlink attribute header")));
        }
        let len = match read_u16(remaining, 0) {
            Some(len) => usize::from(len),
            None => {
                self.stopped = true;
                return Some(Err(invalid("truncated netlink attribute length")));
            }
        };
        if len < RTA_HDRLEN || len > remaining.len() {
            self.stopped = true;
            return Some(Err(invalid("invalid netlink attribute length")));
        }
        let kind = match read_u16(remaining, 2) {
            Some(kind) => kind,
            None => {
                self.stopped = true;
                return Some(Err(invalid("truncated netlink attribute type")));
            }
        };
        let aligned = align(len, NLMSG_ALIGNTO);
        if aligned > remaining.len() && len != remaining.len() {
            self.stopped = true;
            return Some(Err(invalid("truncated netlink attribute padding")));
        }
        self.offset += aligned.min(remaining.len());
        Some(Ok(Attribute {
            kind,
            payload: &remaining[RTA_HDRLEN..len],
        }))
    }
}

/// A NETLINK_SOCK_DIAG socket with a reusable bounded receive buffer.
pub struct NetlinkSocket {
    fd: OwnedFd,
    next_sequence: u32,
    receive_buffer: Box<[u8; NETLINK_RECV_BUFFER_LEN]>,
    received_len: usize,
}

impl NetlinkSocket {
    pub fn sock_diag() -> io::Result<Self> {
        Self::open(libc::NETLINK_SOCK_DIAG as i32)
    }

    pub fn open(protocol: i32) -> io::Result<Self> {
        // SAFETY: `socket` has no pointer arguments; the returned fd is checked below.
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                protocol,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: socket returns a new owned descriptor when it succeeds.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: sockaddr_nl is plain old data; all-zero is a valid value (libc keeps nl_pad private).
        let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        // SAFETY: `addr` is a live sockaddr_nl and its exact size is supplied.
        let result = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                (&addr as *const libc::sockaddr_nl).cast(),
                size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if result < 0 {
            let error = io::Error::last_os_error();
            return Err(error);
        }
        Ok(Self {
            fd,
            next_sequence: 0,
            receive_buffer: Box::new([0_u8; NETLINK_RECV_BUFFER_LEN]),
            received_len: 0,
        })
    }

    pub(crate) fn next_sequence(&mut self) -> u32 {
        self.next_sequence = self.next_sequence.wrapping_add(1).max(1);
        self.next_sequence
    }

    pub fn send(&mut self, request: &[u8]) -> io::Result<()> {
        // SAFETY: sockaddr_nl is plain old data; all-zero is a valid value (libc keeps nl_pad private).
        let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        // SAFETY: request and destination are valid for the duration of sendto.
        let sent = unsafe {
            libc::sendto(
                self.fd.as_raw_fd(),
                request.as_ptr().cast(),
                request.len(),
                0,
                (&addr as *const libc::sockaddr_nl).cast(),
                size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if sent < 0 {
            return Err(io::Error::last_os_error());
        }
        if sent as usize != request.len() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "short netlink send",
            ));
        }
        Ok(())
    }

    pub fn recv(&mut self, deadline: Instant) -> io::Result<usize> {
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "netlink deadline elapsed",
                ));
            }
            let millis = deadline
                .duration_since(now)
                .as_millis()
                .min(i32::MAX as u128) as i32;
            let mut pollfd = libc::pollfd {
                fd: self.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: pollfd points to one initialized entry and the bounded timeout is nonnegative.
            let ready = unsafe { libc::poll(&mut pollfd, 1, millis.max(1)) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if ready == 0 {
                continue;
            }
            // SAFETY: sender and its length are initialized, and buffer is writable for its length.
            let mut sender: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
            let mut sender_len = size_of::<libc::sockaddr_nl>() as libc::socklen_t;
            // SAFETY: all pointers refer to live storage; MSG_DONTWAIT prevents blocking.
            let count = unsafe {
                libc::recvfrom(
                    self.fd.as_raw_fd(),
                    self.receive_buffer.as_mut_ptr().cast(),
                    self.receive_buffer.len(),
                    libc::MSG_DONTWAIT,
                    (&mut sender as *mut libc::sockaddr_nl).cast(),
                    &mut sender_len,
                )
            };
            if count < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock
                    || error.kind() == io::ErrorKind::Interrupted
                {
                    continue;
                }
                return Err(error);
            }
            if sender.nl_pid != 0 {
                return Err(invalid("netlink response did not come from kernel"));
            }
            self.received_len = count as usize;
            return Ok(self.received_len);
        }
    }

    pub(crate) fn received(&self) -> &[u8] {
        &self.receive_buffer[..self.received_len]
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_ne_bytes(
        bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn align(value: usize, to: usize) -> usize {
    value.saturating_add(to - 1) & !(to - 1)
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
#[path = "netlink_tests.rs"]
mod tests;
