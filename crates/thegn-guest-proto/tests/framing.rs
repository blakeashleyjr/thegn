use std::io::{Cursor, ErrorKind};
use thegn_guest_proto::{
    Envelope, Hello, MAX_FRAME_BYTES, SPEC_V0, envelope::Body, read_frame, write_frame,
};

const GOLDEN: &[u8] = include_bytes!("fixtures/hello.frame");

#[test]
fn independent_protobuf_encoder_agrees_on_frame_and_handshake() {
    let message = Envelope {
        body: Some(Body::Hello(Hello {
            agent_version: "0.1".into(),
            protocol_versions: vec![0],
            spec_versions: vec![SPEC_V0.into()],
            capabilities: vec!["services".into()],
            max_frame_bytes: 4096,
        })),
    };
    let mut out = Vec::new();
    write_frame(&mut out, &message, MAX_FRAME_BYTES).unwrap();
    assert_eq!(out, GOLDEN);
    let mut input = GOLDEN;
    let decoded = read_frame(&mut input, MAX_FRAME_BYTES).unwrap();
    assert_eq!(decoded, message);
    let Some(Body::Hello(mut hello)) = decoded.body else {
        panic!("missing hello")
    };
    assert_eq!(hello.negotiate_v0(2000).unwrap(), 2000);
    hello.protocol_versions = vec![1];
    assert_eq!(
        hello.negotiate_v0(2000).unwrap_err().kind(),
        ErrorKind::Unsupported
    );
}

#[test]
fn oversized_frame_stops_after_header_and_partial_frames_fail() {
    let mut input = Cursor::new(GOLDEN);
    assert_eq!(
        read_frame(&mut input, 1).unwrap_err().kind(),
        ErrorKind::InvalidData
    );
    assert_eq!(input.position(), 4);
    let mut truncated = &GOLDEN[..GOLDEN.len() - 1];
    assert_eq!(
        read_frame(&mut truncated, MAX_FRAME_BYTES)
            .unwrap_err()
            .kind(),
        ErrorKind::UnexpectedEof
    );
    assert!(read_frame(&mut &b"\x00\x00\x00\x02\x10\x01"[..], 20).is_err());
}
