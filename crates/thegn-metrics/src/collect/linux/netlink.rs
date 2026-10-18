//! Bounded Linux netlink transport and length-checked wire iterators.
use std::io;
use std::mem::size_of;
use std::os::fd::RawFd;
use std::time::Instant;

const NLMSG_HDRLEN: usize = 16;
const NLMSG_ALIGNTO: usize = 4;
const RTA_HDRLEN: usize = 4;
// NLMSG_* and rtattr alignment values from include/uapi/linux/netlink.h.
const NLMSG_DONE: u16 = 3;
const NLMSG_ERROR: u16 = 2;
// NETLINK_SOCK_DIAG from include/uapi/linux/netlink.h.
const NETLINK_SOCK_DIAG: i32 = 4;

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
}

impl<'a> MessageIter<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            offset: 0,
            stopped: false,
            done: false,
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
            self.stopped = true;
            self.done = true;
            return None;
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

/// A NETLINK_SOCK_DIAG socket. Receive data is always written into caller-owned storage.
pub struct NetlinkSocket {
    fd: RawFd,
}

impl NetlinkSocket {
    pub fn sock_diag() -> io::Result<Self> {
        Self::open(NETLINK_SOCK_DIAG)
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
        // SAFETY: sockaddr_nl is plain old data; all-zero is a valid value (libc keeps nl_pad private).
        let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        // SAFETY: `addr` is a live sockaddr_nl and its exact size is supplied.
        let result = unsafe {
            libc::bind(
                fd,
                (&addr as *const libc::sockaddr_nl).cast(),
                size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if result < 0 {
            let error = io::Error::last_os_error();
            // SAFETY: fd was returned by socket and is owned here.
            unsafe { libc::close(fd) };
            return Err(error);
        }
        Ok(Self { fd })
    }

    pub fn send(&mut self, request: &[u8]) -> io::Result<()> {
        // SAFETY: sockaddr_nl is plain old data; all-zero is a valid value (libc keeps nl_pad private).
        let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        // SAFETY: request and destination are valid for the duration of sendto.
        let sent = unsafe {
            libc::sendto(
                self.fd,
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

    pub fn recv(&mut self, buffer: &mut [u8], deadline: Instant) -> io::Result<usize> {
        if buffer.len() > 64 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "netlink receive buffer exceeds 64 KiB",
            ));
        }
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
                fd: self.fd,
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
            // SAFETY: buffer is writable for its length; MSG_DONTWAIT prevents a blocking receive.
            let count = unsafe {
                libc::recv(
                    self.fd,
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    libc::MSG_DONTWAIT,
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
            return Ok(count as usize);
        }
    }
}

impl Drop for NetlinkSocket {
    fn drop(&mut self) {
        // SAFETY: fd is exclusively owned by this socket and closed exactly once.
        unsafe { libc::close(self.fd) };
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
