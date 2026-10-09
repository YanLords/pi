use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use thiserror::Error;

pub const MAX_FRAME_SIZE: usize = 8 * 1024 * 1024; // 8 MiB

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame size {0} exceeds maximum of {MAX_FRAME_SIZE} bytes")]
    FrameTooLarge(usize),
    #[error("invalid utf-8 payload: {0}")]
    InvalidUtf8(#[from] std::string::FromUtf8Error),
    #[error("json serialize/deserialize error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("protocol violation: {0}")]
    Violation(String),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct RawRequest {
    pub id: u64,
    pub op: String,
    #[serde(flatten)]
    pub params: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ErrorDetail {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum Response {
    Ok {
        id: u64,
        ok: bool,
        result: serde_json::Value,
    },
    Err {
        id: u64,
        ok: bool,
        error: ErrorDetail,
    },
}

impl Response {
    pub fn ok(id: u64, result: serde_json::Value) -> Self {
        Response::Ok {
            id,
            ok: true,
            result,
        }
    }

    pub fn err(id: u64, code: impl Into<String>, message: impl Into<String>) -> Self {
        Response::Err {
            id,
            ok: false,
            error: ErrorDetail {
                code: code.into(),
                message: message.into(),
            },
        }
    }

    pub fn id(&self) -> u64 {
        match self {
            Response::Ok { id, .. } | Response::Err { id, .. } => *id,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct EventFrame {
    pub event: String,
    pub data: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum OutgoingMessage {
    Response(Response),
    Event(EventFrame),
}

pub fn read_frame<R: Read>(reader: &mut R) -> Result<Option<Vec<u8>>, ProtocolError> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(ProtocolError::Io(e)),
    }
    let length = u32::from_be_bytes(len_buf) as usize;
    if length > MAX_FRAME_SIZE {
        return Err(ProtocolError::FrameTooLarge(length));
    }
    let mut payload = vec![0u8; length];
    reader.read_exact(&mut payload)?;
    Ok(Some(payload))
}

pub fn write_frame<W: Write>(writer: &mut W, payload: &[u8]) -> Result<(), ProtocolError> {
    if payload.len() > MAX_FRAME_SIZE {
        return Err(ProtocolError::FrameTooLarge(payload.len()));
    }
    let len_bytes = (payload.len() as u32).to_be_bytes();
    writer.write_all(&len_bytes)?;
    writer.write_all(payload)?;
    writer.flush()?;
    Ok(())
}

pub fn write_message<W: Write>(writer: &mut W, msg: &OutgoingMessage) -> Result<(), ProtocolError> {
    let json = serde_json::to_vec(msg)?;
    write_frame(writer, &json)
}

pub fn write_response<W: Write>(writer: &mut W, resp: Response) -> Result<(), ProtocolError> {
    write_message(writer, &OutgoingMessage::Response(resp))
}

pub fn write_event<W: Write>(
    writer: &mut W,
    event: &str,
    data: serde_json::Value,
) -> Result<(), ProtocolError> {
    write_message(
        writer,
        &OutgoingMessage::Event(EventFrame {
            event: event.to_string(),
            data,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn roundtrip_request_frame() {
        let req = RawRequest {
            id: 42,
            op: "authorize".into(),
            params: serde_json::json!({"action": "shell", "detail": "cargo test"}),
        };
        let mut buf = Vec::new();
        let payload = serde_json::to_vec(&req).unwrap();
        write_frame(&mut buf, &payload).unwrap();

        let mut cursor = Cursor::new(buf);
        let read = read_frame(&mut cursor).unwrap().expect("frame");
        let decoded: RawRequest = serde_json::from_slice(&read).unwrap();
        assert_eq!(decoded, req);
    }

    #[test]
    fn roundtrip_response_and_event() {
        let mut buf = Vec::new();
        let resp = Response::ok(1, serde_json::json!({"status": "ready"}));
        write_response(&mut buf, resp).unwrap();
        write_event(&mut buf, "tick", serde_json::json!({"count": 1})).unwrap();

        let mut cursor = Cursor::new(buf);
        let f1 = read_frame(&mut cursor).unwrap().unwrap();
        let msg1: OutgoingMessage = serde_json::from_slice(&f1).unwrap();
        assert_eq!(
            msg1,
            OutgoingMessage::Response(Response::ok(1, serde_json::json!({"status": "ready"})))
        );

        let f2 = read_frame(&mut cursor).unwrap().unwrap();
        let msg2: OutgoingMessage = serde_json::from_slice(&f2).unwrap();
        assert_eq!(
            msg2,
            OutgoingMessage::Event(EventFrame {
                event: "tick".into(),
                data: serde_json::json!({"count": 1}),
            })
        );

        assert!(read_frame(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn frame_larger_than_max_fails() {
        let len = (MAX_FRAME_SIZE + 1) as u32;
        let mut buf = len.to_be_bytes().to_vec();
        buf.extend_from_slice(&[0u8; 10]);
        let mut cursor = Cursor::new(buf);
        let err = read_frame(&mut cursor).unwrap_err();
        assert!(matches!(err, ProtocolError::FrameTooLarge(_)));
    }
}
