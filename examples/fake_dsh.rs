//! Stand-in for `dsh --profile concert --json [--session-id id] -`, used by the integration tests.
//! Behaviour is driven by markers in the task text:
//!   SLEEP <s>     wait s seconds before answering
//!   WRITE <file>  create <file> in the working directory
//!   FAIL          end the turn with an error and exit 1
use std::io::Read;

fn emit(v: serde_json::Value) {
    println!("{v}");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let resumed = args.iter().position(|a| a == "--session-id").and_then(|i| args.get(i + 1)).cloned();
    let session_id = resumed.clone().unwrap_or_else(|| format!("session-fake-{}", uuid::Uuid::new_v4()));
    let mut task = String::new();
    let _ = std::io::stdin().read_to_string(&mut task);
    // go-dsh prepends a two-line work-folder header plus a blank line.
    let body = match task.strip_prefix("Working folder for this task:") {
        Some(rest) => rest.splitn(3, '\n').nth(2).unwrap_or("").to_string(),
        None => task.clone(),
    };
    let first_line = body.trim().lines().next().unwrap_or("").to_string();
    let usage = serde_json::json!({ "inputTokens": 100, "outputTokens": 20, "cacheReadTokens": 50 });
    emit(serde_json::json!({ "type": "session", "sessionId": session_id, "cwd": std::env::current_dir().unwrap().display().to_string() }));
    emit(serde_json::json!({ "type": "status", "phase": "turn_start", "turn": 1 }));
    emit(serde_json::json!({ "type": "thinking", "text": "Fake thinking" }));
    emit(serde_json::json!({ "type": "tool_call", "callId": "c1", "tool": "bash", "input": { "command": "echo hallo" } }));
    emit(serde_json::json!({ "type": "tool_result", "callId": "c1", "status": "completed", "result": "hallo\n" }));
    emit(serde_json::json!({ "type": "status", "phase": "step_end", "turn": 1, "step": 1, "usage": usage }));
    if let Some(idx) = task.find("SLEEP ") {
        let n: String = task[idx + 6..].chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
        if let Ok(s) = n.parse::<f64>() {
            std::thread::sleep(std::time::Duration::from_millis((s * 1000.0) as u64));
        }
    }
    for part in task.split("WRITE ").skip(1) {
        let file: String = part.chars().take_while(|c| !c.is_whitespace()).collect();
        let _ = std::fs::write(&file, format!("written by fake-dsh: {first_line}\n"));
    }
    if task.split_whitespace().any(|w| w == "FAIL") {
        eprintln!("dsh: fake: Fake failure");
        emit(serde_json::json!({ "type": "status", "phase": "turn_end", "turn": 1, "reason": { "kind": "error", "code": "fake", "message": "Fake failure" } }));
        emit(serde_json::json!({ "type": "final", "text": "" }));
        std::process::exit(1);
    }
    let answer = format!("FAKE-RESULT: {first_line}{}", resumed.map(|r| format!(" (resumed {r})")).unwrap_or_default());
    emit(serde_json::json!({ "type": "text", "text": answer }));
    emit(serde_json::json!({ "type": "status", "phase": "step_end", "turn": 1, "step": 2, "usage": usage }));
    emit(serde_json::json!({ "type": "status", "phase": "turn_end", "turn": 1, "reason": { "kind": "completed" } }));
    emit(serde_json::json!({ "type": "final", "text": answer }));
}
