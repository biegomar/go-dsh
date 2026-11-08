//! Plain-JSON state for jobs and batches under `DSHGO_HOME`. Every process reads
//! and writes these files directly; there is no daemon.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::paths::{batch_dir, batches_dir, job_dir, jobs_dir};
use crate::util::{new_id, now_iso, pid_alive, read_json, write_json_atomic, DshgoError, Result};

pub fn is_terminal(status: &str) -> bool {
    matches!(status, "done" | "failed" | "cancelled" | "timeout" | "lost")
}

pub fn is_active(status: &str) -> bool {
    matches!(status, "queued" | "running")
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Progress {
    pub steps: u64,
    pub tool_calls: u64,
    pub last_tool: Option<String>,
    pub last_activity_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WebInfo {
    pub workspace_id: Option<String>,
    pub workspace_title: Option<String>,
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Job {
    pub id: String,
    pub name: Option<String>,
    pub title: Option<String>,
    pub batch: Option<String>,
    pub index: Option<u32>,
    pub template: String,
    pub prompt: String,
    pub cwd: String,
    pub out_dir: Option<String>,
    pub backend: Option<String>,
    pub web: Option<WebInfo>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub timeout_sec: u64,
    pub parent: Option<String>,
    pub resume: Option<String>,
    pub status: String,
    pub owner_pid: Option<u32>,
    pub dsh_pid: Option<u32>,
    pub session_id: Option<String>,
    pub profile: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub exit_code: Option<i64>,
    pub turn_end: Option<String>,
    pub error: Option<String>,
    pub progress: Progress,
    pub usage: Usage,
}

impl Default for Job {
    fn default() -> Self {
        Job {
            id: String::new(),
            name: None,
            title: None,
            batch: None,
            index: None,
            template: "raw".into(),
            prompt: String::new(),
            cwd: std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default(),
            out_dir: None,
            backend: None,
            web: None,
            model: None,
            effort: None,
            timeout_sec: 1800,
            parent: None,
            resume: None,
            status: "queued".into(),
            owner_pid: None,
            dsh_pid: None,
            session_id: None,
            profile: None,
            created_at: now_iso(),
            started_at: None,
            ended_at: None,
            exit_code: None,
            turn_end: None,
            error: None,
            progress: Progress::default(),
            usage: Usage::default(),
        }
    }
}

impl Job {
    /// Folder the job writes its files into.
    pub fn out(&self) -> &str {
        self.out_dir.as_deref().unwrap_or(&self.cwd)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Batch {
    pub id: String,
    pub name: Option<String>,
    pub root: String,
    pub parallel: u32,
    pub template: String,
    pub jobs: Vec<String>,
    pub owner_pid: Option<u32>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
}

impl Default for Batch {
    fn default() -> Self {
        Batch {
            id: String::new(),
            name: None,
            root: std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default(),
            parallel: 4,
            template: "raw".into(),
            jobs: vec![],
            owner_pid: None,
            created_at: now_iso(),
            started_at: None,
            ended_at: None,
        }
    }
}

fn job_file(id: &str) -> PathBuf {
    job_dir(id).join("job.json")
}
fn batch_file(id: &str) -> PathBuf {
    batch_dir(id).join("batch.json")
}

/// A queued or running job whose owner process vanished is reported as lost.
fn with_derived_status(mut job: Job) -> Job {
    if is_active(&job.status) {
        if let Some(pid) = job.owner_pid {
            if !pid_alive(pid) {
                job.status = "lost".into();
                if job.error.is_none() {
                    job.error = Some("owner process exited before the job finished".into());
                }
            }
        }
    }
    job
}

/// Create a job; `init` fills the caller-specific fields.
pub fn create_job(init: impl FnOnce(&mut Job)) -> Result<Job> {
    let id = new_id("j");
    fs::create_dir_all(job_dir(&id))?;
    let mut job = Job { id: id.clone(), ..Job::default() };
    init(&mut job);
    write_json_atomic(&job_file(&id), &job)?;
    Ok(job)
}

/// Raw job as stored, without lost detection.
pub fn read_job_raw(id: &str) -> Result<Option<Job>> {
    read_json(&job_file(id))
}

pub fn read_job(id: &str) -> Result<Option<Job>> {
    Ok(read_job_raw(id)?.map(with_derived_status))
}

/// Read a job that must exist (derived status applied).
pub fn require_job(id: &str) -> Result<Job> {
    read_job(id)?.ok_or_else(|| DshgoError::new(format!("Job {id} not found"), 3))
}

pub fn update_job(id: &str, patch: impl FnOnce(&mut Job)) -> Result<Job> {
    let mut job = read_job_raw(id)?.ok_or_else(|| DshgoError::new(format!("Job {id} not found"), 3))?;
    patch(&mut job);
    write_json_atomic(&job_file(id), &job)?;
    Ok(job)
}

fn list_ids(dir: &PathBuf) -> Vec<String> {
    let mut ids: Vec<String> = match fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).filter_map(|e| e.file_name().into_string().ok()).filter(|n| !n.starts_with('.')).collect(),
        Err(_) => vec![],
    };
    ids.sort();
    ids.reverse();
    ids
}

pub fn list_jobs(batch: Option<&str>, limit: Option<usize>, by_index: bool) -> Vec<Job> {
    let mut jobs: Vec<Job> = list_ids(&jobs_dir()).iter().filter_map(|id| read_job(id).ok().flatten()).collect();
    if let Some(b) = batch {
        jobs.retain(|j| j.batch.as_deref() == Some(b));
    }
    if by_index {
        jobs.sort_by_key(|j| j.index.unwrap_or(0));
    } else {
        jobs.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| b.id.cmp(&a.id)));
    }
    if let Some(l) = limit {
        jobs.truncate(l);
    }
    jobs
}

pub fn create_batch(init: impl FnOnce(&mut Batch)) -> Result<Batch> {
    let id = new_id("b");
    fs::create_dir_all(batch_dir(&id))?;
    let mut batch = Batch { id: id.clone(), ..Batch::default() };
    init(&mut batch);
    write_json_atomic(&batch_file(&id), &batch)?;
    Ok(batch)
}

pub fn read_batch(id: &str) -> Result<Option<Batch>> {
    read_json(&batch_file(id))
}

pub fn require_batch(id: &str) -> Result<Batch> {
    read_batch(id)?.ok_or_else(|| DshgoError::new(format!("Batch {id} not found"), 3))
}

pub fn update_batch(id: &str, patch: impl FnOnce(&mut Batch)) -> Result<Batch> {
    let mut batch = require_batch(id)?;
    patch(&mut batch);
    write_json_atomic(&batch_file(id), &batch)?;
    Ok(batch)
}

pub fn list_batches(limit: Option<usize>) -> Vec<Batch> {
    let mut batches: Vec<Batch> = list_ids(&batches_dir()).iter().filter_map(|id| read_batch(id).ok().flatten()).collect();
    batches.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| b.id.cmp(&a.id)));
    if let Some(l) = limit {
        batches.truncate(l);
    }
    batches
}

#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub status: String,
    pub counts: BTreeMap<String, usize>,
    pub total: usize,
}

/// Count job states and fold them into one batch status.
pub fn summarize_jobs(jobs: &[Job]) -> Summary {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for j in jobs {
        *counts.entry(j.status.clone()).or_default() += 1;
    }
    let c = |k: &str| counts.get(k).copied().unwrap_or(0);
    let n = jobs.len();
    let status = if c("running") + c("queued") > 0 {
        "running"
    } else if n > 0 && c("done") == n {
        "done"
    } else if n > 0 && c("cancelled") == n {
        "cancelled"
    } else if n == 0 {
        "empty"
    } else if c("done") > 0 {
        "partial"
    } else {
        "failed"
    };
    Summary { status: status.into(), counts, total: n }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    Job,
    Batch,
}

#[derive(Debug, Clone)]
pub struct Ref {
    pub kind: RefKind,
    pub id: String,
}

/// Resolve a user reference to a job or batch: exact id, unique id prefix,
/// `last`, `last-batch`, batch name or job name (newest wins).
pub fn resolve_ref(reference: Option<&str>) -> Result<Ref> {
    let reference = reference.filter(|r| !r.is_empty()).ok_or_else(|| DshgoError::new("Missing reference (job id, batch id, name, last or last-batch)", 2))?;
    let r = reference.strip_prefix('@').unwrap_or(reference);
    let job_ids = list_ids(&jobs_dir());
    let batch_ids = list_ids(&batches_dir());
    let job = |id: &String| Ref { kind: RefKind::Job, id: id.clone() };
    let batch = |id: &String| Ref { kind: RefKind::Batch, id: id.clone() };
    if r == "last" {
        return job_ids.first().map(job).ok_or_else(|| DshgoError::new("No jobs yet", 3));
    }
    if r == "last-batch" || r == "lastb" {
        return batch_ids.first().map(batch).ok_or_else(|| DshgoError::new("No batches yet", 3));
    }
    if job_ids.iter().any(|i| i == r) {
        return Ok(Ref { kind: RefKind::Job, id: r.into() });
    }
    if batch_ids.iter().any(|i| i == r) {
        return Ok(Ref { kind: RefKind::Batch, id: r.into() });
    }
    let mut prefixed: Vec<Ref> = job_ids.iter().filter(|i| i.starts_with(r)).map(job).collect();
    prefixed.extend(batch_ids.iter().filter(|i| i.starts_with(r)).map(batch));
    if prefixed.len() == 1 {
        return Ok(prefixed.remove(0));
    }
    if prefixed.len() > 1 {
        let list: Vec<String> = prefixed.iter().take(5).map(|p| p.id.clone()).collect();
        return Err(DshgoError::new(format!("Reference \"{reference}\" is ambiguous: {}", list.join(", ")), 2));
    }
    if let Some(b) = batch_ids.iter().filter_map(|id| read_batch(id).ok().flatten()).find(|b| b.name.as_deref() == Some(r)) {
        return Ok(Ref { kind: RefKind::Batch, id: b.id });
    }
    if let Some(j) = job_ids.iter().filter_map(|id| read_job_raw(id).ok().flatten()).find(|j| j.name.as_deref() == Some(r)) {
        return Ok(Ref { kind: RefKind::Job, id: j.id });
    }
    Err(DshgoError::new(format!("Nothing found for \"{reference}\""), 3).hint("go-dsh ls lists jobs, go-dsh ls --batches lists batches"))
}
