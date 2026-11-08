//! Runs one job: spawns headless dsh in the job's workspace (or drives the DSH
//! web UI), streams the event log to disk, keeps job.json current and maps the
//! outcome to a status.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::dsh::{model_selection, resolve_model, spawn_spec};
use crate::events::{fold, EventState};
use crate::paths::{home, job_dir, logs_dir};
use crate::store::{create_job, is_terminal, read_job_raw, update_job, Job, WebInfo};
use crate::util::{detach_command, force_kill, now_iso, pid_alive, sleep_ms, terminate, DshgoError, Result};
use crate::web::{self, TranslateCtx};

const KILL_GRACE: Duration = Duration::from_secs(5);
const PROGRESS_WRITE_MS: i64 = 1000;
const WEB_OUTAGE: Duration = Duration::from_secs(120);

fn web_poll_ms() -> u64 {
    std::env::var("DSHGO_WEB_POLL_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(2000)
}

/// Live dsh child pids of this process (job id -> pid), for signal forwarding.
fn active_children() -> &'static Mutex<HashMap<String, u32>> {
    static M: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn cancel_marker(id: &str) -> PathBuf {
    job_dir(id).join("cancel")
}

fn task_file(id: &str) -> PathBuf {
    job_dir(id).join("task.md")
}

/// Create a job and store the exact task text dsh will receive.
pub fn create_runnable_job(init: impl FnOnce(&mut Job), task_text: &str) -> Result<Job> {
    let job = create_job(init)?;
    fs::write(task_file(&job.id), task_text)?;
    Ok(job)
}

pub fn read_task(id: &str) -> String {
    fs::read_to_string(task_file(id)).unwrap_or_default()
}

fn stderr_tail(file: &PathBuf) -> String {
    let text = fs::read_to_string(file).unwrap_or_default();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty() && !l.contains("statectl failed")).collect();
    let tail = lines[lines.len().saturating_sub(6)..].join("\n");
    let n = tail.chars().count();
    tail.chars().skip(n.saturating_sub(800)).collect()
}

/// Request cancellation; the owning runner notices and records `cancelled`.
pub fn cancel_job(id: &str) -> Result<bool> {
    let job = read_job_raw(id)?.ok_or_else(|| DshgoError::new(format!("Job {id} not found"), 3))?;
    if is_terminal(&job.status) {
        return Ok(false);
    }
    fs::write(cancel_marker(id), now_iso())?;
    if job.status == "running" {
        if let Some(pid) = job.dsh_pid {
            terminate(pid);
        }
    }
    if !job.owner_pid.map(pid_alive).unwrap_or(false) {
        update_job(id, |j| {
            j.status = "cancelled".into();
            j.ended_at = Some(now_iso());
        })?;
    }
    Ok(true)
}

/// Run a job on its backend: `web` (inside the DSH UI server) or `headless`.
pub fn run_job(id: &str) -> Result<()> {
    let job = read_job_raw(id)?.ok_or_else(|| DshgoError::new(format!("Job {id} not found"), 3))?;
    if job.backend.as_deref() == Some("web") {
        run_web(id)
    } else {
        run_headless(id)
    }
}

fn apply_progress(j: &mut Job, state: &EventState) {
    j.progress.steps = state.steps;
    j.progress.tool_calls = state.tool_calls;
    j.progress.last_tool = state.last_tool.clone();
    j.progress.last_activity_at = Some(now_iso());
    j.usage.input_tokens = state.input_tokens;
    j.usage.output_tokens = state.output_tokens;
    j.usage.cache_read_tokens = state.cache_read_tokens;
}

fn run_headless(id: &str) -> Result<()> {
    let job = read_job_raw(id)?.ok_or_else(|| DshgoError::new(format!("Job {id} not found"), 3))?;
    if is_terminal(&job.status) {
        return Ok(());
    }
    if cancel_marker(id).exists() {
        update_job(id, |j| {
            j.status = "cancelled".into();
            j.ended_at = Some(now_iso());
        })?;
        return Ok(());
    }

    let dir = job_dir(id);
    let events_path = dir.join("events.jsonl");
    let stderr_path = dir.join("stderr.log");
    fs::create_dir_all(&job.cwd)?;
    if let Some(o) = &job.out_dir {
        fs::create_dir_all(o)?;
    }

    let spec = match spawn_spec(job.resume.as_deref(), job.model.as_deref(), job.effort.as_deref(), &dir) {
        Ok(s) => s,
        Err(e) => {
            update_job(id, |j| {
                j.status = "failed".into();
                j.error = Some(e.message.clone());
                j.ended_at = Some(now_iso());
            })?;
            return Ok(());
        }
    };

    let err_file = OpenOptions::new().create(true).append(true).open(&stderr_path)?;
    let mut cmd = Command::new(&spec.cmd);
    cmd.args(&spec.args).envs(spec.extra_env.iter().cloned()).current_dir(&job.cwd);
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::from(err_file));
    detach_command(&mut cmd, true);

    let mut spawn_failed = false;
    let mut child = match cmd.spawn() {
        Ok(c) => Some(c),
        Err(e) => {
            let mut f = OpenOptions::new().create(true).append(true).open(&stderr_path)?;
            let _ = writeln!(f, "go-dsh: spawn failed: {e}");
            spawn_failed = true;
            None
        }
    };

    let mut state = EventState::default();
    let mut exit_code: i64 = -1;
    let mut timed_out = false;

    if let Some(child) = child.as_mut() {
        let pid = child.id();
        active_children().lock().unwrap().insert(id.to_string(), pid);
        let profile = spec.profile.clone();
        update_job(id, |j| {
            j.status = "running".into();
            j.owner_pid = Some(std::process::id());
            j.dsh_pid = Some(pid);
            j.profile = Some(profile);
            j.started_at = Some(now_iso());
        })?;

        // Task goes through stdin from a thread so a large prompt cannot deadlock the pipes.
        let stdin = child.stdin.take();
        let task = read_task(id);
        let writer = std::thread::spawn(move || {
            if let Some(mut stdin) = stdin {
                let _ = stdin.write_all(task.as_bytes());
            }
        });

        let stdout = child.stdout.take();
        let job_id = id.to_string();
        let events_path2 = events_path.clone();
        let reader = std::thread::spawn(move || {
            let mut state = EventState::default();
            let Some(stdout) = stdout else { return state };
            let mut events_file = OpenOptions::new().create(true).append(true).open(&events_path2).ok();
            let mut last_write: i64 = 0;
            let mut reader = BufReader::new(stdout);
            let mut buf: Vec<u8> = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                if let Some(f) = events_file.as_mut() {
                    let _ = f.write_all(&buf);
                }
                let line = String::from_utf8_lossy(&buf);
                if line.trim().is_empty() {
                    continue;
                }
                // Non-JSON output stays in events.jsonl only.
                if let Ok(ev) = serde_json::from_str::<Value>(line.trim_end()) {
                    fold(&mut state, &ev);
                    let force = matches!(ev["type"].as_str(), Some("session") | Some("final"));
                    let now = crate::util::now_ms();
                    if force || now - last_write >= PROGRESS_WRITE_MS {
                        last_write = now;
                        let snapshot = state.clone();
                        let _ = update_job(&job_id, |j| {
                            if let Some(s) = &snapshot.session_id {
                                j.session_id = Some(s.clone());
                            }
                            apply_progress(j, &snapshot);
                        });
                    }
                }
            }
            state
        });

        let started = Instant::now();
        let timeout = Duration::from_secs(job.timeout_sec);
        let mut kill_at: Option<Instant> = None;
        let mut stop_sent = false;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    exit_code = status.code().map(|c| c as i64).unwrap_or(128);
                    break;
                }
                Ok(None) => {}
                Err(_) => {
                    exit_code = -1;
                    break;
                }
            }
            if !stop_sent {
                if started.elapsed() > timeout {
                    timed_out = true;
                    stop_sent = true;
                } else if cancel_marker(id).exists() {
                    stop_sent = true;
                }
                if stop_sent {
                    terminate(pid);
                    kill_at = Some(Instant::now() + KILL_GRACE);
                }
            }
            if let Some(k) = kill_at {
                if Instant::now() >= k {
                    force_kill(pid);
                    kill_at = None;
                }
            }
            sleep_ms(200);
        }
        let _ = writer.join();
        state = reader.join().unwrap_or_default();
        active_children().lock().unwrap().remove(id);
    }

    if let Some(text) = &state.final_text {
        fs::write(dir.join("result.md"), text)?;
    }

    let cancelled = cancel_marker(id).exists();
    let status = if cancelled {
        "cancelled"
    } else if timed_out {
        "timeout"
    } else if exit_code == 0 && state.turn_end.as_deref() == Some("completed") {
        "done"
    } else {
        "failed"
    };
    let error = match status {
        "timeout" => Some(format!("timed out after {}s", job.timeout_sec)),
        "failed" => Some(
            state
                .error
                .clone()
                .filter(|e| !e.is_empty())
                .or_else(|| Some(stderr_tail(&stderr_path)).filter(|t| !t.is_empty()))
                .unwrap_or_else(|| format!("dsh exited with code {exit_code}")),
        ),
        _ => None,
    };
    let _ = spawn_failed;
    update_job(id, |j| {
        j.status = status.into();
        j.exit_code = Some(exit_code);
        j.turn_end = state.turn_end.clone();
        j.error = error.clone();
        if let Some(s) = &state.session_id {
            j.session_id = Some(s.clone());
        }
        j.ended_at = Some(now_iso());
        apply_progress(j, &state);
    })?;
    Ok(())
}

fn run_web(id: &str) -> Result<()> {
    let job = read_job_raw(id)?.ok_or_else(|| DshgoError::new(format!("Job {id} not found"), 3))?;
    if is_terminal(&job.status) {
        return Ok(());
    }
    if cancel_marker(id).exists() {
        update_job(id, |j| {
            j.status = "cancelled".into();
            j.ended_at = Some(now_iso());
        })?;
        return Ok(());
    }
    let dir = job_dir(id);
    let events_path = dir.join("events.jsonl");
    fs::create_dir_all(&job.cwd)?;
    if let Some(o) = &job.out_dir {
        fs::create_dir_all(o)?;
    }
    update_job(id, |j| {
        j.status = "running".into();
        j.owner_pid = Some(std::process::id());
        j.started_at = Some(now_iso());
    })?;

    let mut state = EventState::default();
    let mut ctx = TranslateCtx::default();
    let mut last_write: i64 = 0;
    let mut record = |events: Vec<Value>, state: &mut EventState| {
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&events_path) {
            for ev in &events {
                let _ = writeln!(f, "{ev}");
            }
        }
        for ev in &events {
            fold(state, ev);
        }
        let now = crate::util::now_ms();
        if !events.is_empty() && now - last_write >= PROGRESS_WRITE_MS {
            last_write = now;
            let snapshot = state.clone();
            let _ = update_job(id, |j| apply_progress(j, &snapshot));
        }
    };

    let mut cancelled = false;
    let mut timed_out = false;
    let mut extra_error: Option<String> = None;
    let mut session_id = job.resume.clone();

    let outcome: Result<()> = (|| {
        let sid: String = match &session_id {
            None => {
                let workspace = web::ensure_workspace(&job.cwd)?;
                let ws_id = workspace["workspaceId"].as_str().unwrap_or_default().to_string();
                let ws_title = workspace["title"].as_str().map(String::from);
                let created = web::create_session(&ws_id)?;
                let sid = created["sessionId"].as_str().unwrap_or_default().to_string();
                let sid2 = sid.clone();
                update_job(id, |j| {
                    j.session_id = Some(sid2);
                    j.web = Some(WebInfo { workspace_id: Some(ws_id), workspace_title: ws_title, base_url: Some(web::base_url()) });
                })?;
                if let Some(title) = &job.title {
                    // The title is cosmetic; the generated title stays on failure.
                    let _ = web::rename_session(&sid, title);
                }
                sid
            }
            Some(sid) => {
                let sid2 = sid.clone();
                update_job(id, |j| {
                    j.session_id = Some(sid2);
                    let mut w = j.web.clone().unwrap_or_default();
                    if w.base_url.is_none() {
                        w.base_url = Some(web::base_url());
                    }
                    j.web = Some(w);
                })?;
                sid.clone()
            }
        };
        session_id = Some(sid.clone());
        if job.model.is_some() || job.effort.is_some() {
            let _ = resolve_model(job.model.as_deref());
            let sel = model_selection(job.model.as_deref(), job.effort.as_deref())?;
            web::select_model(&sid, &sel)?;
        }
        record(vec![json!({ "type": "session", "sessionId": sid, "cwd": job.cwd })], &mut state);
        let mut last_seq = web::cursor(&sid)?;
        let request_id = web::prompt(&sid, &read_task(id))?;
        let deadline = Instant::now() + Duration::from_secs(job.timeout_sec);
        let mut my_seq: Option<i64> = None;
        let mut outage_since: Option<Instant> = None;
        let mut stop_requested_at: Option<Instant> = None;
        loop {
            sleep_ms(web_poll_ms());
            if stop_requested_at.is_none() && (cancel_marker(id).exists() || Instant::now() > deadline) {
                if cancel_marker(id).exists() {
                    cancelled = true;
                } else {
                    timed_out = true;
                }
                stop_requested_at = Some(Instant::now());
                // The turn may already be over; the poll below settles it.
                let _ = web::cancel_session(&sid);
            }
            if stop_requested_at.map(|t| t.elapsed() > Duration::from_secs(15)).unwrap_or(false) {
                break;
            }
            let events = match web::cursor(&sid).and_then(|cur| web::events_since(&sid, last_seq, cur).map(|e| (cur, e))) {
                Ok((cur, events)) => {
                    last_seq = cur;
                    outage_since = None;
                    events
                }
                Err(e) => {
                    let since = *outage_since.get_or_insert_with(Instant::now);
                    if since.elapsed() > WEB_OUTAGE {
                        return Err(DshgoError::new(format!("DSH web UI unreachable for 2 minutes: {}", e.message), 1));
                    }
                    continue;
                }
            };
            if my_seq.is_none() {
                my_seq = web::prompt_seq(&events, &request_id);
                if my_seq.is_none() {
                    continue;
                }
            }
            let min = my_seq.unwrap_or(0);
            let ours: Vec<&Value> = events.iter().filter(|e| e["seq"].as_i64().unwrap_or(0) >= min).collect();
            let translated: Vec<Value> = ours.iter().flat_map(|e| web::translate(e, &mut ctx)).collect();
            record(translated, &mut state);
            if ours.iter().any(|e| e["type"] == "turn/end") {
                break;
            }
        }
        Ok(())
    })();
    if let Err(e) = outcome {
        extra_error = Some(e.message);
    }

    if let Some(text) = &state.final_text {
        fs::write(dir.join("result.md"), text)?;
    }
    let status = if cancelled {
        "cancelled"
    } else if timed_out {
        "timeout"
    } else if state.turn_end.as_deref() == Some("completed") {
        "done"
    } else {
        "failed"
    };
    let error = match status {
        "timeout" => Some(format!("timed out after {}s", job.timeout_sec)),
        "failed" => Some(
            extra_error
                .filter(|e| !e.is_empty())
                .or_else(|| state.error.clone().filter(|e| !e.is_empty()))
                .unwrap_or_else(|| "turn ended without completing".into()),
        ),
        _ => None,
    };
    update_job(id, |j| {
        j.status = status.into();
        j.exit_code = Some(if status == "done" { 0 } else { 1 });
        j.turn_end = state.turn_end.clone();
        j.error = error.clone();
        if session_id.is_some() {
            j.session_id = session_id.clone();
        }
        j.ended_at = Some(now_iso());
        apply_progress(j, &state);
    })?;
    Ok(())
}

/// Forward termination to running dsh children and record them as cancelled.
pub fn install_signal_forwarding() {
    let _ = ctrlc::set_handler(|| {
        let children: Vec<(String, u32)> = active_children().lock().map(|m| m.iter().map(|(k, v)| (k.clone(), *v)).collect()).unwrap_or_default();
        for (id, pid) in &children {
            let _ = fs::write(cancel_marker(id), format!("{} signal", now_iso()));
            terminate(*pid);
        }
        if !children.is_empty() {
            sleep_ms(1500);
        }
        std::process::exit(130);
    });
}

/// Re-invoke go-dsh detached (own session) for `__run-job` or `__run-batch`.
pub fn spawn_detached(args: &[&str]) -> Result<u32> {
    fs::create_dir_all(logs_dir())?;
    let log_name = args.get(1).copied().unwrap_or("runner");
    let log = OpenOptions::new().create(true).append(true).open(logs_dir().join(format!("{log_name}.log")))?;
    let log2 = log.try_clone()?;
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.args(args).stdin(Stdio::null()).stdout(log).stderr(log2).current_dir(home());
    detach_command(&mut cmd, false);
    let child = cmd.spawn()?;
    Ok(child.id())
}
