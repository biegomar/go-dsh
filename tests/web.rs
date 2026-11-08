//! The DSH web backend against a tiny in-process fake of `dsh web`.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

#[derive(Default)]
struct State {
    events: Vec<Value>,
    calls: Vec<String>,
    title: Option<String>,
}

fn read_request(stream: &mut TcpStream) -> Option<(String, String, String)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut tmp).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let len = head.lines().find_map(|l| l.to_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0))).unwrap_or(0);
    while buf.len() < header_end + len {
        let n = stream.read(&mut tmp).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body = String::from_utf8_lossy(&buf[header_end..]).to_string();
    let first = head.lines().next()?.to_string();
    let mut parts = first.split_whitespace();
    Some((parts.next()?.to_string(), parts.next()?.to_string(), format!("{head}\n{body}")))
}

fn respond(stream: &mut TcpStream, status: &str, extra: &str, body: &str) {
    let _ = write!(stream, "HTTP/1.1 {status}\r\n{extra}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
}

fn ok(v: Value) -> String {
    json!({ "type": "server-response", "result": { "ok": true, "value": v } }).to_string()
}

fn handle(state: &Arc<Mutex<State>>, method: &str, path: &str, raw: &str, stream: &mut TcpStream) {
    if method == "GET" && path.starts_with("/?token=") {
        return respond(stream, "303 See Other", "Set-Cookie: dsh_session=cookie123; Path=/; HttpOnly\r\nLocation: /\r\n", "");
    }
    if method == "GET" {
        let authed = raw.contains("dsh_session=cookie123");
        return respond(stream, if authed { "200 OK" } else { "401 Unauthorized" }, "", "{}");
    }
    let body = raw.rsplit_once('\n').map(|(_, b)| b).unwrap_or("");
    if !raw.contains("dsh_session=cookie123") {
        return respond(stream, "401 Unauthorized", "", "{}");
    }
    let req: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let args = &req["payload"]["args"]["request"];
    let name = path.trim_start_matches("/api/").to_string();
    let mut st = state.lock().unwrap();
    st.calls.push(name.clone());
    let reply = match name.as_str() {
        "workspace/create" => ok(json!({ "workspace": { "workspaceId": "ws-1", "title": "fake-ws" } })),
        "session/create" => ok(json!({ "sessionId": "sess-1" })),
        "session/rename" => {
            st.title = args["title"].as_str().map(String::from);
            ok(json!({}))
        }
        "session/selectModel" => ok(json!({})),
        "session/cancel" => ok(json!({})),
        "session/prompt" => {
            let request_id = args["requestId"].clone();
            let text = args["content"][0]["text"].as_str().unwrap_or("").to_string();
            let base = st.events.len() as i64;
            let answer = format!(
                "WEB-RESULT: {}",
                text.lines().find(|l| !l.trim().is_empty() && !l.starts_with("Working folder") && !l.starts_with("Put every")).unwrap_or("")
            );
            let evs = vec![
                json!({ "seq": base + 1, "type": "user/message", "data": { "source": { "rpcId": request_id } } }),
                json!({ "seq": base + 2, "type": "turn/start", "data": { "turn": 1 } }),
                json!({ "seq": base + 3, "type": "tool/call", "data": { "callId": "c1", "name": "bash", "arguments": "{\"command\":\"echo hi\"}" } }),
                json!({ "seq": base + 4, "type": "tool/result", "data": { "message": { "source": { "callId": "c1" }, "content": [{ "type": "text", "text": "hi" }] } } }),
                json!({ "seq": base + 5, "type": "assistant/message", "data": { "usage": { "inputTokens": 10, "outputTokens": 5, "cacheReadTokens": 0 }, "message": { "content": [{ "type": "text", "text": answer }] } } }),
                json!({ "seq": base + 6, "type": "step/end", "data": { "turn": 1, "step": 1 } }),
                json!({ "seq": base + 7, "type": "turn/end", "data": { "turn": 1, "reason": { "kind": "completed" } } }),
            ];
            st.events.extend(evs);
            ok(json!({}))
        }
        "session/page" => {
            let through = args["throughSeq"].as_i64().unwrap_or(0);
            let newest = st.events.len() as i64;
            if through > newest {
                json!({ "type": "server-response", "result": { "ok": false, "error": { "message": format!("session history cut {through} exceeds {newest}") } } }).to_string()
            } else {
                let records: Vec<Value> =
                    st.events.iter().filter(|e| e["seq"].as_i64().unwrap() <= through).map(|e| json!({ "type": "event", "event": e })).collect();
                ok(json!({ "records": records, "hasMore": false }))
            }
        }
        other => json!({ "type": "server-response", "result": { "ok": false, "error": { "message": format!("unknown method {other}") } } }).to_string(),
    };
    drop(st);
    respond(stream, "200 OK", "", &reply);
}

fn start_fake() -> (u16, Arc<Mutex<State>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let state = Arc::new(Mutex::new(State::default()));
    let st = Arc::clone(&state);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            if let Some((method, path, raw)) = read_request(&mut stream) {
                handle(&st, &method, &path, &raw, &mut stream);
            }
        }
    });
    (port, state)
}

#[test]
fn web_backend_creates_a_session_and_returns_the_answer() {
    let (port, state) = start_fake();
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let origin = format!("http://127.0.0.1:{port}");
    let out = Command::new(env!("CARGO_BIN_EXE_go-dsh"))
        .args(["run", "-b", "web", "-w", "--max-wait", "30s", "--json", "-C"])
        .arg(ws.path().join("w"))
        .args(["--name", "Web title", "Say hello via web"])
        .env("DSHGO_HOME", home.path())
        .env("DSHGO_WEB_URL", &origin)
        .env("DSHGO_WEB_TOKEN_URL", format!("{origin}/?token=abc"))
        .env("DSHGO_WEB_POLL_MS", "100")
        .env("DSHGO_DSH_CMD", "unused")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let j: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(j["job"]["status"], "done", "{stdout}");
    assert_eq!(j["job"]["backend"], "web");
    assert_eq!(j["job"]["web"]["workspaceTitle"], "fake-ws");
    assert!(j["result"].as_str().unwrap().starts_with("WEB-RESULT: Say hello via web"), "{stdout}");
    assert_eq!(j["job"]["progress"]["toolCalls"], 1);
    let st = state.lock().unwrap();
    assert_eq!(st.title.as_deref(), Some("Web title"));
    for call in ["workspace/create", "session/create", "session/rename", "session/prompt"] {
        assert!(st.calls.iter().any(|c| c == call), "missing {call}: {:?}", st.calls);
    }
    // The cookie was cached for later invocations.
    assert!(home.path().join("web-auth.json").exists());
}

#[test]
fn ui_stop_refuses_while_web_jobs_are_running() {
    // The fake UI lives in this test process, so a successful stop would kill the test itself:
    // only the guard is exercised here (the live stop was checked against a real `dsh web`).
    let (port, _state) = start_fake();
    let home = tempfile::tempdir().unwrap();
    let origin = format!("http://127.0.0.1:{port}");
    let job_dir = home.path().join("jobs").join("j-guard");
    std::fs::create_dir_all(&job_dir).unwrap();
    let job = json!({ "id": "j-guard", "status": "running", "backend": "web", "ownerPid": std::process::id() });
    std::fs::write(job_dir.join("job.json"), job.to_string()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_go-dsh"))
        .args(["ui", "stop", "--json"])
        .env("DSHGO_HOME", home.path())
        .env("DSHGO_WEB_URL", &origin)
        .env("DSHGO_WEB_TOKEN_URL", format!("{origin}/?token=abc"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{stdout}");
    let j: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(j["ok"], false);
    assert!(j["error"].as_str().unwrap().contains("j-guard"), "{stdout}");
    assert!(j["hint"].as_str().unwrap().contains("--force"), "{stdout}");
}

#[test]
fn ui_stop_reports_a_ui_that_is_not_running() {
    let home = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_go-dsh"))
        .args(["ui", "stop", "--json"])
        .env("DSHGO_HOME", home.path())
        .env("DSHGO_WEB_URL", "http://127.0.0.1:9")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let j: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(j["stopped"], false);
    assert_eq!(j["running"], false);
}
