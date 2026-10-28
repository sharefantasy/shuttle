//! §9.9 tier-3 — the transport-generic LSP **client** conversation.
//!
//! Given an [`LspRequest`] and any [`BufRead`]+[`Write`] pair wired to a
//! language server, [`converse`] drives a full one-shot handshake —
//! `initialize -> initialized -> didOpen -> <op> -> shutdown -> exit` — and
//! maps the server's `Location`/`LocationLink` answer into [`shuttle_nav::NavHit`]s
//! tagged `Backend::Lsp` / `Confidence::High` (via [`crate::lsp_hit`]).
//!
//! # Why transport-generic
//! The conversation touches NO process and NO pipe of its own: it reads and
//! writes only the handles it is handed. That keeps it **pure** — it lives in
//! the DEFAULT (server-free) build and is driven in tests against an in-memory
//! scripted server (a `Cursor` of framed responses + a `Vec` write sink). The
//! feature-gated [`crate::real`] adapter is the only place that spawns a child
//! and hands its stdio pipes here; the moment the child drops it is killed
//! (yong-wan-ji-zou — never resident, never a port, §2.2).
//!
//! # §7 never-guess degradation
//!   * an **empty / null** result ⇒ zero hits. A symbol whose type the server
//!     cannot resolve yields nothing; we never invent a position.
//!   * a JSON-RPC **error** object ⇒ [`BridgeError::Query`] carrying the
//!     server's message — surfaced, not swallowed.
//!   * a location whose `uri` is not a `file://` under `root` is dropped rather
//!     than reported at a fabricated path.

use std::io::{BufRead, Write};

use serde_json::{json, Value};

use crate::framing::{read_message, write_message};
use crate::{lsp_hit, BridgeError, LspOp, LspRequest, LspServer};
use shuttle_nav::NavHit;

/// JSON-RPC id for the `initialize` request.
const ID_INITIALIZE: i64 = 1;
/// JSON-RPC id for the capability request (typeDefinition / prepareTypeHierarchy).
const ID_OP: i64 = 2;
/// JSON-RPC id for the shutdown request.
const ID_SHUTDOWN: i64 = 3;

/// The LSP `languageId` for a server's language, used in `didOpen`.
fn language_id(server: LspServer) -> &'static str {
    match server {
        LspServer::RustAnalyzer => "rust",
        LspServer::Tsserver => "typescript",
        LspServer::Pyright => "python",
    }
}

/// Build the target path for a request: `root/path` unless `path` is already
/// absolute. Kept string-only (no filesystem canonicalisation) so the
/// conversation stays pure and testable.
fn joined_path(root: &str, path: &str) -> String {
    if path.starts_with('/') || root.is_empty() || root == "." {
        path.to_string()
    } else {
        format!("{}/{}", root.trim_end_matches('/'), path)
    }
}

/// `file://` URI for a request's target document.
fn file_uri(root: &str, path: &str) -> String {
    let joined = joined_path(root, path);
    format!("file://{joined}")
}

/// Turn a returned `file://` URI back into a path relative to `root` (so a
/// [`NavHit`] reads the same as shuttle's static ladder). A URI outside `root` is
/// returned as its bare filesystem path; a non-`file://` URI yields `None` so
/// the caller can drop it rather than fabricate a location.
pub fn uri_to_rel_path(uri: &str, root: &str) -> Option<String> {
    let fs = uri.strip_prefix("file://")?;
    let root_norm = root.trim_end_matches('/');
    if !root_norm.is_empty() && root_norm != "." {
        if let Some(rest) = fs.strip_prefix(root_norm) {
            return Some(rest.trim_start_matches('/').to_string());
        }
    }
    Some(fs.to_string())
}

/// Build the `initialize` request. Advertises `positionEncoding: utf-16` (the
/// LSP default) and a minimal client capability set. `rootUri` is the repo.
pub fn build_initialize(id: i64, root: &str) -> Value {
    let root_uri = if root.is_empty() || root == "." {
        Value::Null
    } else {
        json!(format!("file://{root}"))
    };
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "processId": null,
            "rootUri": root_uri,
            "capabilities": {
                "general": { "positionEncodings": ["utf-16"] },
                "textDocument": {
                    "typeDefinition": { "linkSupport": true },
                    "typeHierarchy": { "dynamicRegistration": false }
                }
            },
            "clientInfo": { "name": "shuttle-lsp-bridge", "version": "tier-3" }
        }
    })
}

/// The `initialized` notification (no id).
pub fn build_initialized() -> Value {
    json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} })
}

/// `textDocument/didOpen` — feed the file text so a stateless server can answer
/// a position query without re-reading disk.
pub fn build_did_open(uri: &str, language_id: &str, text: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": {
            "textDocument": {
                "uri": uri,
                "languageId": language_id,
                "version": 1,
                "text": text
            }
        }
    })
}

/// The capability request for the op: `textDocument/typeDefinition` or
/// `textDocument/prepareTypeHierarchy`. `line`/`character` are 0-based (LSP).
pub fn build_op_request(id: i64, op: LspOp, uri: &str, line: u32, character: u32) -> Value {
    let method = match op {
        LspOp::TypeDefinition => "textDocument/typeDefinition",
        LspOp::TypeHierarchy => "textDocument/prepareTypeHierarchy",
    };
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": {
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }
    })
}

/// The shutdown request (server should stop accepting work).
pub fn build_shutdown(id: i64) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": "shutdown", "params": null })
}

/// The `exit` notification (server should terminate its own loop).
pub fn build_exit() -> Value {
    json!({ "jsonrpc": "2.0", "method": "exit" })
}

/// One resolved location extracted from an op result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawLocation {
    pub uri: String,
    /// 0-based LSP line.
    pub line: u32,
    /// 0-based LSP character.
    pub character: u32,
}

/// Parse a `typeDefinition` result into locations. LSP allows `Location`,
/// `Location[]`, `LocationLink[]`, or `null`. `prepareTypeHierarchy` returns
/// `TypeHierarchyItem[]` (each carries `uri` + `range`). We handle all forms,
/// preferring `targetSelectionRange`/`selectionRange` then `range`, and never
/// invent a position for a shape we do not recognise.
pub fn parse_locations(result: &Value) -> Vec<RawLocation> {
    fn from_obj(o: &Value) -> Option<RawLocation> {
        let uri = o
            .get("uri")
            .or_else(|| o.get("targetUri"))
            .and_then(Value::as_str)?;
        let range = o
            .get("targetSelectionRange")
            .or_else(|| o.get("selectionRange"))
            .or_else(|| o.get("targetRange"))
            .or_else(|| o.get("range"))?;
        let start = range.get("start")?;
        let line = start.get("line").and_then(Value::as_u64)? as u32;
        let character = start.get("character").and_then(Value::as_u64).unwrap_or(0) as u32;
        Some(RawLocation {
            uri: uri.to_string(),
            line,
            character,
        })
    }
    match result {
        Value::Null => Vec::new(),
        Value::Object(_) => from_obj(result).into_iter().collect(),
        Value::Array(items) => items.iter().filter_map(from_obj).collect(),
        _ => Vec::new(),
    }
}

/// Read framed messages until the response with `awaited_id` arrives, returning
/// its `result` (or an error). While waiting we honour the full protocol:
///   * a **notification** (method, no id) is ignored;
///   * a **server->client request** (method + id) is answered with a null result
///     so the server never blocks waiting on us (e.g. `window/workDoneProgress/
///     create`, `client/registerCapability`); we never guess a capability, a
///     null result is the honest "acknowledged, nothing to add";
///   * the awaited response with a matching id is returned;
///   * a response carrying an `error` becomes [`BridgeError::Query`];
///   * EOF before the response is a [`BridgeError::Query`] (the server died).
fn read_until_response<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    awaited_id: i64,
) -> Result<Value, BridgeError> {
    loop {
        let msg = match read_message(reader).map_err(|e| BridgeError::Query(e.to_string()))? {
            Some(m) => m,
            None => {
                return Err(BridgeError::Query(format!(
                    "server closed before responding to id {awaited_id}"
                )))
            }
        };
        let id = msg.get("id").and_then(Value::as_i64);
        let has_method = msg.get("method").and_then(Value::as_str).is_some();
        match (id, has_method) {
            (Some(server_id), true) => {
                let ack = json!({ "jsonrpc": "2.0", "id": server_id, "result": null });
                write_message(writer, &ack).map_err(|e| BridgeError::Query(e.to_string()))?;
            }
            (Some(resp_id), false) if resp_id == awaited_id => {
                if let Some(err) = msg.get("error") {
                    let m = err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown LSP error");
                    return Err(BridgeError::Query(m.to_string()));
                }
                return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
            }
            _ => {}
        }
    }
}

/// Drive the full tier-3 conversation over an already-connected server. `text`
/// is the target file's current content (fed via `didOpen`). Pure: the only
/// I/O is through `reader`/`writer`. Positions in `req` are 1-based line /
/// 0-based column (shuttle/user convention); we convert to LSP's 0-based line here
/// and back to 1-based in the resulting [`NavHit`]s.
pub fn converse<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    req: &LspRequest,
    text: &str,
) -> Result<Vec<NavHit>, BridgeError> {
    let uri = file_uri(&req.root, &req.path);
    let lang = language_id(req.server);
    let lsp_line = (req.line as u32).saturating_sub(1); // 1-based -> 0-based
    let lsp_char = req.col as u32;

    write_message(writer, &build_initialize(ID_INITIALIZE, &req.root))
        .map_err(|e| BridgeError::Query(e.to_string()))?;
    let _init = read_until_response(reader, writer, ID_INITIALIZE)?;

    write_message(writer, &build_initialized()).map_err(|e| BridgeError::Query(e.to_string()))?;
    write_message(writer, &build_did_open(&uri, lang, text))
        .map_err(|e| BridgeError::Query(e.to_string()))?;

    write_message(
        writer,
        &build_op_request(ID_OP, req.op, &uri, lsp_line, lsp_char),
    )
    .map_err(|e| BridgeError::Query(e.to_string()))?;
    let result = read_until_response(reader, writer, ID_OP)?;

    write_message(writer, &build_shutdown(ID_SHUTDOWN))
        .map_err(|e| BridgeError::Query(e.to_string()))?;
    let _ = read_until_response(reader, writer, ID_SHUTDOWN);
    let _ = write_message(writer, &build_exit());

    let locations = parse_locations(&result);
    let mut hits: Vec<NavHit> = Vec::new();
    for loc in locations {
        let Some(rel) = uri_to_rel_path(&loc.uri, &req.root) else {
            continue; // non-file:// uri — never fabricate a path.
        };
        let nav_line = (loc.line as usize) + 1; // LSP 0-based -> NavHit 1-based
        let mut hit = lsp_hit(req, nav_line, format!("{} at {}", req.symbol, rel));
        hit.path = rel;
        hit.col = Some(loc.character as usize);
        hits.push(hit);
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shuttle_nav::{Backend, Confidence};
    use std::io::Cursor;

    fn frame(v: &Value) -> Vec<u8> {
        let body = serde_json::to_vec(v).unwrap();
        let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        out.extend_from_slice(&body);
        out
    }

    fn req(op: LspOp) -> LspRequest {
        LspRequest {
            server: LspServer::RustAnalyzer,
            root: "/repo".into(),
            path: "src/lib.rs".into(),
            line: 10,
            col: 4,
            symbol: "Widget".into(),
            op,
        }
    }

    fn scripted(op_result: Value) -> Cursor<Vec<u8>> {
        let mut buf = Vec::new();
        buf.extend(frame(
            &json!({ "jsonrpc": "2.0", "id": ID_INITIALIZE, "result": { "capabilities": {} } }),
        ));
        buf.extend(frame(
            &json!({ "jsonrpc": "2.0", "id": ID_OP, "result": op_result }),
        ));
        buf.extend(frame(
            &json!({ "jsonrpc": "2.0", "id": ID_SHUTDOWN, "result": null }),
        ));
        Cursor::new(buf)
    }

    fn write_requests(sink: &[u8]) -> Vec<Value> {
        let mut reader = Cursor::new(sink.to_vec());
        let mut out = Vec::new();
        while let Some(m) = read_message(&mut reader).unwrap() {
            out.push(m);
        }
        out
    }

    #[test]
    fn single_location_maps_to_one_navhit() {
        let result = json!({
            "uri": "file:///repo/src/widget.rs",
            "range": { "start": { "line": 41, "character": 8 }, "end": { "line": 41, "character": 20 } }
        });
        let mut reader = scripted(result);
        let mut sink: Vec<u8> = Vec::new();
        let hits = converse(&mut reader, &mut sink, &req(LspOp::TypeDefinition), "text").unwrap();
        assert_eq!(hits.len(), 1);
        let h = &hits[0];
        assert_eq!(h.path, "src/widget.rs");
        assert_eq!(h.line, 42);
        assert_eq!(h.col, Some(8));
        assert_eq!(h.confidence, Confidence::High);
        assert_eq!(h.source, Backend::Lsp);
        assert!(h.snippet.as_ref().unwrap().contains("via external LSP"));
    }

    #[test]
    fn location_link_form_is_supported() {
        let result = json!([{
            "targetUri": "file:///repo/src/widget.rs",
            "targetRange": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } },
            "targetSelectionRange": { "start": { "line": 5, "character": 3 }, "end": { "line": 5, "character": 9 } }
        }]);
        let mut reader = scripted(result);
        let mut sink: Vec<u8> = Vec::new();
        let hits = converse(&mut reader, &mut sink, &req(LspOp::TypeDefinition), "t").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line, 6);
        assert_eq!(hits[0].col, Some(3));
    }

    #[test]
    fn empty_result_yields_zero_hits_never_fabricates() {
        let mut reader = scripted(Value::Null);
        let mut sink: Vec<u8> = Vec::new();
        let hits = converse(&mut reader, &mut sink, &req(LspOp::TypeDefinition), "t").unwrap();
        assert!(hits.is_empty());

        let mut reader = scripted(json!([]));
        let mut sink: Vec<u8> = Vec::new();
        let hits = converse(&mut reader, &mut sink, &req(LspOp::TypeDefinition), "t").unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn jsonrpc_error_becomes_query_error() {
        let mut buf = Vec::new();
        buf.extend(frame(
            &json!({ "jsonrpc": "2.0", "id": ID_INITIALIZE, "result": {} }),
        ));
        buf.extend(frame(&json!({
            "jsonrpc": "2.0", "id": ID_OP,
            "error": { "code": -32601, "message": "typeDefinition not supported" }
        })));
        let mut reader = Cursor::new(buf);
        let mut sink: Vec<u8> = Vec::new();
        let err = converse(&mut reader, &mut sink, &req(LspOp::TypeDefinition), "t").unwrap_err();
        match err {
            BridgeError::Query(m) => assert!(m.contains("typeDefinition not supported")),
            other => panic!("expected Query, got {other:?}"),
        }
    }

    #[test]
    fn emits_well_formed_correlated_request_sequence() {
        let result = json!({
            "uri": "file:///repo/src/widget.rs",
            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } }
        });
        let mut reader = scripted(result);
        let mut sink: Vec<u8> = Vec::new();
        let _ = converse(&mut reader, &mut sink, &req(LspOp::TypeDefinition), "body").unwrap();

        let msgs = write_requests(&sink);
        let methods: Vec<&str> = msgs
            .iter()
            .map(|m| m.get("method").and_then(Value::as_str).unwrap_or(""))
            .collect();
        assert_eq!(
            methods,
            vec![
                "initialize",
                "initialized",
                "textDocument/didOpen",
                "textDocument/typeDefinition",
                "shutdown",
                "exit",
            ]
        );
        assert_eq!(
            msgs[0].get("id").and_then(Value::as_i64),
            Some(ID_INITIALIZE)
        );
        assert_eq!(msgs[1].get("id"), None);
        assert_eq!(msgs[2].get("id"), None);
        assert_eq!(msgs[3].get("id").and_then(Value::as_i64), Some(ID_OP));
        assert_eq!(msgs[4].get("id").and_then(Value::as_i64), Some(ID_SHUTDOWN));
        assert_eq!(msgs[5].get("id"), None);
        assert_eq!(msgs[2]["params"]["textDocument"]["text"], json!("body"));
        assert_eq!(
            msgs[3]["params"]["position"],
            json!({ "line": 9, "character": 4 })
        );
    }

    #[test]
    fn type_hierarchy_op_uses_prepare_method_and_maps_items() {
        let items = json!([{
            "name": "Widget",
            "kind": 5,
            "uri": "file:///repo/src/base.rs",
            "range": { "start": { "line": 2, "character": 0 }, "end": { "line": 9, "character": 1 } },
            "selectionRange": { "start": { "line": 2, "character": 7 }, "end": { "line": 2, "character": 13 } }
        }]);
        let mut reader = scripted(items);
        let mut sink: Vec<u8> = Vec::new();
        let hits = converse(&mut reader, &mut sink, &req(LspOp::TypeHierarchy), "t").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "src/base.rs");
        assert_eq!(hits[0].line, 3);

        let msgs = write_requests(&sink);
        assert_eq!(
            msgs[3].get("method").and_then(Value::as_str),
            Some("textDocument/prepareTypeHierarchy")
        );
    }

    #[test]
    fn server_request_during_wait_is_acknowledged_then_skipped() {
        let mut buf = Vec::new();
        buf.extend(frame(
            &json!({ "jsonrpc": "2.0", "id": ID_INITIALIZE, "result": {} }),
        ));
        buf.extend(frame(&json!({
            "jsonrpc": "2.0", "id": 99, "method": "window/workDoneProgress/create",
            "params": { "token": "t" }
        })));
        buf.extend(frame(&json!({
            "jsonrpc": "2.0", "id": ID_OP,
            "result": { "uri": "file:///repo/x.rs", "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } } }
        })));
        buf.extend(frame(
            &json!({ "jsonrpc": "2.0", "id": ID_SHUTDOWN, "result": null }),
        ));
        let mut reader = Cursor::new(buf);
        let mut sink: Vec<u8> = Vec::new();
        let hits = converse(&mut reader, &mut sink, &req(LspOp::TypeDefinition), "t").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "x.rs");

        let msgs = write_requests(&sink);
        assert!(msgs
            .iter()
            .any(|m| m.get("id").and_then(Value::as_i64) == Some(99) && m.get("result").is_some()));
    }

    #[test]
    fn eof_before_op_response_is_query_error() {
        let mut buf = Vec::new();
        buf.extend(frame(
            &json!({ "jsonrpc": "2.0", "id": ID_INITIALIZE, "result": {} }),
        ));
        let mut reader = Cursor::new(buf);
        let mut sink: Vec<u8> = Vec::new();
        let err = converse(&mut reader, &mut sink, &req(LspOp::TypeDefinition), "t").unwrap_err();
        assert!(matches!(err, BridgeError::Query(_)));
    }

    #[test]
    fn non_file_uri_is_dropped_not_fabricated() {
        let result = json!({
            "uri": "jdt://contents/Foo.class",
            "range": { "start": { "line": 3, "character": 0 }, "end": { "line": 3, "character": 1 } }
        });
        let mut reader = scripted(result);
        let mut sink: Vec<u8> = Vec::new();
        let hits = converse(&mut reader, &mut sink, &req(LspOp::TypeDefinition), "t").unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn uri_to_rel_path_strips_root_and_rejects_non_file() {
        assert_eq!(
            uri_to_rel_path("file:///repo/src/a.rs", "/repo"),
            Some("src/a.rs".to_string())
        );
        assert_eq!(
            uri_to_rel_path("file:///other/b.rs", "/repo"),
            Some("/other/b.rs".to_string())
        );
        assert_eq!(uri_to_rel_path("http://x/y", "/repo"), None);
    }
}
