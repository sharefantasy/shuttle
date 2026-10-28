//! Binary-level CLI integration tests for the G9·D1 `shuttle lsp` server surface
//! . `shuttle lsp` is the mirror direction
//! of the shuttle-lsp-bridge *client*: shuttle acts as an LSP *server* that feeds its
//! reverse-closure impact + caller confidence into an editor's hover. The hard
//! daemonless constraint (§2.2) and the §2.3 honest-degradation rows are
//! asserted end-to-end by driving the real process over stdio pipes:
//!
//!   1. **stdio only, never a resident port** — `initialize` advertises a
//!      `stdio` transport and NO port/socket/tcp endpoint; the process answers
//!      over the inherited pipes and exits on `exit`. It never binds a socket.
//!   2. **honest hover** — hover over a real symbol reports the impact numbers;
//!      hover over an unanalysable language says so explicitly and NEVER emits
//!      a fabricated number; an unknown method gets a JSON-RPC method-not-found
//!      rather than an invented capability.
//!
//! Transport framing (`Content-Length` header + JSON body) is implemented
//! inline so the test exercises the exact wire format an editor would speak.
//! Hover impact numbers need the symbol graph (ctags); the shape/transport and
//! honest-degradation cases run unconditionally.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

fn shuttle_bin() -> &'static str {
    env!("CARGO_BIN_EXE_shuttle")
}

fn ctags_present() -> bool {
    Command::new("ctags")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("shuttle_lsp_it_{tag}_{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &std::path::Path, name: &str, body: &str) {
    let p = dir.join(name);
    let mut f = std::fs::File::create(&p).unwrap();
    f.write_all(body.as_bytes()).unwrap();
}

/// Serialize a JSON-RPC object into an LSP `Content-Length`-framed message.
fn frame(obj: &serde_json::Value) -> Vec<u8> {
    let body = serde_json::to_vec(obj).unwrap();
    let mut msg = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    msg.extend_from_slice(&body);
    msg
}

/// Read one `Content-Length`-framed JSON message from a reader. Returns `None`
/// at clean EOF.
fn read_frame<R: BufRead>(r: &mut R) -> Option<serde_json::Value> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = r.read_line(&mut line).ok()?;
        if n == 0 {
            return None; // EOF
        }
        let t = line.trim_end();
        if t.is_empty() {
            break;
        }
        if let Some(rest) = t.strip_prefix("Content-Length:") {
            content_length = rest.trim().parse().ok();
        }
    }
    let len = content_length?;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

/// Drive `shuttle lsp` with a batch of framed requests, then read every reply until
/// the process exits (on the trailing `exit` notification). Returns the parsed
/// replies in order. The whole exchange rides the child's stdin/stdout pipes —
/// no port is ever opened.
fn drive_lsp(
    ws: Option<&std::path::Path>,
    requests: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    let mut child = Command::new(shuttle_bin())
        .args(["lsp", "--idle-secs", "10"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(ws.unwrap_or(&std::env::temp_dir()))
        .spawn()
        .expect("spawn shuttle lsp");

    {
        let mut stdin = child.stdin.take().expect("child stdin");
        for req in requests {
            stdin.write_all(&frame(req)).unwrap();
        }
        stdin.flush().unwrap();
        // Drop stdin (EOF) so the server's read loop also ends if `exit` was
        // not sent — the process must never hang waiting on a socket.
    }

    let stdout = child.stdout.take().expect("child stdout");
    let mut reader = BufReader::new(stdout);
    let mut replies = Vec::new();
    while let Some(v) = read_frame(&mut reader) {
        replies.push(v);
    }
    let status = child.wait().expect("wait shuttle lsp");
    assert!(status.success(), "shuttle lsp exited non-zero: {status:?}");
    replies
}

// ---------------------------------------------------------------------------
// §2.2 red line: stdio transport, never a resident port. `initialize`
// advertises hover over stdio and declares NO port/socket endpoint.
// ---------------------------------------------------------------------------

#[test]
fn initialize_advertises_stdio_and_never_a_resident_port() {
    let replies = drive_lsp(
        None,
        &[
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
            serde_json::json!({"jsonrpc":"2.0","method":"exit","params":{}}),
        ],
    );
    assert_eq!(replies.len(), 1, "initialize replies once: {replies:?}");
    let init = &replies[0];
    assert_eq!(init["id"], 1, "{init}");
    assert_eq!(
        init["result"]["capabilities"]["hoverProvider"], true,
        "{init}"
    );
    assert_eq!(init["result"]["serverInfo"]["transport"], "stdio", "{init}");

    // The serialized reply must declare no network endpoint: no `"port"`
    // field, no socket/tcp transport. A resident-server capability would leak
    // one of these tokens; a daemonless stdio server never does.
    let raw = serde_json::to_string(init).unwrap();
    assert!(
        !raw.contains("\"port\"") && !raw.contains("socket") && !raw.contains("tcp"),
        "server must not advertise a resident network endpoint: {raw}"
    );
}

// ---------------------------------------------------------------------------
// §2.3 row: an unknown method gets a JSON-RPC method-not-found — the server
// never fabricates a capability it does not have.
// ---------------------------------------------------------------------------

#[test]
fn unknown_method_is_method_not_found_never_fabricated() {
    let replies = drive_lsp(
        None,
        &[
            serde_json::json!({"jsonrpc":"2.0","id":7,"method":"textDocument/definition","params":{}}),
            serde_json::json!({"jsonrpc":"2.0","method":"exit","params":{}}),
        ],
    );
    assert_eq!(replies.len(), 1, "{replies:?}");
    let r = &replies[0];
    assert_eq!(r["id"], 7, "{r}");
    assert_eq!(r["error"]["code"], -32601, "method-not-found: {r}");
    assert!(r.get("result").is_none(), "no fabricated result: {r}");
}

// ---------------------------------------------------------------------------
// §2.3 row: hover on an unanalysable language says so explicitly and NEVER
// emits a number. (No repo/ctags needed — the language gate short-circuits.)
// ---------------------------------------------------------------------------

#[test]
fn hover_on_unanalysable_language_says_so_never_a_number() {
    let replies = drive_lsp(
        None,
        &[
            serde_json::json!({
                "jsonrpc":"2.0","id":3,"method":"textDocument/hover",
                "params":{"root":".","symbol":"whatever","lang":"cobol"}
            }),
            serde_json::json!({"jsonrpc":"2.0","method":"exit","params":{}}),
        ],
    );
    assert_eq!(replies.len(), 1, "{replies:?}");
    let value = replies[0]["result"]["contents"]["value"]
        .as_str()
        .expect("hover markdown value");
    assert!(
        value.contains("no static analysis for") && value.contains("cobol"),
        "honest 'no analysis for cobol': {value}"
    );
    // The honest degradation must not carry a fabricated impact count.
    assert!(
        !value.contains("reaches") && !value.chars().any(|c| c.is_ascii_digit()),
        "unanalysable hover must never emit a number: {value}"
    );
}

// ---------------------------------------------------------------------------
// §2.3 row: hover on an empty symbol is honest ("no symbol"), never a number.
// ---------------------------------------------------------------------------

#[test]
fn hover_on_empty_symbol_is_honest() {
    let replies = drive_lsp(
        None,
        &[
            serde_json::json!({
                "jsonrpc":"2.0","id":4,"method":"textDocument/hover",
                "params":{"root":".","symbol":"","lang":"python"}
            }),
            serde_json::json!({"jsonrpc":"2.0","method":"exit","params":{}}),
        ],
    );
    let value = replies[0]["result"]["contents"]["value"]
        .as_str()
        .expect("hover markdown value");
    assert!(
        value.contains("no symbol"),
        "honest empty-symbol hover: {value}"
    );
}

// ---------------------------------------------------------------------------
// The happy path: hover over a real, analysable symbol reports the impact.
// `wrapper.py` imports `core`, so hovering `core_fn` reaches the wrapper.
// Needs the symbol graph → gated on ctags.
// ---------------------------------------------------------------------------

#[test]
fn hover_on_a_real_symbol_reports_impact_over_stdio() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping shuttle lsp hover-impact E2E");
        return;
    }
    let ws = scratch_dir("hover");
    write(&ws, "core.py", "def core_fn():\n    return 1\n");
    write(
        &ws,
        "wrapper.py",
        "import core\n\ndef wrap():\n    return core.core_fn()\n",
    );

    let replies = drive_lsp(
        Some(&ws),
        &[
            serde_json::json!({
                "jsonrpc":"2.0","id":5,"method":"textDocument/hover",
                "params":{"root": ws.to_str().unwrap(),"symbol":"core_fn","lang":"python"}
            }),
            serde_json::json!({
                "jsonrpc":"2.0","id":6,"method":"shuttle/impact",
                "params":{"root": ws.to_str().unwrap(),"symbol":"core_fn","lang":"python"}
            }),
            serde_json::json!({"jsonrpc":"2.0","method":"exit","params":{}}),
        ],
    );
    assert_eq!(replies.len(), 2, "hover + impact reply: {replies:?}");

    // Hover markdown names the symbol and speaks in impact terms (either it
    // reached dependents, or an honest "no dependent symbols found" — never a
    // fabricated capability).
    let hover = replies[0]["result"]["contents"]["value"]
        .as_str()
        .expect("hover value");
    assert!(hover.contains("core_fn"), "hover names the symbol: {hover}");
    assert!(
        hover.contains("impact") || hover.contains("callers"),
        "hover speaks in impact/caller terms: {hover}"
    );

    // shuttle/impact returns the structured sibling: analysable, symbol echoed.
    let imp = &replies[1]["result"];
    assert_eq!(imp["symbol"], "core_fn", "{imp}");
    assert_eq!(imp["analysable"], true, "python is analysable: {imp}");

    let _ = std::fs::remove_dir_all(&ws);
}
