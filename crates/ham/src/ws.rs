//! WebSocket framing for the AgentCore command shell (RFC 6455) over a hyper HTTP/1.1 upgrade: masked client
//! frames, unmasked server frames, fragments joined, pings answered by the caller. The command
//! shell's channel protocol rides in binary frames: the first byte names the channel (0x00 STDIN,
//! 0x01 STDOUT, 0x02 STDERR, 0x03 STATUS JSON, 0x04 RESIZE JSON, 0x05 HEARTBEAT, 0xFF CLOSE).
//!
//! The upgrade answer's `Sec-WebSocket-Accept` is not checked (it needs SHA-1, which the workspace
//! does not carry); TLS and the SigV4-signed upgrade already bind the connection to the service.

use serde_json::Value;

/// A server frame larger than this is a protocol error (the service sends at most 64 KB).
pub const MAX_FRAME: u64 = 16 * 1024 * 1024;

pub const OP_CONTINUATION: u8 = 0x0;
pub const OP_TEXT: u8 = 0x1;
pub const OP_BINARY: u8 = 0x2;
pub const OP_CLOSE: u8 = 0x8;
pub const OP_PING: u8 = 0x9;
pub const OP_PONG: u8 = 0xA;

pub const CH_STDIN: u8 = 0x00;
pub const CH_STDOUT: u8 = 0x01;
pub const CH_STDERR: u8 = 0x02;
pub const CH_STATUS: u8 = 0x03;
pub const CH_RESIZE: u8 = 0x04;
pub const CH_HEARTBEAT: u8 = 0x05;
pub const CH_CLOSE: u8 = 0xFF;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WsError {
    #[error("WebSocket protocol: {0}")]
    Protocol(String),
}

/// One whole message from the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    Text(Vec<u8>),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    /// The close code, when the frame carries one.
    Close(Option<u16>),
}

/// One frame with FIN set; `mask` for a client frame (RFC 6455 5.3).
pub fn encode(opcode: u8, payload: &[u8], mask: Option<[u8; 4]>) -> Vec<u8> {
    let mut f = vec![0x80 | (opcode & 0x0f)];
    let bit = if mask.is_some() { 0x80 } else { 0 };
    let n = payload.len();
    if n < 126 {
        f.push(bit | n as u8);
    } else if n <= usize::from(u16::MAX) {
        f.push(bit | 126);
        f.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        f.push(bit | 127);
        f.extend_from_slice(&(n as u64).to_be_bytes());
    }
    match mask {
        Some(m) => {
            f.extend_from_slice(&m);
            f.extend(payload.iter().enumerate().map(|(i, b)| b ^ m[i % 4]));
        }
        None => f.extend_from_slice(payload),
    }
    f
}

/// A masked client frame with a fresh key.
pub fn client_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut m = [0u8; 4];
    // A fixed key would still frame correctly; the entropy failure is not worth failing a send.
    let _ = getrandom::fill(&mut m);
    encode(opcode, payload, Some(m))
}

/// A command-shell message on `channel`.
pub fn shell_frame(channel: u8, data: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(data.len() + 1);
    p.push(channel);
    p.extend_from_slice(data);
    client_frame(OP_BINARY, &p)
}

/// Server frames from a byte stream, fed in any pieces.
#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
    /// The data message being continued: its opcode and the bytes so far.
    partial: Option<(u8, Vec<u8>)>,
}

impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next whole message, `None` until one is complete.
    pub fn next_message(&mut self) -> Result<Option<Message>, WsError> {
        loop {
            let Some(RawFrame {
                fin,
                opcode,
                payload,
                used,
            }) = self.frame()?
            else {
                return Ok(None);
            };
            self.buf.drain(..used);
            match opcode {
                OP_PING => return Ok(Some(Message::Ping(payload))),
                OP_PONG => return Ok(Some(Message::Pong(payload))),
                OP_CLOSE => {
                    let code =
                        (payload.len() >= 2).then(|| u16::from_be_bytes([payload[0], payload[1]]));
                    return Ok(Some(Message::Close(code)));
                }
                OP_CONTINUATION => {
                    let Some((op, mut data)) = self.partial.take() else {
                        return Err(WsError::Protocol(
                            "a continuation frame with nothing to continue".into(),
                        ));
                    };
                    data.extend_from_slice(&payload);
                    if fin {
                        return Ok(Some(message(op, data)));
                    }
                    self.partial = Some((op, data));
                }
                OP_TEXT | OP_BINARY if fin => return Ok(Some(message(opcode, payload))),
                OP_TEXT | OP_BINARY => self.partial = Some((opcode, payload)),
                other => {
                    return Err(WsError::Protocol(format!("unknown opcode {other:#x}")));
                }
            }
        }
    }

    /// The first complete frame in the buffer.
    fn frame(&self) -> Result<Option<RawFrame>, WsError> {
        let b = &self.buf;
        if b.len() < 2 {
            return Ok(None);
        }
        let fin = b[0] & 0x80 != 0;
        let opcode = b[0] & 0x0f;
        if b[1] & 0x80 != 0 {
            return Err(WsError::Protocol("the server sent a masked frame".into()));
        }
        let (len, mut at) = match b[1] & 0x7f {
            126 if b.len() >= 4 => (u64::from(u16::from_be_bytes([b[2], b[3]])), 4),
            127 if b.len() >= 10 => {
                let mut n = [0u8; 8];
                n.copy_from_slice(&b[2..10]);
                (u64::from_be_bytes(n), 10)
            }
            126 | 127 => return Ok(None),
            n => (u64::from(n), 2),
        };
        if opcode >= 0x8 && (len > 125 || !fin) {
            return Err(WsError::Protocol(
                "a control frame longer than 125 bytes or fragmented".into(),
            ));
        }
        if len > MAX_FRAME {
            return Err(WsError::Protocol(format!(
                "a {len}-byte frame is above the {MAX_FRAME}-byte limit"
            )));
        }
        let len = len as usize;
        if b.len() < at + len {
            return Ok(None);
        }
        let payload = b[at..at + len].to_vec();
        at += len;
        Ok(Some(RawFrame {
            fin,
            opcode,
            payload,
            used: at,
        }))
    }
}

/// One frame and the bytes it took in the buffer.
struct RawFrame {
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
    used: usize,
}

fn message(opcode: u8, data: Vec<u8>) -> Message {
    if opcode == OP_TEXT {
        Message::Text(data)
    } else {
        Message::Binary(data)
    }
}

/// What a STATUS frame says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShellStatus {
    pub shell_id: Option<String>,
    pub reconnected: bool,
    /// The shell's exit code from an `ExitCode` cause of the final STATUS.
    pub exit_code: Option<i32>,
}

pub fn shell_status(json: &[u8]) -> ShellStatus {
    let Ok(v) = serde_json::from_slice::<Value>(json) else {
        return ShellStatus::default();
    };
    let exit_code = v
        .pointer("/details/causes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|c| c.get("reason").and_then(Value::as_str) == Some("ExitCode"))
        .and_then(|c| c.get("message"))
        .and_then(|m| match m {
            Value::String(s) => s.trim().parse::<i32>().ok(),
            Value::Number(n) => n.as_i64().and_then(|n| i32::try_from(n).ok()),
            _ => None,
        });
    ShellStatus {
        shell_id: v
            .pointer("/metadata/shellId")
            .and_then(Value::as_str)
            .map(str::to_string),
        reconnected: v
            .pointer("/metadata/reconnected")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        exit_code,
    }
}
