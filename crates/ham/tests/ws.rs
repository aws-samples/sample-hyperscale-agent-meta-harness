//! WebSocket framing for `ham shell` (RFC 6455 section 5, the examples of 5.7) and the command
//! shell's channel protocol (AgentCore interactive shells: the first byte of each binary frame
//! names the channel; STATUS carries JSON).
#![allow(clippy::unwrap_used)]

use ham::ws::{Decoder, Message, WsError, encode, shell_status};

#[test]
fn frames_encode_as_rfc_6455_shows_them() {
    // 5.7: a single-frame masked text message "Hello".
    assert_eq!(
        encode(0x1, b"Hello", Some([0x37, 0xfa, 0x21, 0x3d])),
        [
            0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58
        ]
    );
    // Unmasked, 256 bytes (16-bit length) and 64 KiB (64-bit length) binary messages.
    let f = encode(0x2, &[7u8; 256], None);
    assert_eq!(&f[..4], [0x82, 0x7e, 0x01, 0x00]);
    assert_eq!(f.len(), 4 + 256);
    let f = encode(0x2, &vec![7u8; 65536], None);
    assert_eq!(&f[..10], [0x82, 0x7f, 0, 0, 0, 0, 0, 1, 0, 0]);
    let f = encode(0x2, &[1, 2, 3], Some([0, 0, 0, 0]));
    assert_eq!(f, [0x82, 0x83, 0, 0, 0, 0, 1, 2, 3]);
}

#[test]
fn server_frames_decode_across_reads_fragments_and_control_frames() {
    let mut d = Decoder::default();
    // 5.7: a fragmented unmasked text message, delivered one byte at a time, with a ping between.
    let mut wire = vec![0x01, 0x03, 0x48, 0x65, 0x6c];
    wire.extend([0x89, 0x05, b'H', b'e', b'l', b'l', b'o']);
    wire.extend([0x80, 0x02, 0x6c, 0x6f]);
    wire.extend(encode(0x2, &[0x01, b'o', b'k'], None));
    wire.extend([0x88, 0x02, 0x03, 0xe8]);
    let mut got = Vec::new();
    for b in wire {
        d.push(&[b]);
        while let Some(m) = d.next_message().unwrap() {
            got.push(m);
        }
    }
    assert_eq!(
        got,
        vec![
            Message::Ping(b"Hello".to_vec()),
            Message::Text(b"Hello".to_vec()),
            Message::Binary(vec![0x01, b'o', b'k']),
            Message::Close(Some(1000)),
        ]
    );
    let mut d = Decoder::default();
    d.push(&[0x8a, 0x00, 0x88, 0x00]);
    assert_eq!(d.next_message().unwrap(), Some(Message::Pong(vec![])));
    assert_eq!(d.next_message().unwrap(), Some(Message::Close(None)));
    assert_eq!(d.next_message().unwrap(), None);
}

#[test]
fn a_masked_server_frame_a_long_control_frame_and_a_huge_frame_are_errors() {
    let mut d = Decoder::default();
    d.push(&encode(0x2, b"x", Some([1, 2, 3, 4])));
    assert!(matches!(d.next_message(), Err(WsError::Protocol(_))));
    let mut d = Decoder::default();
    d.push(&[0x89, 0x7e, 0x00, 0x80]);
    assert!(matches!(d.next_message(), Err(WsError::Protocol(_))));
    let mut d = Decoder::default();
    d.push(&[0x82, 0x7f, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00]);
    assert!(
        matches!(d.next_message(), Err(WsError::Protocol(_))),
        "a frame above the limit"
    );
    let mut d = Decoder::default();
    d.push(&[0x80, 0x01, 0x41]);
    assert!(
        matches!(d.next_message(), Err(WsError::Protocol(_))),
        "a continuation with nothing to continue"
    );
}

#[test]
fn status_frames_name_the_shell_the_reconnect_and_the_exit_code() {
    let s = shell_status(
        br#"{"kind":"Status","apiVersion":"v1","metadata":{"shellId":"my-shell","reconnected":true,"bytesDropped":12},"status":"Success"}"#,
    );
    assert_eq!(s.shell_id.as_deref(), Some("my-shell"));
    assert!(s.reconnected);
    assert_eq!(s.exit_code, None);
    let s = shell_status(
        br#"{"kind":"Status","status":"Failure","details":{"causes":[{"reason":"ExitCode","message":"7"}]}}"#,
    );
    assert_eq!(s.exit_code, Some(7));
    assert!(s.shell_id.is_none() && !s.reconnected);
    assert_eq!(shell_status(b"not json"), Default::default());
}
