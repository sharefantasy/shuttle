import sys, io

path = "/Users/fankainang/toolspace/shuttle/crates/shuttle-cli/src/run.rs"
src = io.open(path, encoding="utf-8").read()
orig = src

edits = []  # (old, new, expected_count)

# --- 1. add helper fns after soft_timeout() ---
old_soft = '''fn soft_timeout() -> Option<Duration> {
    std::env::var("SHUTTLE_SOFT_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&ms| ms > 0)
        .map(Duration::from_millis)
}
'''
new_soft = old_soft + '''
/// Progress stall budget (`SHUTTLE_STALL_TIMEOUT_MS`). When set, shuttle kills pi and
/// reports exit code 125 if no *progress* event arrives within this window —
/// i.e. the model emits only empty (0-token) `message_update` heartbeats while
/// neither tokens nor tool activity advance. Distinct from the wall-clock soft
/// timeout (124); this catches provider empty-response livelocks where the stream
/// never falls silent but never advances either. Absent/0 disables it.
fn stall_timeout() -> Option<Duration> {
    std::env::var("SHUTTLE_STALL_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&ms| ms > 0)
        .map(Duration::from_millis)
}

/// A pi stream line counts as *progress* unless it is a zero-token
/// `message_update` heartbeat. Any tool activity, message/turn boundary, or
/// token-bearing update resets the stall clock.
fn event_is_progress(v: &serde_json::Value) -> bool {
    match v.get("type").and_then(serde_json::Value::as_str) {
        Some("message_update") => v
            .pointer("/usage/totalTokens")
            .and_then(serde_json::Value::as_u64)
            .map_or(false, |t| t > 0),
        _ => true,
    }
}
'''
edits.append((old_soft, new_soft, 1))

# --- 2a. form B vars (no `settled`) ---
old_b_vars = '''    let deadline = soft_timeout().map(|d| started + d);
    let mut acc = StreamAcc::default();
    let mut timed_out = false;
    let out = std::io::stdout();'''
new_b_vars = '''    let deadline = soft_timeout().map(|d| started + d);
    let stall_budget = stall_timeout();
    let mut acc = StreamAcc::default();
    let mut timed_out = false;
    let mut stalled = false;
    let mut last_progress = Instant::now();
    let out = std::io::stdout();'''
edits.append((old_b_vars, new_b_vars, 1))

# --- 2b. form C vars (has `settled`) ---
old_c_vars = '''    let deadline = soft_timeout().map(|d| started + d);
    let mut acc = StreamAcc::default();
    let mut timed_out = false;
    let mut settled = false;
    let out = std::io::stdout();'''
new_c_vars = '''    let deadline = soft_timeout().map(|d| started + d);
    let stall_budget = stall_timeout();
    let mut acc = StreamAcc::default();
    let mut timed_out = false;
    let mut stalled = false;
    let mut settled = false;
    let mut last_progress = Instant::now();
    let out = std::io::stdout();'''
edits.append((old_c_vars, new_c_vars, 1))

# --- 3. recv block (identical in both loops) -> replace both ---
old_recv = '''        let recv = match deadline {
            Some(dl) => {
                let now = Instant::now();
                if now >= dl {
                    timed_out = true;
                    break;
                }
                rx.recv_timeout(dl - now)
            }
            None => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
        };'''
new_recv = '''        let recv = {
            let now = Instant::now();
            if let Some(dl) = deadline {
                if now >= dl {
                    timed_out = true;
                    break;
                }
            }
            if let Some(sb) = stall_budget {
                if now.duration_since(last_progress) >= sb {
                    stalled = true;
                    break;
                }
            }
            match (deadline, stall_budget) {
                (None, None) => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
                _ => {
                    let mut wait = Duration::from_millis(500);
                    if let Some(dl) = deadline {
                        wait = wait.min(dl.saturating_duration_since(now));
                    }
                    rx.recv_timeout(wait)
                }
            }
        };'''
edits.append((old_recv, new_recv, 2))

# --- 4. Timeout arm (identical both) -> continue instead of break ---
old_to = '''            Err(mpsc::RecvTimeoutError::Timeout) => {
                timed_out = true;
                break;
            }'''
new_to = '''            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Tick woke us; re-check soft/stall deadlines at loop top.
                continue;
            }'''
edits.append((old_to, new_to, 2))

# --- 5a. form B ingest block ---
old_b_ing = '''                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                    ingest_event(&mut acc, &v);
                }'''
new_b_ing = '''                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                    ingest_event(&mut acc, &v);
                    if event_is_progress(&v) {
                        last_progress = Instant::now();
                    }
                }'''
edits.append((old_b_ing, new_b_ing, 1))

# --- 5b. form C ingest block (followed by match) ---
old_c_ing = '''                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                    ingest_event(&mut acc, &v);
                    match v.get("type").and_then(serde_json::Value::as_str) {'''
new_c_ing = '''                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                    ingest_event(&mut acc, &v);
                    if event_is_progress(&v) {
                        last_progress = Instant::now();
                    }
                    match v.get("type").and_then(serde_json::Value::as_str) {'''
edits.append((old_c_ing, new_c_ing, 1))

# --- 6. exit_code (identical both) ---
old_exit = '''    let exit_code = if timed_out {
        124
    } else {
        match status {
            Ok(st) => st.code().unwrap_or(-1),
            Err(_) => -1,
        }
    };'''
new_exit = '''    let exit_code = if stalled {
        125
    } else if timed_out {
        124
    } else {
        match status {
            Ok(st) => st.code().unwrap_or(-1),
            Err(_) => -1,
        }
    };'''
edits.append((old_exit, new_exit, 2))

# --- 7a. form B kill block ---
old_b_kill = '''    if timed_out {
        tracing::warn!("shuttle run: soft timeout hit, killing pi");
        let _ = child.kill();
    }'''
new_b_kill = '''    if stalled {
        tracing::warn!("shuttle run: progress stall detected, killing pi");
        let _ = child.kill();
    } else if timed_out {
        tracing::warn!("shuttle run: soft timeout hit, killing pi");
        let _ = child.kill();
    }'''
edits.append((old_b_kill, new_b_kill, 1))

# --- 7b. form C kill block ---
old_c_kill = '''    drop(stdin);
    if timed_out {
        tracing::warn!("shuttle run (rpc): soft timeout hit, killing pi");
        let _ = child.kill();
    }'''
new_c_kill = '''    drop(stdin);
    if stalled {
        tracing::warn!("shuttle run (rpc): progress stall detected, killing pi");
        let _ = child.kill();
    } else if timed_out {
        tracing::warn!("shuttle run (rpc): soft timeout hit, killing pi");
        let _ = child.kill();
    }'''
edits.append((old_c_kill, new_c_kill, 1))

# --- 8. add unit tests before final closing brace of tests mod ---
old_test_anchor = '''    #[test]
    fn soft_timeout_env_parsing() {
        std::env::set_var("SHUTTLE_SOFT_TIMEOUT_MS", "1500");
        assert_eq!(soft_timeout(), Some(Duration::from_millis(1500)));
        std::env::set_var("SHUTTLE_SOFT_TIMEOUT_MS", "0");
        assert_eq!(soft_timeout(), None);
        std::env::remove_var("SHUTTLE_SOFT_TIMEOUT_MS");
        assert_eq!(soft_timeout(), None);
    }'''
new_test_anchor = old_test_anchor + '''

    #[test]
    fn stall_timeout_env_parsing() {
        std::env::set_var("SHUTTLE_STALL_TIMEOUT_MS", "90000");
        assert_eq!(stall_timeout(), Some(Duration::from_millis(90000)));
        std::env::set_var("SHUTTLE_STALL_TIMEOUT_MS", "0");
        assert_eq!(stall_timeout(), None);
        std::env::remove_var("SHUTTLE_STALL_TIMEOUT_MS");
        assert_eq!(stall_timeout(), None);
    }

    #[test]
    fn empty_heartbeat_is_not_progress() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"type":"message_update","usage":{"totalTokens":0}}"#,
        )
        .unwrap();
        assert!(!event_is_progress(&v));
    }

    #[test]
    fn token_update_and_tool_events_are_progress() {
        let tok: serde_json::Value = serde_json::from_str(
            r#"{"type":"message_update","usage":{"totalTokens":42}}"#,
        )
        .unwrap();
        assert!(event_is_progress(&tok));
        let tool: serde_json::Value =
            serde_json::from_str(r#"{"type":"tool_execution_start","toolName":"bash"}"#).unwrap();
        assert!(event_is_progress(&tool));
    }'''
edits.append((old_test_anchor, new_test_anchor, 1))

# apply with strict count checks
for i, (old, new, exp) in enumerate(edits):
    c = src.count(old)
    if c != exp:
        print("EDIT %d COUNT MISMATCH: expected %d got %d" % (i, exp, c))
        print("---- snippet ----")
        print(old[:200])
        sys.exit(2)
    src = src.replace(old, new)

io.open(path, "w", encoding="utf-8").write(src)
print("OK: all %d edits applied. bytes %d -> %d" % (len(edits), len(orig), len(src)))
