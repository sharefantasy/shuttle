//! Shared LSP wire framing: the `Content-Length` header block + JSON body that
//! every LSP message rides on, regardless of direction.
//!
//! Both sides of this crate speak this framing:
//!   * the **server** ([`crate::server`]) reads client requests and writes
//!     responses over the inherited stdio pipes;
//!   * the **client** ([`crate::conversation`]) writes requests to a spawned
//!     language server and reads its responses back.
//!
//! Factoring it here means there is exactly ONE implementation of the framing
//! rules (header parsing, `Content-Length`, CRLF blank-line terminator), so the
//! two directions can never drift. It is pure transport: it operates only on
//! the [`BufRead`]/[`Write`] it is handed and never touches a socket or a port
//! (§2.2 daemonless — there is no network surface in this crate at all).

use std::io::{BufRead, Write};

use serde_json::Value;

/// Read one LSP-framed message (`Content-Length` header + JSON body) from a
/// buffered reader. Returns `Ok(None)` on clean EOF (the pipe closed — the peer
/// went away, so the caller's loop should end). This is transport framing only:
/// a body that is not valid JSON degrades to [`Value::Null`] rather than an
/// error, so a single malformed frame never aborts a whole conversation.
pub fn read_message<R: BufRead>(reader: &mut R) -> std::io::Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(None); // EOF — pipe closed.
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break; // blank line ends the header block.
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            content_length = rest.trim().parse::<usize>().ok();
        }
        // Other headers (Content-Type) are ignored.
    }
    let len = match content_length {
        Some(l) => l,
        None => return Ok(None), // malformed header block — treat as EOF.
    };
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    let body: Value = serde_json::from_slice(&buf).unwrap_or(Value::Null);
    Ok(Some(body))
}

/// Write one LSP-framed message to a writer: the `Content-Length` header, the
/// CRLF blank line, then the JSON body. Flushes so a spawned peer sees the
/// message immediately (otherwise a request could sit in the pipe buffer while
/// we block reading the response — a deadlock).
pub fn write_message<W: Write>(writer: &mut W, msg: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(msg)?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;

    #[test]
    fn round_trips_a_message() {
        let msg = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" });
        let mut sink: Vec<u8> = Vec::new();
        write_message(&mut sink, &msg).unwrap();

        let text = String::from_utf8(sink.clone()).unwrap();
        assert!(text.starts_with("Content-Length: "));
        assert!(text.contains("\r\n\r\n"));

        let mut reader = Cursor::new(sink);
        let got = read_message(&mut reader).unwrap().unwrap();
        assert_eq!(got, msg);
        // A second read hits EOF cleanly.
        assert!(read_message(&mut reader).unwrap().is_none());
    }

    #[test]
    fn eof_on_empty_reader() {
        let mut reader = Cursor::new(Vec::<u8>::new());
        assert!(read_message(&mut reader).unwrap().is_none());
    }

    #[test]
    fn ignores_content_type_header() {
        let body = b"{\"ok\":true}";
        let framed = format!(
            "Content-Length: {}\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n",
            body.len()
        );
        let mut bytes = framed.into_bytes();
        bytes.extend_from_slice(body);
        let mut reader = Cursor::new(bytes);
        let got = read_message(&mut reader).unwrap().unwrap();
        assert_eq!(got, json!({ "ok": true }));
    }
}
