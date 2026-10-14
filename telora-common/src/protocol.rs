//! Wire protocol of the daemon socket and the GUI control socket.
//!
//! One JSON object per line each way, then the server closes the
//! connection:
//!
//! ```text
//! → {"v":1,"cmd":"stop"}
//! ← {"ok":true,"text":"hola"}
//! ← {"ok":false,"error":{"code":"busy","message":"…"}}
//! ```
//!
//! Errors never travel inside a successful payload, so a transcription
//! that starts with "ERROR:" is still a transcription.

use std::fmt;
use std::path::Path;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

pub const VERSION: u64 = 1;

/// Longest request or reply, newline included.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadRequest,
    UnsupportedVersion,
    UnknownCommand,
    /// The daemon is recording or transcribing and cannot do this now.
    Busy,
    NoModel,
    Microphone,
    Transcription,
    Cancelled,
    /// Another client took over the request.
    Superseded,
    InvalidConfig,
    ModelLoad,
    Internal,
    /// A code this build does not know, from a newer peer.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
}

impl Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

/// The fields of a successful reply (without `ok`), or the error.
pub type Reply = Result<Map<String, Value>, Error>;

/// `STATUS` payload, under the `status` key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusResponse {
    pub pid: u32,
    pub model_id: String,
    pub model_kind: String,
    /// On-disk file or directory of the loaded engine; empty until loaded.
    pub model_path: String,
    pub language: String,
    pub max_recording_seconds: u32,
    /// `Idle`, `Recording` or `Processing`.
    pub state: String,
    /// `loading`, `ready` or `failed: <reason>`.
    pub engine: String,
}

pub fn encode_request(cmd: &str) -> Vec<u8> {
    line(&serde_json::json!({ "v": VERSION, "cmd": cmd }))
}

/// The command name of a request line.
///
/// # Errors
///
/// When the line is not a v1 request.
pub fn decode_request(message: &[u8]) -> Result<String, Error> {
    let bad = |m: &str| Error::new(ErrorCode::BadRequest, m);
    let value: Value = serde_json::from_slice(message)
        .map_err(|_| bad(r#"se esperaba una línea JSON como {"v":1,"cmd":"status"}"#))?;
    let Value::Object(request) = value else {
        return Err(bad("la petición debe ser un objeto JSON"));
    };
    match request.get("v").and_then(Value::as_u64) {
        Some(VERSION) => {}
        _ => {
            return Err(Error::new(
                ErrorCode::UnsupportedVersion,
                format!("versión de protocolo no soportada; se espera \"v\":{VERSION}"),
            ));
        }
    }
    match request.get("cmd").and_then(Value::as_str) {
        Some(cmd) => Ok(cmd.to_string()),
        None => Err(bad("falta \"cmd\"")),
    }
}

pub fn encode_reply(reply: &Reply) -> Vec<u8> {
    let value = match reply {
        Ok(fields) => {
            let mut object = Map::with_capacity(fields.len() + 1);
            object.insert("ok".into(), Value::Bool(true));
            object.extend(fields.clone());
            Value::Object(object)
        }
        Err(error) => serde_json::json!({ "ok": false, "error": error }),
    };
    line(&value)
}

/// # Errors
///
/// When the line is not a well-formed reply.
pub fn decode_reply(message: &[u8]) -> anyhow::Result<Reply> {
    let mut object: Map<String, Value> =
        serde_json::from_slice(message).context("respuesta que no es JSON")?;
    match object.remove("ok") {
        Some(Value::Bool(true)) => Ok(Ok(object)),
        Some(Value::Bool(false)) => {
            let error = object.remove("error").context("respuesta sin \"error\"")?;
            Ok(Err(
                serde_json::from_value(error).context("\"error\" mal formado")?
            ))
        }
        _ => bail!("respuesta sin \"ok\""),
    }
}

/// Read one message: up to a newline or the end of the stream, so
/// clients do not have to half-close.
///
/// # Errors
///
/// `BadRequest` when the message is empty or longer than
/// [`MAX_MESSAGE_BYTES`]; `Internal` on an I/O error.
pub async fn read_message<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Vec<u8>, Error> {
    let mut message = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = reader
            .read(&mut chunk)
            .await
            .map_err(|e| Error::new(ErrorCode::Internal, format!("error de lectura: {e}")))?;
        let data = &chunk[..n];
        let end = data.iter().position(|&b| b == b'\n');
        message.extend_from_slice(&data[..end.unwrap_or(n)]);
        if message.len() >= MAX_MESSAGE_BYTES {
            return Err(Error::new(
                ErrorCode::BadRequest,
                format!("mensaje de más de {} KiB", MAX_MESSAGE_BYTES / 1024),
            ));
        }
        if end.is_some() || n == 0 {
            break;
        }
    }
    if message.iter().all(u8::is_ascii_whitespace) {
        return Err(Error::new(ErrorCode::BadRequest, "mensaje vacío"));
    }
    Ok(message)
}

/// Send `cmd` to the server at `socket` and wait for its reply.
///
/// # Errors
///
/// The outer error is a transport failure (no server, broken or
/// malformed reply); the inner one is the server's answer.
pub async fn call(socket: &Path, cmd: &str) -> anyhow::Result<Reply> {
    let mut stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("no se pudo conectar con {}", socket.display()))?;
    stream
        .write_all(&encode_request(cmd))
        .await
        .context("no se pudo enviar la petición")?;
    let message = read_message(&mut stream)
        .await
        .map_err(|e| anyhow::anyhow!("respuesta inválida: {e}"))?;
    decode_reply(&message)
}

fn line(value: &Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(value).expect("a JSON value always serialises");
    bytes.push(b'\n');
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_text_that_looks_like_an_error_is_still_a_text() {
        let mut fields = Map::new();
        fields.insert("text".into(), "ERROR: el código falla".into());
        let decoded = decode_reply(&encode_reply(&Ok(fields))).unwrap().unwrap();
        assert_eq!(decoded["text"], "ERROR: el código falla");
    }

    #[test]
    fn errors_round_trip_with_their_code() {
        let error = Error::new(ErrorCode::Busy, "grabando");
        let decoded = decode_reply(&encode_reply(&Err(error.clone()))).unwrap();
        assert_eq!(decoded, Err(error));
    }

    #[test]
    fn an_error_code_from_a_newer_peer_is_kept_as_unknown() {
        let reply = br#"{"ok":false,"error":{"code":"quota","message":"x"}}"#;
        let error = decode_reply(reply).unwrap().unwrap_err();
        assert_eq!(error.code, ErrorCode::Unknown);
        assert_eq!(error.message, "x");
    }

    #[test]
    fn requests_round_trip() {
        let encoded = encode_request("stop");
        assert_eq!(encoded.last(), Some(&b'\n'));
        let message = &encoded[..encoded.len() - 1];
        assert_eq!(decode_request(message).unwrap(), "stop");
    }

    #[test]
    fn legacy_text_commands_are_rejected() {
        assert_eq!(
            decode_request(b"STOP").unwrap_err().code,
            ErrorCode::BadRequest
        );
    }

    #[test]
    fn other_versions_are_rejected() {
        for request in [&br#"{"v":2,"cmd":"stop"}"#[..], br#"{"cmd":"stop"}"#] {
            assert_eq!(
                decode_request(request).unwrap_err().code,
                ErrorCode::UnsupportedVersion
            );
        }
    }

    #[tokio::test]
    async fn a_message_ends_at_the_newline_without_waiting_for_eof() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        client.write_all(b"{\"v\":1}\n").await.unwrap();
        // `client` stays open: reading must not wait for EOF.
        let message =
            tokio::time::timeout(std::time::Duration::from_secs(1), read_message(&mut server))
                .await
                .expect("must not wait for the client to close")
                .unwrap();
        assert_eq!(message, b"{\"v\":1}");
    }

    #[tokio::test]
    async fn a_message_split_across_writes_is_reassembled() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        let reader = tokio::spawn(async move { read_message(&mut server).await });
        client.write_all(b"{\"v\":1,").await.unwrap();
        tokio::task::yield_now().await;
        client.write_all(b"\"cmd\":\"stop\"}\n").await.unwrap();
        let message = reader.await.unwrap().unwrap();
        assert_eq!(decode_request(&message).unwrap(), "stop");
    }

    #[tokio::test]
    async fn an_oversized_message_is_rejected() {
        let (mut client, mut server) = tokio::io::duplex(MAX_MESSAGE_BYTES * 2);
        client
            .write_all(&vec![b' '; MAX_MESSAGE_BYTES + 1])
            .await
            .unwrap();
        drop(client);
        let error = read_message(&mut server).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::BadRequest);
    }
}
