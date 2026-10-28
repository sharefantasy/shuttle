//! §9.9 — on-demand real-LSP bridge (isolated, optional).
//!
//! Tier-3 capabilities ("need real semantics": goto type-definition,
//! completion, type hierarchy) do NOT belong in `shuttle-nav`'s static ladder. They
//! live here behind the OFF-by-default `real-lsp` feature so the core `shuttle`
//! binary never links a language server. Every result normalises to
//! [`shuttle_nav::NavHit`] with `source = Backend::Lsp` and `confidence = High`, and
//! is explicitly tagged "via external LSP" so the planner knows it did not come
//! from shuttle's static analysis.
//!
//! Lifecycle is **用完即走** (use-and-leave): a query spawns the server, asks
//! its one question, and kills it — never resident, matching §8's no-daemon
//! boundary.

use shuttle_nav::{Backend, Confidence, NavHit, NavKind};

/// Shared LSP wire framing (`Content-Length` header + JSON body). One
/// implementation used by BOTH the client [`conversation`] and the
/// [`server`], so the two directions can never drift. Pure transport.
pub mod framing;

/// §9.9 tier-3 — the transport-generic LSP *client* conversation: drive a
/// full `initialize -> didOpen -> <op> -> shutdown -> exit` handshake over any
/// [`std::io::BufRead`]+[`std::io::Write`] pair, correlate responses by id, and
/// map LSP `Location`/`LocationLink` results into [`shuttle_nav::NavHit`]. Pure (no
/// spawning) so it lives in the DEFAULT build and is hermetically testable; the
/// feature-gated [`real`] adapter drives it over a spawned child's pipes.
pub mod conversation;

/// §9.9 D2 — lower shuttle's own refactors into LSP `codeAction`s (pure; no server).
pub mod code_action;

/// §9.9·D1 — shuttle as an LSP *server* (stdio, on-demand, no port). Mirror of the
/// client tiers above: feeds shuttle's own impact/callers confidence into an editor
/// hover / codeLens over stdio, spawn-per-session, idle-exit — never resident.
pub mod server;

/// Which real language server a request would route to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspServer {
    /// rust-analyzer — Rust.
    RustAnalyzer,
    /// tsserver / typescript-language-server — TS/JS.
    Tsserver,
    /// pyright — Python.
    Pyright,
}

impl LspServer {
    /// The launch command a bridge build would exec (for diagnostics / `shuttle
    /// backends` style reporting). Present even when the feature is off so the
    /// CLI can *describe* the tier without compiling the adapters.
    pub fn command(self) -> &'static str {
        match self {
            LspServer::RustAnalyzer => "rust-analyzer",
            LspServer::Tsserver => "typescript-language-server",
            LspServer::Pyright => "pyright-langserver",
        }
    }

    /// Route a language id (as from `shuttle_symbols::lang_for_path`) to a server.
    pub fn for_lang(lang: &str) -> Option<LspServer> {
        match lang {
            "rust" => Some(LspServer::RustAnalyzer),
            "typescript" | "tsx" | "javascript" => Some(LspServer::Tsserver),
            "python" => Some(LspServer::Pyright),
            _ => None,
        }
    }
}

/// A tier-3 request: what to ask a real server about a symbol at a position.
#[derive(Debug, Clone)]
pub struct LspRequest {
    pub server: LspServer,
    pub root: String,
    pub path: String,
    pub line: usize,
    pub col: usize,
    pub symbol: String,
    pub op: LspOp,
}

/// The type-level operation being requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspOp {
    /// goto type-definition (resolve the *type* of an expression).
    TypeDefinition,
    /// type hierarchy (super/subtypes).
    TypeHierarchy,
}

/// Errors surfaced by the bridge.
#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    /// The `real-lsp` feature was not compiled in — the core binary is
    /// intentionally LSP-free. Rebuild with `--features real-lsp` to enable.
    #[error("shuttle-lsp-bridge compiled without `real-lsp`; real LSP adapters are disabled")]
    Disabled,
    /// The requested server binary was not found on PATH.
    #[error("language server `{0}` not found on PATH")]
    ServerMissing(&'static str),
    /// The server was spawned but the query failed.
    #[error("LSP query failed: {0}")]
    Query(String),
}

/// True only when the crate was built with the `real-lsp` feature. Lets callers
/// (e.g. `shuttle backends`) report tier-3 availability without a hard dependency.
pub const fn real_lsp_enabled() -> bool {
    cfg!(feature = "real-lsp")
}

/// Tag a hit produced through this bridge so downstream never confuses it with
/// shuttle's static ladder. Always High + Backend::Lsp per §9.9.
// Only exercised by the `real` adapters (once a transport is wired) and by
// tests; keep it from tripping dead_code in scaffold builds either way.
#[allow(dead_code)]
fn lsp_hit(req: &LspRequest, line: usize, snippet: impl Into<String>) -> NavHit {
    NavHit {
        path: req.path.clone(),
        line,
        col: None,
        kind: NavKind::Definition,
        symbol: req.symbol.clone(),
        snippet: Some(format!(
            "[via external LSP: {}] {}",
            req.server.command(),
            snippet.into()
        )),
        confidence: Confidence::High,
        source: Backend::Lsp,
    }
}

/// Resolve a tier-3 request via a real language server.
///
/// **用完即走**: when the `real-lsp` feature is enabled this spawns the server,
/// issues the single request, and kills it. With the feature off (the default)
/// it returns [`BridgeError::Disabled`] immediately — the core binary links no
/// server. The stub keeps `lsp_hit`/types exercised so the schema is stable.
pub fn resolve(req: &LspRequest) -> Result<Vec<NavHit>, BridgeError> {
    #[cfg(not(feature = "real-lsp"))]
    {
        let _ = req; // keep the signature honest without spawning anything.
        Err(BridgeError::Disabled)
    }
    #[cfg(feature = "real-lsp")]
    {
        real::resolve(req)
    }
}

// The real adapters compile only under the feature. They spawn the server,
// speak LSP over stdio, take the answer, and drop (killing) the child — never
// resident. Kept minimal/behind-feature so the default build is server-free.
#[cfg(feature = "real-lsp")]
mod real {
    use super::*;
    use crate::conversation::converse;
    use std::io::BufReader;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    /// Deadline (ms) for a single spawn-query-kill conversation. A heavyweight
    /// server (e.g. rust-analyzer indexing a cold workspace) can take a long
    /// time to answer; rather than hang forever the owning thread kills the
    /// child at the deadline, which closes its stdout and makes the worker's
    /// blocking `converse` read return an honest EOF error. Override with
    /// `SHUTTLE_LSP_TIMEOUT_MS` (0 disables the deadline entirely, for debugging).
    fn timeout_ms() -> u64 {
        std::env::var("SHUTTLE_LSP_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(30_000)
    }

    /// Arbitrate a worker's result against a deadline — the pure heart of the
    /// spawn-query-kill watchdog, split out of [`resolve`] so it is
    /// hermetically testable without spawning a real language server.
    ///
    /// * `deadline_ms == 0` — no deadline; block until the worker answers
    ///   (debug/testing only). A closed channel maps to an honest "worker
    ///   vanished" [`BridgeError::Query`].
    /// * otherwise — wait at most `deadline_ms`. On timeout, run `on_timeout`
    ///   (the caller's kill trigger) and return an honest "no answer" error;
    ///   we never fabricate a hit for a server that never answered (§7). A
    ///   disconnected channel is likewise reported, not swallowed.
    fn await_with_deadline(
        rx: &mpsc::Receiver<Result<Vec<NavHit>, BridgeError>>,
        deadline_ms: u64,
        cmd: &str,
        on_timeout: &mut dyn FnMut(),
    ) -> Result<Vec<NavHit>, BridgeError> {
        if deadline_ms == 0 {
            return rx
                .recv()
                .unwrap_or_else(|_| Err(BridgeError::Query(format!("{cmd}: worker vanished"))));
        }
        match rx.recv_timeout(Duration::from_millis(deadline_ms)) {
            Ok(res) => res,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // The server did not answer in time. The caller's `on_timeout`
                // kills the child, closing its stdout so the worker's blocking
                // read returns EOF and the worker finishes. Report honestly.
                on_timeout();
                Err(BridgeError::Query(format!(
                    "{cmd}: no answer within {deadline_ms}ms (killed; set SHUTTLE_LSP_TIMEOUT_MS to adjust)"
                )))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(BridgeError::Query(format!("{cmd}: worker vanished")))
            }
        }
    }

    /// Resolve a tier-3 request by spawning the language server, running the
    /// transport-generic [`converse`] over its stdio pipes, and killing it.
    ///
    /// yong-wan-ji-zou: the child is spawned with piped stdin/stdout, we speak
    /// exactly one LSP conversation over those pipes, then `child.kill()` +
    /// `child.wait()` guarantee no lingering process. Nothing binds a port and
    /// nothing stays resident. The single conversation is the SAME pure function
    /// the hermetic tests drive, so the spawned path can never diverge from the
    /// tested one.
    pub fn resolve(req: &LspRequest) -> Result<Vec<NavHit>, BridgeError> {
        let cmd = req.server.command();
        // Presence check — resolve the binary on PATH before spawning.
        let found = Command::new(cmd)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !found {
            return Err(BridgeError::ServerMissing(cmd));
        }

        // Read the target file text once (fed to the server via didOpen). An
        // unreadable file is an honest hard error — never a fabricated answer.
        let full_path = if req.path.starts_with('/') || req.root.is_empty() || req.root == "." {
            req.path.clone()
        } else {
            format!("{}/{}", req.root.trim_end_matches('/'), req.path)
        };
        let text = std::fs::read_to_string(&full_path)
            .map_err(|e| BridgeError::Query(format!("read {full_path}: {e}")))?;

        // Spawn with piped stdio (stderr silenced so a chatty server never
        // corrupts our framed channel).
        let mut child = Command::new(cmd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| BridgeError::Query(format!("spawn {cmd}: {e}")))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| BridgeError::Query(format!("{cmd}: no stdin pipe")))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| BridgeError::Query(format!("{cmd}: no stdout pipe")))?;

        // Drive the conversation on a WORKER thread that OWNS the pipes; keep
        // the `Child` handle on THIS thread so we alone can kill it. This is the
        // key to a bounded lifetime: `converse` does blocking reads with no
        // internal deadline, so if we waited on it inline a wedged/indexing
        // server would hang us forever. Instead we `recv_timeout` on the result
        // channel; on timeout WE kill the child, its stdout closes, the worker's
        // blocking read returns EOF (-> Query error), and the worker finishes.
        //
        // The conversation is the SAME pure function the hermetic tests drive;
        // only its host thread and the kill trigger differ, so the tested and
        // spawned paths cannot diverge in behaviour, only in liveness.
        let req_owned = req.clone();
        let text_owned = text.clone();
        let (tx, rx) = mpsc::channel::<Result<Vec<NavHit>, BridgeError>>();
        let worker = thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut stdin = stdin;
            let outcome = converse(&mut reader, &mut stdin, &req_owned, &text_owned);
            // Drop stdin here (EOF to the server) before reporting, so a polite
            // server can shut its own loop even before we kill it.
            drop(stdin);
            let _ = tx.send(outcome);
        });

        // Arbitrate the deadline with the extracted, hermetically-testable
        // helper. The kill trigger is injected as a closure so the "server
        // never answered -> kill -> honest error" decision can be exercised in
        // tests with an in-memory channel, never a real process. The closure is
        // scoped so its &mut borrow of `child` ends before the reap below.
        let deadline = timeout_ms();
        let outcome = {
            let mut on_timeout = || {
                let _ = child.kill();
            };
            await_with_deadline(&rx, deadline, cmd, &mut on_timeout)
        };

        // yong-wan-ji-zou: ensure the child is terminated and reaped, then join
        // the worker so no thread or process lingers past this call.
        let _ = child.kill();
        let _ = child.wait();
        let _ = worker.join();

        outcome
    }

    #[cfg(test)]
    mod watchdog_tests {
        use super::await_with_deadline;
        use crate::{lsp_hit, BridgeError, LspOp, LspRequest, LspServer};
        use shuttle_nav::NavHit;
        use std::sync::mpsc;
        use std::thread;
        use std::time::Duration;

        fn sample_hits() -> Vec<NavHit> {
            let req = LspRequest {
                server: LspServer::RustAnalyzer,
                root: "/repo".into(),
                path: "src/lib.rs".into(),
                line: 1,
                col: 0,
                symbol: "Widget".into(),
                op: LspOp::TypeDefinition,
            };
            vec![lsp_hit(&req, 7, "class Widget")]
        }

        // A worker that answers before the deadline is returned verbatim, and
        // the kill trigger is NOT fired — a healthy query never gets killed.
        #[test]
        fn answer_before_deadline_passes_through_without_kill() {
            let (tx, rx) = mpsc::channel();
            let worker = thread::spawn(move || {
                let _ = tx.send(Ok(sample_hits()));
            });
            let mut killed = false;
            let out = {
                let mut on_timeout = || killed = true;
                await_with_deadline(&rx, 2_000, "rust-analyzer", &mut on_timeout)
            };
            worker.join().unwrap();
            let hits = out.expect("should carry the worker's Ok result");
            assert_eq!(hits.len(), 1);
            assert!(!killed, "a timely answer must never trigger the kill path");
        }

        // A worker that never answers within the deadline yields an honest
        // "no answer ... (killed)" error AND fires the kill trigger exactly
        // once — never a fabricated hit for a server that stayed silent (§7).
        #[test]
        fn silence_past_deadline_kills_and_reports_honestly() {
            // Hold the sender so the channel stays open but silent; the receiver
            // must time out rather than see a Disconnected.
            let (tx, rx) = mpsc::channel::<Result<Vec<NavHit>, BridgeError>>();
            let mut kill_count = 0usize;
            let out = {
                let mut on_timeout = || kill_count += 1;
                await_with_deadline(&rx, 20, "rust-analyzer", &mut on_timeout)
            };
            drop(tx); // release only after arbitration, proving it was a timeout.
            match out {
                Err(BridgeError::Query(msg)) => {
                    assert!(msg.contains("no answer within 20ms"), "msg={msg}");
                    assert!(msg.contains("killed"), "msg={msg}");
                }
                other => panic!("expected honest Query timeout, got {other:?}"),
            }
            assert_eq!(
                kill_count, 1,
                "kill trigger must fire exactly once on timeout"
            );
        }

        // With the deadline disabled (0), we block for the answer however long
        // it takes; the kill path is never reached.
        #[test]
        fn zero_deadline_blocks_until_answer() {
            let (tx, rx) = mpsc::channel();
            let worker = thread::spawn(move || {
                thread::sleep(Duration::from_millis(30));
                let _ = tx.send(Ok(sample_hits()));
            });
            let mut killed = false;
            let out = {
                let mut on_timeout = || killed = true;
                await_with_deadline(&rx, 0, "rust-analyzer", &mut on_timeout)
            };
            worker.join().unwrap();
            assert_eq!(out.expect("blocking recv should succeed").len(), 1);
            assert!(!killed);
        }

        // A worker that dies without sending (dropped sender) is reported as an
        // honest "worker vanished" error, never a silent empty success.
        #[test]
        fn dropped_worker_is_reported_not_swallowed() {
            let (tx, rx) = mpsc::channel::<Result<Vec<NavHit>, BridgeError>>();
            drop(tx); // worker vanished before sending.
            let mut killed = false;
            let out = {
                let mut on_timeout = || killed = true;
                // Non-zero deadline: recv_timeout sees Disconnected immediately.
                await_with_deadline(&rx, 1_000, "rust-analyzer", &mut on_timeout)
            };
            match out {
                Err(BridgeError::Query(msg)) => {
                    assert!(msg.contains("worker vanished"), "msg={msg}")
                }
                other => panic!("expected worker-vanished Query, got {other:?}"),
            }
            assert!(!killed, "a disconnected channel is not a timeout kill");
        }

        // Same vanished-worker honesty on the zero-deadline (blocking) path.
        #[test]
        fn dropped_worker_zero_deadline_is_reported() {
            let (tx, rx) = mpsc::channel::<Result<Vec<NavHit>, BridgeError>>();
            drop(tx);
            let mut killed = false;
            let out = {
                let mut on_timeout = || killed = true;
                await_with_deadline(&rx, 0, "rust-analyzer", &mut on_timeout)
            };
            match out {
                Err(BridgeError::Query(msg)) => {
                    assert!(msg.contains("worker vanished"), "msg={msg}")
                }
                other => panic!("expected worker-vanished Query, got {other:?}"),
            }
            assert!(!killed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // This invariant only holds in the default (feature-off) build; under
    // `--features real-lsp` the bridge is enabled by design.
    #[cfg(not(feature = "real-lsp"))]
    #[test]
    fn default_build_is_lsp_free() {
        // With the default feature set, the bridge must not pretend to work.
        assert!(!real_lsp_enabled());
        let req = LspRequest {
            server: LspServer::RustAnalyzer,
            root: ".".into(),
            path: "src/lib.rs".into(),
            line: 1,
            col: 0,
            symbol: "Foo".into(),
            op: LspOp::TypeDefinition,
        };
        match resolve(&req) {
            Err(BridgeError::Disabled) => {}
            other => panic!("expected Disabled, got {other:?}"),
        }
    }

    #[test]
    fn server_routing_by_lang() {
        assert_eq!(LspServer::for_lang("rust"), Some(LspServer::RustAnalyzer));
        assert_eq!(LspServer::for_lang("tsx"), Some(LspServer::Tsserver));
        assert_eq!(LspServer::for_lang("python"), Some(LspServer::Pyright));
        assert_eq!(LspServer::for_lang("go"), None);
    }

    #[test]
    fn lsp_hits_are_high_and_tagged() {
        let req = LspRequest {
            server: LspServer::Pyright,
            root: ".".into(),
            path: "m.py".into(),
            line: 10,
            col: 3,
            symbol: "Widget".into(),
            op: LspOp::TypeDefinition,
        };
        let h = lsp_hit(&req, 42, "class Widget");
        assert_eq!(h.confidence, Confidence::High);
        assert_eq!(h.source, Backend::Lsp);
        assert_eq!(h.line, 42);
        assert!(h.snippet.unwrap().contains("via external LSP"));
    }
}
