//! Fold the `dsh --json` event stream into job progress and render it as a
//! compact timeline for `go-dsh logs`.

use serde_json::Value;

use crate::util::excerpt;

#[derive(Debug, Clone, Default)]
pub struct EventState {
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub steps: u64,
    pub tool_calls: u64,
    pub last_tool: Option<String>,
    pub last_text: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub final_text: Option<String>,
    pub turn_end: Option<String>,
    pub error: Option<String>,
}

pub fn parse_lines(text: &str) -> Vec<Value> {
    text.lines().filter(|l| !l.trim().is_empty()).filter_map(|l| serde_json::from_str(l).ok()).collect()
}

fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(String::from)
}

fn u(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(|x| x.as_u64()).unwrap_or(0)
}

/// Error code and message of a turn-end reason. dsh nests them as `reason.error.{code,message}`;
/// older builds and the web translation put them directly on `reason`.
fn reason_details(reason: &Value) -> Vec<String> {
    let nested = reason.get("error").filter(|e| e.is_object());
    [s(reason, "code"), s(reason, "message"), nested.and_then(|e| s(e, "code")), nested.and_then(|e| s(e, "message"))]
        .into_iter()
        .flatten()
        .filter(|p| !p.is_empty())
        .collect()
}

pub fn fold(state: &mut EventState, ev: &Value) {
    match ev.get("type").and_then(|t| t.as_str()) {
        Some("session") => {
            if let Some(id) = s(ev, "sessionId") {
                state.session_id = Some(id);
            }
            if let Some(cwd) = s(ev, "cwd") {
                state.cwd = Some(cwd);
            }
        }
        Some("status") => match ev.get("phase").and_then(|p| p.as_str()) {
            Some("step_end") => {
                state.steps += 1;
                if let Some(usage) = ev.get("usage").filter(|u| u.is_object()) {
                    state.input_tokens += u(usage, "inputTokens");
                    state.output_tokens += u(usage, "outputTokens");
                    state.cache_read_tokens += u(usage, "cacheReadTokens");
                }
            }
            Some("turn_end") => {
                let reason = ev.get("reason");
                let kind = reason.and_then(|r| s(r, "kind")).unwrap_or_else(|| "unknown".into());
                if kind != "completed" {
                    let parts = reason.map(reason_details).unwrap_or_default();
                    state.error = Some(if parts.is_empty() { kind.clone() } else { parts.join(": ") });
                }
                state.turn_end = Some(kind);
            }
            _ => {}
        },
        Some("tool_call") => {
            state.tool_calls += 1;
            state.last_tool = s(ev, "tool");
        }
        Some("text") => {
            if let Some(t) = s(ev, "text").filter(|t| !t.is_empty()) {
                state.last_text = Some(t);
            }
        }
        Some("final") => state.final_text = Some(s(ev, "text").unwrap_or_default()),
        Some("error") => state.error = Some(s(ev, "message").unwrap_or_else(|| "unknown error".into())),
        _ => {}
    }
}

fn join_strs(v: &Value, sep: &str) -> String {
    match v {
        Value::Array(items) => items.iter().map(|i| i.as_str().map(String::from).unwrap_or_else(|| i.to_string())).collect::<Vec<_>>().join(sep),
        Value::String(t) => t.clone(),
        other => other.to_string(),
    }
}

/// One readable line describing what a tool call does.
pub fn describe_input(input: &Value) -> String {
    match input {
        Value::Null => String::new(),
        Value::Object(map) => {
            let truthy = |k: &str| map.get(k).filter(|v| !matches!(v, Value::Null | Value::Bool(false)) && v.as_str() != Some(""));
            if let Some(v) = truthy("command") {
                return join_strs(v, " ");
            }
            if let Some(v) = truthy("queries") {
                return join_strs(v, " | ");
            }
            if let Some(v) = truthy("query") {
                return join_strs(v, " ");
            }
            if let Some(v) = truthy("url") {
                return join_strs(v, " ");
            }
            if let Some(v) = truthy("urls") {
                return join_strs(v, " ");
            }
            if let Some(v) = truthy("file_path").or_else(|| truthy("path")) {
                return join_strs(v, " ");
            }
            if let Some(v) = truthy("pattern") {
                return join_strs(v, " ");
            }
            if let Some(v) = truthy("prompt") {
                return join_strs(v, " ");
            }
            input.to_string()
        }
        Value::String(t) => t.clone(),
        other => other.to_string(),
    }
}

pub fn timeline(events: &[Value], full: bool) -> Vec<String> {
    let mut lines = Vec::new();
    let short = if full { 600 } else { 140 };
    for ev in events {
        match ev.get("type").and_then(|t| t.as_str()) {
            Some("session") => lines.push(format!("session: {}", s(ev, "sessionId").unwrap_or_default())),
            Some("thinking") => {
                if full {
                    if let Some(t) = s(ev, "text").filter(|t| !t.is_empty()) {
                        lines.push(format!("thinking: {}", excerpt(&t, short)));
                    }
                }
            }
            Some("tool_call") => {
                let input = ev.get("input").cloned().unwrap_or(Value::Null);
                lines.push(format!("→ {}: {}", s(ev, "tool").unwrap_or_default(), excerpt(&describe_input(&input), short)));
            }
            Some("tool_result") => {
                let ok = ev.get("status").and_then(|x| x.as_str()) == Some("completed");
                let result = match ev.get("result") {
                    Some(Value::String(t)) => t.clone(),
                    Some(other) => other.to_string(),
                    None => String::new(),
                };
                let body = excerpt(&result, if full { 400 } else { 100 });
                if ok {
                    lines.push(format!("  ✓ {body}"));
                } else {
                    lines.push(format!("  ✗ {}: {body}", s(ev, "status").unwrap_or_default()));
                }
            }
            Some("text") => {
                if let Some(t) = s(ev, "text").filter(|t| !t.is_empty()) {
                    lines.push(format!("text: {}", if full { t } else { excerpt(&t, 200) }));
                }
            }
            Some("status") => {
                if ev.get("phase").and_then(|p| p.as_str()) == Some("turn_end") {
                    let r = ev.get("reason").cloned().unwrap_or(Value::Null);
                    let mut parts: Vec<String> = s(&r, "kind").into_iter().filter(|p| !p.is_empty()).collect();
                    parts.extend(reason_details(&r));
                    lines.push(format!("turn_end: {}", parts.join(" / ")));
                }
            }
            Some("error") => lines.push(format!("error: {}", s(ev, "message").unwrap_or_default())),
            Some("final") => {
                let t = s(ev, "text").unwrap_or_default();
                lines.push(format!("final: {}", if full { t } else { excerpt(&t, 300) }));
            }
            _ => {}
        }
    }
    lines
}
