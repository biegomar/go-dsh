//! Client for a running DeepSeek Harness web server (`dsh web`).
//! Jobs started through it run inside that process, so they appear live in the
//! browser UI, grouped under their Workspace.
//!
//! Auth: `dsh web` prints a URL carrying a per-process launch token. `GET /?token=...`
//! answers 303 with a signed 30-day cookie, which go-dsh caches in
//! `~/.dshgo/web-auth.json`. Remote calls: `POST /api/<namespace>/<method>`.

use std::fs;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use url::Url;

use crate::config::cfg;
use crate::dsh::dsh_command;
use crate::paths::{home, logs_dir};
use crate::util::{detach_command, home_dir, now_iso, read_json, sleep_ms, write_private, DshgoError, Result};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

fn auth_file() -> std::path::PathBuf {
    home().join("web-auth.json")
}

pub fn token_url_file() -> std::path::PathBuf {
    home().join("web-token-url")
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new().redirects(0).timeout(timeout).build()
}

fn origin_of(u: &str) -> Option<String> {
    Url::parse(u).ok().map(|p| p.origin().ascii_serialization())
}

/// Places a token URL may come from, most explicit first.
fn token_url_candidates() -> Vec<String> {
    let mut files: Vec<std::path::PathBuf> = vec![];
    if let Some(f) = std::env::var_os("DSHGO_WEB_TOKEN_FILE").filter(|v| !v.is_empty()) {
        files.push(f.into());
    }
    files.push(token_url_file());
    let cache = std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()).map(std::path::PathBuf::from).or_else(|| home_dir().map(|h| h.join(".cache")));
    if let Some(c) = cache {
        files.push(c.join("deepseek-harness").join("last-url"));
    }
    let mut urls: Vec<String> = std::env::var("DSHGO_WEB_TOKEN_URL").ok().filter(|v| !v.is_empty()).into_iter().collect();
    for f in files {
        if let Ok(raw) = fs::read_to_string(&f) {
            let raw = raw.trim();
            if raw.starts_with("http") {
                urls.push(raw.to_string());
            }
        }
    }
    urls
}

/// Candidate token URLs for the configured origin, most explicit first.
fn token_urls() -> Vec<String> {
    let base = cfg("webUrl").map(|b| b.trim_end_matches('/').to_string());
    let urls = token_url_candidates();
    match base {
        Some(b) => urls.into_iter().filter(|u| origin_of(u).as_deref() == Some(b.as_str())).collect(),
        None => urls,
    }
}

/// Remember a token URL printed by `dsh web` (`go-dsh ui login` / `ui start`).
pub fn save_token_url(url: &str) -> Result<String> {
    let parsed = Url::parse(url).map_err(|e| DshgoError::new(format!("Invalid URL: {e}"), 2))?;
    if !parsed.query_pairs().any(|(k, v)| k == "token" && !v.is_empty()) {
        return Err(DshgoError::new("The URL carries no ?token= parameter", 2).hint("copy the full URL that `dsh web` prints"));
    }
    write_private(&token_url_file(), &format!("{}\n", parsed.as_str()))?;
    Ok(parsed.origin().ascii_serialization())
}

/// Address to open in a browser: the remembered token URL (it logs the browser in through the same
/// 303 handshake go-dsh uses) or, without one, the plain origin.
pub fn browser_url() -> String {
    token_urls().into_iter().next().unwrap_or_else(base_url)
}

pub fn base_url() -> String {
    if let Some(c) = cfg("webUrl") {
        return c.trim_end_matches('/').to_string();
    }
    if let Some(o) = token_urls().first().and_then(|t| origin_of(t)) {
        return o;
    }
    "http://127.0.0.1:3080".into()
}

/// Exchange a token URL for the cookie; `only` restricts the attempt to one URL.
pub fn exchange_token(only: Option<&str>) -> Option<String> {
    let candidates: Vec<String> = match only {
        Some(o) => vec![o.to_string()],
        None => token_urls(),
    };
    let base = base_url();
    for t in candidates {
        if origin_of(&t).as_deref() != Some(base.as_str()) {
            continue;
        }
        let res = match agent(Duration::from_secs(5)).get(&t).call() {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            Err(_) => return None,
        };
        let set_cookie = res.header("set-cookie").map(String::from);
        if res.status() != 303 {
            continue;
        }
        let Some(set_cookie) = set_cookie else { continue };
        let cookie = set_cookie.split(';').next().unwrap_or("").to_string();
        let doc = json!({ "origin": base, "cookie": cookie, "savedAt": now_iso() });
        let _ = write_private(&auth_file(), &(doc.to_string() + "\n"));
        return Some(cookie);
    }
    None
}

fn cookie(refresh: bool) -> Option<String> {
    if !refresh {
        if let Ok(Some(cached)) = read_json::<Value>(&auth_file()) {
            if cached.get("origin").and_then(|o| o.as_str()) == Some(base_url().as_str()) {
                if let Some(c) = cached.get("cookie").and_then(|c| c.as_str()).filter(|c| !c.is_empty()) {
                    return Some(c.to_string());
                }
            }
        }
    }
    exchange_token(None)
}

/// Call one unary Remote method. `args` maps parameter names to values.
pub fn call(method: &str, args: Value) -> Result<Value> {
    call_inner(method, args, true)
}

fn call_inner(method: &str, args: Value, retry_auth: bool) -> Result<Value> {
    let c = cookie(false).ok_or_else(|| {
        DshgoError::new("Cannot authenticate against the DSH web UI (no valid token)", 5).hint("go-dsh ui start, or go-dsh ui login <url printed by dsh web>")
    })?;
    let body = json!({ "type": "client-request", "rpcId": uuid::Uuid::new_v4().to_string(), "method": method, "payload": { "args": args } });
    let url = format!("{}/api/{}", base_url(), method);
    let res = match agent(REQUEST_TIMEOUT).post(&url).set("cookie", &c).send_json(body) {
        Ok(r) => r,
        Err(ureq::Error::Status(401, _)) if retry_auth => {
            cookie(true);
            return call_inner(method, args, false);
        }
        Err(ureq::Error::Status(code, _)) => return Err(DshgoError::new(format!("DSH web {method}: HTTP {code}"), 5)),
        Err(e) => {
            return Err(DshgoError::new(format!("DSH web UI at {} is not reachable: {e}", base_url()), 5).hint("go-dsh ui start"));
        }
    };
    if res.status() == 401 && retry_auth {
        cookie(true);
        return call_inner(method, args, false);
    }
    if res.status() != 200 {
        return Err(DshgoError::new(format!("DSH web {method}: HTTP {}", res.status()), 5));
    }
    let body: Value = res.into_json().map_err(|e| DshgoError::new(format!("DSH web {method}: bad response: {e}"), 1))?;
    let result = &body["result"];
    if result["ok"].as_bool() != Some(true) {
        let msg = result["error"]["message"].as_str().unwrap_or("error");
        return Err(DshgoError::new(format!("DSH web {method}: {msg}"), 1));
    }
    Ok(result["value"].clone())
}

/// True when the DSH web server answers an authenticated request.
pub fn available() -> bool {
    let Some(c) = cookie(false) else { return false };
    let get = |c: &str| -> Option<u16> {
        match agent(Duration::from_millis(2500)).get(&format!("{}/", base_url())).set("cookie", c).call() {
            Ok(r) => Some(r.status()),
            Err(ureq::Error::Status(code, _)) => Some(code),
            Err(_) => None,
        }
    };
    match get(&c) {
        Some(401) => match cookie(true) {
            Some(fresh) => get(&fresh) == Some(200),
            None => false,
        },
        Some(code) => code == 200,
        None => false,
    }
}

/// Register (or adopt) the Workspace for an existing directory.
pub fn ensure_workspace(path: &str) -> Result<Value> {
    let v = call("workspace/create", json!({ "request": { "path": path } }))?;
    Ok(v["workspace"].clone())
}

pub fn create_session(workspace_id: &str) -> Result<Value> {
    call("session/create", json!({ "request": { "workspaceId": workspace_id } }))
}

pub fn rename_session(session_id: &str, title: &str) -> Result<Value> {
    let t: String = title.chars().take(120).collect();
    call("session/rename", json!({ "request": { "sessionId": session_id, "title": t } }))
}

pub fn select_model(session_id: &str, selection: &Value) -> Result<Value> {
    let mut req = json!({ "sessionId": session_id });
    if let (Some(r), Some(s)) = (req.as_object_mut(), selection.as_object()) {
        for (k, v) in s {
            r.insert(k.clone(), v.clone());
        }
    }
    call("session/selectModel", json!({ "request": req }))
}

/// Queue a prompt; returns the request id used to find it in the log again.
pub fn prompt(session_id: &str, text: &str) -> Result<String> {
    let request_id = uuid::Uuid::new_v4().to_string();
    call(
        "session/prompt",
        json!({ "request": { "requestId": request_id, "sessionId": session_id, "mode": "queue", "content": [{ "type": "text", "text": text }] } }),
    )?;
    Ok(request_id)
}

pub fn cancel_session(session_id: &str) -> Result<Value> {
    call("session/cancel", json!({ "request": { "sessionId": session_id } }))
}

fn address(session_id: &str) -> Value {
    json!({ "kind": "session", "sessionId": session_id })
}

/// The newest seq the server reports when asked for a cut beyond the end of the log. The wording
/// depends on the dsh version: older builds say "past cursor N", current ones "session history cut X exceeds N".
fn parse_cursor_from_error(message: &str) -> Option<i64> {
    for marker in ["past cursor ", "exceeds "] {
        if let Some(pos) = message.find(marker) {
            let digits: String = message[pos + marker.len()..].chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(n) = digits.parse() {
                return Some(n);
            }
        }
    }
    None
}

/// Newest committed event seq of a Session.
pub fn cursor(session_id: &str) -> Result<i64> {
    match call("session/page", json!({ "request": { "address": address(session_id), "throughSeq": 2147483647u32, "maxMessages": 1 } })) {
        Ok(v) => v["cursor"].as_i64().ok_or_else(|| DshgoError::new("DSH web: cannot determine the session cursor", 1)),
        Err(e) => parse_cursor_from_error(&e.message).ok_or(e),
    }
}

/// All events with seq in (after_seq, through_seq], oldest first.
pub fn events_since(session_id: &str, after_seq: i64, through_seq: i64) -> Result<Vec<Value>> {
    if through_seq <= after_seq {
        return Ok(vec![]);
    }
    let mut out: Vec<Value> = vec![];
    let mut before_seq: Option<i64> = None;
    for _ in 0..50 {
        let mut req = json!({ "address": address(session_id), "throughSeq": through_seq, "maxMessages": 40 });
        if let Some(b) = before_seq {
            req["beforeSeq"] = json!(b);
        }
        let page = call("session/page", json!({ "request": req }))?;
        let events: Vec<Value> =
            page["records"].as_array().map(|rs| rs.iter().filter(|r| r["type"] == "event").map(|r| r["event"].clone()).collect()).unwrap_or_default();
        out.extend(events.iter().filter(|e| e["seq"].as_i64().unwrap_or(0) > after_seq).cloned());
        let oldest = events.iter().filter_map(|e| e["seq"].as_i64()).min();
        let has_more = page["hasMore"].as_bool().unwrap_or(false);
        match oldest {
            Some(o) if has_more && o > after_seq + 1 => before_seq = Some(o),
            _ => break,
        }
    }
    out.sort_by_key(|e| e["seq"].as_i64().unwrap_or(0));
    out.dedup_by_key(|e| e["seq"].as_i64().unwrap_or(0));
    Ok(out)
}

fn tool_result_text(content: &[Value]) -> String {
    let mut parts: Vec<Value> = vec![];
    for part in content {
        if part["type"] == "tool-result" {
            if let Some(inner) = part["content"].as_array() {
                parts.extend(inner.iter().cloned());
            }
        } else {
            parts.push(part.clone());
        }
    }
    parts
        .iter()
        .map(|p| match p["type"].as_str() {
            Some("text") => p["text"].as_str().unwrap_or("").to_string(),
            Some("image") => "[image]".to_string(),
            _ => String::new(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// State carried across `translate` calls.
#[derive(Default)]
pub struct TranslateCtx {
    pub last_usage: Option<Value>,
    pub turn_texts: Vec<String>,
}

/// Translate durable web Session events into the headless `--json` event
/// vocabulary, so job state, logs and results work the same for both backends.
pub fn translate(ev: &Value, ctx: &mut TranslateCtx) -> Vec<Value> {
    let d = &ev["data"];
    match ev["type"].as_str().unwrap_or("") {
        "turn/start" => {
            ctx.turn_texts.clear();
            vec![json!({ "type": "status", "phase": "turn_start", "turn": d["turn"] })]
        }
        "step/start" => vec![json!({ "type": "status", "phase": "step_start", "turn": d["turn"], "step": d["step"] })],
        "assistant/message" => {
            ctx.last_usage = d.get("usage").filter(|u| !u.is_null()).cloned();
            let mut out = vec![];
            for part in d["message"]["content"].as_array().cloned().unwrap_or_default() {
                let text = part["text"].as_str().unwrap_or("");
                if part["type"] == "reasoning" && !text.is_empty() {
                    out.push(json!({ "type": "thinking", "text": text }));
                }
                if part["type"] == "text" && !text.is_empty() {
                    out.push(json!({ "type": "text", "text": text }));
                    ctx.turn_texts.push(text.to_string());
                }
            }
            out
        }
        "tool/call" => {
            let raw = d["arguments"].clone();
            let input = match raw.as_str().and_then(|s| serde_json::from_str::<Value>(s).ok()) {
                Some(parsed) => parsed,
                None => raw,
            };
            vec![json!({ "type": "tool_call", "callId": d["callId"], "tool": d["name"], "input": input })]
        }
        "tool/result" => {
            let parts = d["message"]["content"].as_array().cloned().unwrap_or_default();
            let failed = parts.iter().any(|p| p["isError"].as_bool().unwrap_or(false) || !p["error"].is_null() && p["error"] != false);
            let call_id = if !d["message"]["source"]["callId"].is_null() {
                d["message"]["source"]["callId"].clone()
            } else {
                parts.first().map(|p| p["toolCallId"].clone()).unwrap_or(Value::Null)
            };
            vec![json!({ "type": "tool_result", "callId": call_id, "status": if failed { "failed" } else { "completed" }, "result": tool_result_text(&parts) })]
        }
        "step/end" => {
            let mut ev = json!({ "type": "status", "phase": "step_end", "turn": d["turn"], "step": d["step"] });
            if let Some(u) = &ctx.last_usage {
                ev["usage"] = u.clone();
            }
            vec![ev]
        }
        "turn/end" => vec![
            json!({ "type": "status", "phase": "turn_end", "turn": d["turn"], "reason": d["reason"] }),
            json!({ "type": "final", "text": ctx.turn_texts.last().cloned().unwrap_or_default() }),
        ],
        _ => vec![],
    }
}

/// Seq of the user message carrying our prompt request id, if committed.
pub fn prompt_seq(events: &[Value], request_id: &str) -> Option<i64> {
    events.iter().find(|e| e["type"] == "user/message" && e["data"]["source"]["rpcId"].as_str() == Some(request_id)).and_then(|e| e["seq"].as_i64())
}

pub struct ServerInfo {
    pub url: String,
    pub pid: u32,
    pub log: String,
}

fn find_token_url(text: &str) -> Option<String> {
    let idx = text.find("dsh web: ")? + "dsh web: ".len();
    let rest = &text[idx..];
    let end = rest.find(|c: char| c.is_whitespace() || c == ')').unwrap_or(rest.len());
    let candidate = &rest[..end];
    (candidate.starts_with("http") && candidate.contains("token=")).then(|| candidate.to_string())
}

/// Start `dsh web` detached, wait for the token URL it prints, remember it and
/// exchange it for the cookie. The server keeps running after go-dsh exits.
pub fn start_server(port: u16, timeout_ms: u64) -> Result<ServerInfo> {
    let c = dsh_command(&["web", "--port", &port.to_string(), "--no-open"])?;
    fs::create_dir_all(logs_dir())?;
    let log_file = logs_dir().join("dsh-web.log");
    let out = fs::File::create(&log_file)?;
    let err = out.try_clone()?;
    let mut cmd = Command::new(&c.cmd);
    cmd.args(&c.args).envs(c.env.iter().cloned()).stdin(Stdio::null()).stdout(out).stderr(err);
    if let Some(h) = home_dir() {
        cmd.current_dir(h);
    }
    detach_command(&mut cmd, true);
    let mut child = cmd.spawn().map_err(|e| DshgoError::new(format!("cannot start dsh web: {e}"), 5))?;
    let pid = child.id();
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        sleep_ms(500);
        let text = fs::read_to_string(&log_file).unwrap_or_default();
        if let Some(url) = find_token_url(&text) {
            let origin = origin_of(&url).unwrap_or_default();
            std::env::set_var("DSHGO_WEB_URL", &origin);
            save_token_url(&url)?;
            exchange_token(None);
            return Ok(ServerInfo { url: origin, pid, log: log_file.display().to_string() });
        }
        if !matches!(child.try_wait(), Ok(None)) {
            let tail: Vec<&str> = text.trim().lines().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect();
            return Err(DshgoError::new(format!("dsh web exited early: {}", tail.join(" ")), 5).hint(format!("full log: {}", log_file.display())));
        }
    }
    Err(DshgoError::new(format!("dsh web printed no URL within {}s", timeout_ms / 1000), 5).hint(format!("log: {}", log_file.display())))
}

#[cfg(test)]
mod tests {
    use super::parse_cursor_from_error;

    #[test]
    fn cursor_is_read_from_both_error_wordings() {
        assert_eq!(parse_cursor_from_error("DSH web session/page: session history cut 2147483647 exceeds 3"), Some(3));
        assert_eq!(parse_cursor_from_error("through seq 2147483647 is past cursor 41"), Some(41));
        assert_eq!(parse_cursor_from_error("something else entirely"), None);
    }
}

/// Port of the configured web UI address.
pub fn base_port() -> Option<u16> {
    Url::parse(&base_url()).ok().and_then(|u| u.port_or_known_default())
}

/// True when the configured UI address points at this machine.
pub fn base_is_local() -> bool {
    match Url::parse(&base_url()).ok().and_then(|u| u.host_str().map(String::from)) {
        Some(h) => matches!(h.as_str(), "127.0.0.1" | "localhost" | "[::1]" | "::1" | "0.0.0.0"),
        None => false,
    }
}

/// Pids of the processes listening on a local TCP port.
pub fn listener_pids(port: u16) -> Vec<u32> {
    let mut pids = listener_pids_os(port);
    pids.retain(|p| *p > 4);
    pids.sort_unstable();
    pids.dedup();
    pids
}

/// Parse `netstat -ano` output. The state column is localized ("LISTENING", "ABHÖREN", ...), so a
/// listener is recognized by its foreign address (`0.0.0.0:0`, `[::]:0`) instead.
fn parse_netstat(text: &str, port: u16) -> Vec<u32> {
    let suffix = format!(":{port}");
    text.lines()
        .filter_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() < 5 || !cols[0].eq_ignore_ascii_case("TCP") {
                return None;
            }
            let (local, foreign) = (cols[1], cols[2]);
            (local.ends_with(&suffix) && foreign.ends_with(":0")).then(|| cols[cols.len() - 1].parse().ok()).flatten()
        })
        .collect()
}

#[cfg(windows)]
fn listener_pids_os(port: u16) -> Vec<u32> {
    use std::os::windows::process::CommandExt;
    let out = Command::new("netstat").args(["-ano", "-p", "TCP"]).creation_flags(0x0800_0000).output();
    match out {
        Ok(o) => parse_netstat(&String::from_utf8_lossy(&o.stdout), port),
        Err(_) => vec![],
    }
}

#[cfg(unix)]
fn listener_pids_os(port: u16) -> Vec<u32> {
    let lsof = Command::new("lsof").args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-t"]).output();
    if let Ok(o) = lsof {
        let pids: Vec<u32> = String::from_utf8_lossy(&o.stdout).split_whitespace().filter_map(|p| p.parse().ok()).collect();
        if !pids.is_empty() {
            return pids;
        }
    }
    // Fallback for systems without lsof: `ss -ltnp` prints users:(("name",pid=123,fd=4)).
    let ss = Command::new("ss").args(["-ltnpH", &format!("sport = :{port}")]).output();
    match ss {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .split("pid=")
            .skip(1)
            .filter_map(|rest| rest.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().ok())
            .collect(),
        Err(_) => vec![],
    }
}

#[cfg(test)]
mod listener_tests {
    use super::*;

    #[test]
    fn netstat_listeners_are_found_without_the_localized_state() {
        let sample = "  Proto  Lokale Adresse  Remoteadresse  Status  PID\n  TCP    127.0.0.1:3080   0.0.0.0:0   ABHÖREN   43516\n  TCP    127.0.0.1:3080   127.0.0.1:50000   HERGESTELLT   99\n  TCP    [::]:3080   [::]:0   ABHÖREN   43516\n  TCP    0.0.0.0:8080   0.0.0.0:0   ABHÖREN   7\n";
        assert_eq!(parse_netstat(sample, 3080), vec![43516, 43516]);
        assert!(parse_netstat(sample, 9999).is_empty());
    }

    #[test]
    fn the_own_process_is_found_as_listener() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        assert!(listener_pids(port).contains(&std::process::id()), "port {port}");
    }
}
