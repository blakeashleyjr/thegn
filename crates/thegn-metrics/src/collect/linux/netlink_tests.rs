use super::{AttributeIter, MessageIter, NetlinkSocket};
use std::io;
use std::time::{Duration, Instant};

fn message(kind: u16, payload: &[u8]) -> Vec<u8> {
    let len = 16 + payload.len();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(len as u32).to_ne_bytes());
    bytes.extend_from_slice(&kind.to_ne_bytes());
    bytes.extend_from_slice(&0_u16.to_ne_bytes());
    bytes.extend_from_slice(&1_u32.to_ne_bytes());
    bytes.extend_from_slice(&0_u32.to_ne_bytes());
    bytes.extend_from_slice(payload);
    bytes.resize((len + 3) & !3, 0);
    bytes
}

#[test]
fn message_iter_honors_alignment_and_stops_at_done() {
    let mut bytes = message(40, &[7]);
    bytes.extend_from_slice(&message(41, &[8, 9, 10, 11]));
    bytes.extend_from_slice(&message(3, &[]));
    let mut iter = MessageIter::new(&bytes);
    assert_eq!(iter.next().unwrap().unwrap().payload, &[7]);
    assert_eq!(iter.next().unwrap().unwrap().payload, &[8, 9, 10, 11]);
    assert!(iter.next().is_none());
    assert!(iter.is_done());
    assert!(iter.next().is_none());
}

#[test]
fn message_iter_surfaces_error_codes_and_rejects_bad_lengths() {
    let denied = message(2, &(-13_i32).to_ne_bytes());
    let error = MessageIter::new(&denied).next().unwrap().unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::EACCES));

    let mut oversized = message(40, &[]);
    oversized[0..4].copy_from_slice(&u32::MAX.to_ne_bytes());
    assert_eq!(
        MessageIter::new(&oversized)
            .next()
            .unwrap()
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );

    let mut zero = message(40, &[]);
    zero[0..4].copy_from_slice(&0_u32.to_ne_bytes());
    assert_eq!(
        MessageIter::new(&zero).next().unwrap().unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
}

#[test]
fn attributes_reject_lengths_shorter_than_the_header() {
    let bytes = [3_u8, 0, 1, 0];
    assert_eq!(
        AttributeIter::new(&bytes)
            .next()
            .unwrap()
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
}

#[test]
fn live_sock_diag_dump_completes_within_deadline_when_available() {
    let mut socket = match NetlinkSocket::sock_diag() {
        Ok(socket) => socket,
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::EPERM | libc::EACCES | libc::EAFNOSUPPORT | libc::EPROTONOSUPPORT)
            ) =>
        {
            return;
        }
        Err(error) => panic!("cannot open sock_diag socket: {error}"),
    };
    let mut rows = Vec::new();
    if let Err(error) = super::super::sock_diag::dump_tcp(
        &mut socket,
        &mut rows,
        Instant::now() + Duration::from_secs(1),
    ) {
        if matches!(
            error.raw_os_error(),
            Some(libc::EPERM | libc::EACCES | libc::EAFNOSUPPORT | libc::EPROTONOSUPPORT)
        ) {
            return;
        }
        panic!("sock_diag dump failed: {error}");
    }
}
