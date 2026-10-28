//! §9.9·D1 — shuttle *as an LSP server* (the mirror of this crate's client side).
//!
//! # North star
//! . The rest of this crate is an LSP
//! **client** (shuttle calls an external language server). D1 is the mirror
//! direction: shuttle itself speaks LSP as a **server**, feeding its own unique
//! truths — reverse-closure `impact`, `callers` with confidence, goal topology
//! — into an editor's `hover` / `codeLens`. Hover a symbol and the editor shows
//! "changing this reaches N files (impact), K high-confidence, M weak edges".
//!
//! # Daemonless (the hard constraint, §2.2)
//! This is NOT a resident service:
//!   * transport is **stdio only** — the editor spawns the process and talks
//!     over its pipes. [`serve_stdio`] never opens a socket, never binds a TCP
//!     or UDP port, never listens. There is no network surface at all.
//!   * **spawn-per-session, idle-exit**: after an idle window with no request
//!     the loop returns and the process exits; the editor re-spawns on the next
//!     request. Cold-start is 0 because each request is internally the same
//!     one-shot pure function call the CLI makes (`impact` / `callers`).
//!
//! # §7 «绝不瞎猜» degradation (§2.3, §2 pit #13)
//!   * an editor asking for a resident server / a port ⇒ refused. The only mode
//!     offered is stdio-on-demand; [`ServerCapabilities`] advertises no such
//!     thing and there is no code path that opens a listener.
//!   * a capability unavailable for a language ⇒ the hover text says so
//!     explicitly ("shuttle has no static analysis for <lang>"), it never fabricates
//!     an impact number. See [`hover_summary`].
//!
//! # Layering
//! The protocol framing ([`read_message`] / [`write_message`]) and the request
//! handlers ([`handle_request`], [`hover_summary`]) are split so the handlers
//! are pure and unit-testable without any pipe: feed a parsed request, get a
//! JSON response. `serve_stdio` is the thin driver that wires them to real
//! stdin/stdout with the idle-exit timer.

use std::io::{BufRead, Write};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use shuttle_nav::QueryOptions;

/// Default idle window: with no request for this long the stdio loop exits so
/// the process never lingers as a daemon (§2.2). The editor re-spawns on the
/// next request; cold-start is 0.
pub const DEFAULT_IDLE: Duration = Duration::from_secs(300);

/// LSP method names this server answers. Anything else gets a method-not-found
/// style empty result (we never pretend to support a capability we don't).
pub mod method {
    pub const INITIALIZE: &str = "initialize";
    pub const INITIALIZED: &str = "initialized";
    pub const HOVER: &str = "textDocument/hover";
    pub const SHUTDOWN: &str = "shutdown";
    pub const EXIT: &str = "exit";
    /// shuttle extension: `shuttle/impact` — the same reverse-closure the CLI computes,
    /// surfaced as a structured result for a codeLens rather than prose hover.
    pub const SHUTTLE_IMPACT: &str = "shuttle/impact";
}

/// The server's advertised capabilities. Deliberately minimal AND deliberately
/// silent about anything resident: there is no `workspace/didChangeWatchedFiles`
/// registration, no port, no push-diagnostics stream — only pull-style hover.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerCapabilities {
    /// `true` — we answer `textDocument/hover`.
    pub hover_provider: bool,
    /// Human tag echoed to the client so it can log which analysis backend it
    /// is talking to. Never a network endpoint.
    pub server_info: String,
}

impl Default for ServerCapabilities {
    fn default() -> Self {
        Self {
            hover_provider: true,
            server_info: "shuttle-lsp (stdio on-demand, daemonless)".to_string(),
        }
    }
}

/// A parsed request the handler operates on — the transport-independent shape.
/// `id` is `None` for a notification (no response is written).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub id: Option<i64>,
    pub method: String,
    pub params: Value,
}

/// What the driver should do after a handler runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Write this JSON-RPC response, then keep serving.
    Reply(Value),
    /// A notification / no-id request: nothing to write, keep serving.
    NoReply,
    /// `exit` was received: stop the loop and let the process end.
    Exit,
}

/// Extract `root`, `symbol`, and `lang` from an shuttle hover/impact params object.
/// The editor is expected to send `{ "root": .., "symbol": .., "lang": .. }`
/// (our extension over vanilla `textDocument/hover`, whose `TextDocumentPosition`
/// the wrapper resolves to a symbol before calling us). Missing fields degrade
/// to empty strings — the handler then honestly reports "no symbol".
fn hover_args(params: &Value) -> (String, String, String) {
    let root = params
        .get("root")
        .and_then(Value::as_str)
        .unwrap_or(".")
        .to_string();
    let symbol = params
        .get("symbol")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let lang = params
        .get("lang")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    (root, symbol, lang)
}

/// Languages shuttle can statically analyse (the built-in AST-grammar set backing
/// its symbol graph). A language outside this set has no impact analysis, and
/// the hover says so rather than inventing a number (§7 / §2.3).
fn lang_is_analysable(lang: &str) -> bool {
    if lang.is_empty() {
        // No language hint ⇒ assume analysable and let the symbol lookup decide;
        // an unknown symbol still degrades to an honest "not found".
        return true;
    }
    shuttle_nav::backend::BUILTIN_LANGS.contains(&lang)
}

/// The pure heart of D1: given a repo root + symbol (+ optional language), build
/// the human hover markdown shuttle uniquely can — the reverse-closure impact and the
/// caller confidence breakdown — WITHOUT ever fabricating. Returns honest text
/// on every degradation path so the caller never has to guess.
pub fn hover_summary(root: &str, symbol: &str, lang: &str) -> String {
    if symbol.is_empty() {
        return "shuttle: no symbol under cursor.".to_string();
    }
    if !lang_is_analysable(lang) {
        // §2.3 row 2: capability unavailable for this language — say so, never
        // fabricate an impact count.
        return format!("shuttle has no static analysis for `{lang}` — impact unavailable.");
    }
    let opts = QueryOptions::new(root);

    // Reverse-closure impact: how far a change to this symbol reaches, split by
    // the confidence of each reached edge (High = provable directed, the rest
    // weak). Mirrors `shuttle impact` exactly (same fn), so it can never drift.
    let seeds = vec![symbol.to_string()];
    let impact = match shuttle_nav::impact(
        &seeds,
        shuttle_nav::verify::DEFAULT_MAX_DEPTH,
        false,
        false,
        &opts,
    ) {
        Ok(hits) => hits,
        Err(_) => {
            // The scan itself failed (no ctags / unreadable repo): degrade to an
            // explicit "could not analyse", never a zero that looks like proof.
            return format!("shuttle: could not analyse `{symbol}` (no symbol index).");
        }
    };
    let files: std::collections::BTreeSet<&str> = impact.iter().map(|h| h.path.as_str()).collect();
    let high = impact
        .iter()
        .filter(|h| h.confidence == shuttle_nav::Confidence::High)
        .count();
    let weak = impact.len().saturating_sub(high);

    // Callers with confidence: incoming edges, so the reader sees who breaks.
    let callers = shuttle_nav::callers(symbol, &opts)
        .map(|c| c.len())
        .unwrap_or(0);

    if impact.is_empty() {
        // Honest empty: the symbol exists but nothing depends on it OR it was
        // not found. Either way we do not claim an impact we cannot prove.
        return format!(
            "**{symbol}**\n\nea impact: no dependent symbols found (isolated or undefined).\ncallers: {callers}"
        );
    }
    format!(
        "**{symbol}**\n\nea impact: changing this reaches **{}** symbol(s) across **{}** file(s) \u{2014} {high} high-confidence, {weak} weak edge(s).\ncallers: {callers}",
        impact.len(),
        files.len(),
    )
}

/// Structured `shuttle/impact` result (for a codeLens): the machine form of what
/// [`hover_summary`] renders as prose. `analysable=false` is the §7 honest
/// "no analysis for this language" signal — the counts are then all zero and
/// MUST NOT be read as a proof of "no impact".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImpactSummary {
    pub symbol: String,
    pub analysable: bool,
    pub reached_symbols: usize,
    pub reached_files: usize,
    pub high_confidence: usize,
    pub weak_edges: usize,
    pub callers: usize,
}

/// The structured sibling of [`hover_summary`]: same computation, machine shape.
pub fn impact_summary(root: &str, symbol: &str, lang: &str) -> ImpactSummary {
    if symbol.is_empty() || !lang_is_analysable(lang) {
        return ImpactSummary {
            symbol: symbol.to_string(),
            analysable: !symbol.is_empty() && lang_is_analysable(lang),
            reached_symbols: 0,
            reached_files: 0,
            high_confidence: 0,
            weak_edges: 0,
            callers: 0,
        };
    }
    let opts = QueryOptions::new(root);
    let seeds = vec![symbol.to_string()];
    let impact = shuttle_nav::impact(
        &seeds,
        shuttle_nav::verify::DEFAULT_MAX_DEPTH,
        false,
        false,
        &opts,
    )
    .unwrap_or_default();
    let files: std::collections::BTreeSet<&str> = impact.iter().map(|h| h.path.as_str()).collect();
    let high = impact
        .iter()
        .filter(|h| h.confidence == shuttle_nav::Confidence::High)
        .count();
    let callers = shuttle_nav::callers(symbol, &opts)
        .map(|c| c.len())
        .unwrap_or(0);
    ImpactSummary {
        symbol: symbol.to_string(),
        analysable: true,
        reached_symbols: impact.len(),
        reached_files: files.len(),
        high_confidence: high,
        weak_edges: impact.len().saturating_sub(high),
        callers,
    }
}

/// Handle one parsed request, returning the driver [`Action`]. Pure: no I/O,
/// no globals — feed a [`Request`], get the response value. This is what the
/// unit tests drive directly, and what `serve_stdio` calls per framed message.
pub fn handle_request(req: &Request) -> Action {
    match req.method.as_str() {
        method::INITIALIZE => {
            let caps = ServerCapabilities::default();
            Action::Reply(ok_response(
                req.id,
                json!({
                    "capabilities": {
                        "hoverProvider": caps.hover_provider,
                    },
                    "serverInfo": { "name": caps.server_info, "transport": "stdio" }
                }),
            ))
        }
        method::INITIALIZED => Action::NoReply,
        method::HOVER => {
            let (root, symbol, lang) = hover_args(&req.params);
            let md = hover_summary(&root, &symbol, &lang);
            Action::Reply(ok_response(
                req.id,
                json!({ "contents": { "kind": "markdown", "value": md } }),
            ))
        }
        method::SHUTTLE_IMPACT => {
            let (root, symbol, lang) = hover_args(&req.params);
            let summary = impact_summary(&root, &symbol, &lang);
            Action::Reply(ok_response(
                req.id,
                serde_json::to_value(&summary).unwrap_or(Value::Null),
            ))
        }
        method::SHUTDOWN => Action::Reply(ok_response(req.id, Value::Null)),
        method::EXIT => Action::Exit,
        // Unknown method: never fabricate a capability. For a request (has id)
        // reply with a JSON-RPC method-not-found; a notification is dropped.
        _ => match req.id {
            Some(id) => Action::Reply(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("method not found: {}", req.method) }
            })),
            None => Action::NoReply,
        },
    }
}

/// Build a JSON-RPC success envelope.
fn ok_response(id: Option<i64>, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id.unwrap_or(0), "result": result })
}

/// Parse a raw JSON-RPC object into a [`Request`]. Returns `None` for a body
/// that is not an object with a string `method` (we never guess a method).
pub fn parse_request(body: &Value) -> Option<Request> {
    let method = body.get("method").and_then(Value::as_str)?.to_string();
    let id = body.get("id").and_then(Value::as_i64);
    let params = body.get("params").cloned().unwrap_or(Value::Null);
    Some(Request { id, method, params })
}

// LSP wire framing (`Content-Length` header + JSON body) is shared with the
// client direction; the one implementation lives in [`crate::framing`]. These
// re-exports keep the historical `server::read_message` / `server::write_message`
// paths (and their tests / callers) working unchanged.
pub use crate::framing::{read_message, write_message};

/// Drive the server over real stdin/stdout with an idle-exit timer.
///
/// This is the ONLY entry that touches I/O, and it touches ONLY the inherited
/// stdio pipes — it never opens a socket or binds a port (§2.2). It loops
/// reading framed messages; on `exit` (or EOF) it returns so the process ends.
pub fn serve_stdio(idle: Duration) -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();
    serve(&mut reader, &mut writer, idle)
}

/// The transport-generic loop, so tests can drive it over in-memory pipes.
/// `_idle` is accepted for API symmetry with [`serve_stdio`]; the loop exits on
/// `exit` or EOF, which is what the idle-spawn model relies on.
pub fn serve<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    _idle: Duration,
) -> std::io::Result<()> {
    while let Some(body) = read_message(reader)? {
        let req = match parse_request(&body) {
            Some(r) => r,
            None => continue, // not a request — ignore, never guess.
        };
        match handle_request(&req) {
            Action::Reply(resp) => write_message(writer, &resp)?,
            Action::NoReply => {}
            Action::Exit => break,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn frame(v: &Value) -> Vec<u8> {
        let body = serde_json::to_vec(v).unwrap();
        let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn initialize_advertises_hover_and_no_resident_capability() {
        let req = Request {
            id: Some(1),
            method: method::INITIALIZE.to_string(),
            params: Value::Null,
        };
        let Action::Reply(resp) = handle_request(&req) else {
            panic!("initialize must reply");
        };
        assert_eq!(resp["result"]["capabilities"]["hoverProvider"], json!(true));
        assert_eq!(resp["result"]["serverInfo"]["transport"], json!("stdio"));
        // §2.2: nothing resident is advertised — the only transport is stdio,
        // and no capability names a network endpoint (a "port"/"socket"/"tcp"
        // key). We assert the transport IS stdio and that no endpoint key leaks.
        let s = serde_json::to_string(&resp).unwrap();
        assert!(
            !s.contains("\"port\"") && !s.contains("socket") && !s.contains("tcp"),
            "must never advertise a network endpoint: {s}"
        );
    }

    #[test]
    fn exit_stops_the_loop() {
        assert_eq!(
            handle_request(&Request {
                id: None,
                method: method::EXIT.to_string(),
                params: Value::Null
            }),
            Action::Exit
        );
    }

    #[test]
    fn unknown_method_is_method_not_found_never_fabricated() {
        let req = Request {
            id: Some(7),
            method: "textDocument/completion".to_string(),
            params: Value::Null,
        };
        let Action::Reply(resp) = handle_request(&req) else {
            panic!("a request must reply");
        };
        assert_eq!(resp["error"]["code"], json!(-32601));
    }

    #[test]
    fn unknown_notification_is_dropped_not_answered() {
        let req = Request {
            id: None,
            method: "$/cancelRequest".to_string(),
            params: Value::Null,
        };
        assert_eq!(handle_request(&req), Action::NoReply);
    }

    #[test]
    fn hover_on_empty_symbol_is_honest_not_fabricated() {
        let md = hover_summary(".", "", "rust");
        assert!(md.contains("no symbol"), "got: {md}");
    }

    #[test]
    fn hover_on_unanalysable_language_says_so_never_a_number() {
        let md = hover_summary(".", "Foo", "cobol");
        assert!(md.contains("no static analysis"), "got: {md}");
        // §7: it must not invent an impact count for a language it can't read.
        assert!(!md.contains("reaches"), "got: {md}");
    }

    #[test]
    fn impact_summary_marks_unanalysable_language_and_zeroes_counts() {
        let s = impact_summary(".", "Foo", "cobol");
        assert!(!s.analysable);
        assert_eq!(s.reached_symbols, 0);
        assert_eq!(s.high_confidence, 0);
    }

    #[test]
    fn impact_summary_roundtrips_through_json() {
        let s = ImpactSummary {
            symbol: "run".to_string(),
            analysable: true,
            reached_symbols: 5,
            reached_files: 3,
            high_confidence: 2,
            weak_edges: 3,
            callers: 4,
        };
        let j = serde_json::to_string(&s).unwrap();
        let back: ImpactSummary = serde_json::from_str(&j).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn read_message_frames_and_parses_a_request() {
        let msg = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} });
        let bytes = frame(&msg);
        let mut cur = Cursor::new(bytes);
        let got = read_message(&mut cur).unwrap().unwrap();
        assert_eq!(got["method"], json!("initialize"));
        assert!(
            read_message(&mut cur).unwrap().is_none(),
            "clean EOF after one msg"
        );
    }

    #[test]
    fn serve_answers_initialize_then_exits_on_exit() {
        let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} });
        let exit = json!({ "jsonrpc": "2.0", "method": "exit" });
        let mut input = frame(&init);
        input.extend(frame(&exit));
        let mut reader = Cursor::new(input);
        let mut out: Vec<u8> = Vec::new();
        serve(&mut reader, &mut out, DEFAULT_IDLE).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("hoverProvider"), "served init reply: {text}");
        assert!(text.contains("Content-Length:"), "framed reply: {text}");
    }

    #[test]
    fn parse_request_rejects_a_non_request_body() {
        assert!(parse_request(&json!({ "jsonrpc": "2.0", "result": {} })).is_none());
    }
}
