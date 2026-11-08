//! Merge the outputs of a batch into one INDEX.md plus merged manifest and
//! source lists, so a calling agent reads one file instead of N folders.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};

use crate::paths::job_dir;
use crate::store::{list_jobs, require_batch, summarize_jobs, Job};
use crate::util::{excerpt, fmt_duration, now_ms, parse_iso_ms, read_json, Result};

const SKIP_DIRS: &[&str] = &["node_modules", ".git", "__pycache__"];

#[derive(Debug, Clone, Serialize)]
pub struct FileInfo {
    pub path: String,
    pub bytes: u64,
    pub mtime: String,
}

fn rel(base: &Path, p: &Path) -> String {
    p.strip_prefix(base).unwrap_or(p).display().to_string().replace('\\', "/")
}

/// Files below `dir` (relative paths), newest first, hidden entries skipped.
pub fn list_files(dir: &Path, since_ms: i64, limit: usize) -> Vec<FileInfo> {
    fn walk(base: &Path, current: &Path, since_ms: i64, out: &mut Vec<(SystemTime, FileInfo)>) {
        let Ok(rd) = fs::read_dir(current) else { return };
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            let full = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                walk(base, &full, since_ms, out);
            } else if ft.is_file() {
                let Ok(meta) = entry.metadata() else { continue };
                let Ok(mtime) = meta.modified() else { continue };
                let ms = DateTime::<Utc>::from(mtime).timestamp_millis();
                if ms >= since_ms {
                    let iso = DateTime::<Utc>::from(mtime).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                    out.push((mtime, FileInfo { path: rel(base, &full), bytes: meta.len(), mtime: iso }));
                }
            }
        }
    }
    let mut found = vec![];
    if dir.exists() {
        walk(dir, dir, since_ms, &mut found);
    }
    found.sort_by_key(|f| std::cmp::Reverse(f.0));
    found.into_iter().map(|(_, f)| f).take(limit).collect()
}

pub fn fmt_bytes(n: u64) -> String {
    let n = n as f64;
    if n < 1024.0 {
        format!("{n} B")
    } else if n < 1024f64.powi(2) {
        format!("{:.1} KB", n / 1024.0)
    } else if n < 1024f64.powi(3) {
        format!("{:.1} MB", n / 1024f64.powi(2))
    } else {
        format!("{:.2} GB", n / 1024f64.powi(3))
    }
}

pub fn job_result(id: &str) -> Option<String> {
    fs::read_to_string(job_dir(id).join("result.md")).ok().filter(|t| !t.trim().is_empty())
}

pub fn job_duration(job: &Job) -> Option<i64> {
    let start = parse_iso_ms(job.started_at.as_deref()?)?;
    let end = job.ended_at.as_deref().and_then(parse_iso_ms).unwrap_or_else(now_ms);
    Some(end - start)
}

#[derive(Debug, Serialize)]
pub struct CollectResult {
    pub batch: String,
    pub status: String,
    pub counts: BTreeMap<String, usize>,
    pub index: String,
    pub manifest: Option<String>,
    pub sources: Option<String>,
    #[serde(rename = "materialFiles")]
    pub material_files: usize,
    #[serde(rename = "sourceCount")]
    pub source_count: usize,
}

pub fn collect_batch(batch_id: &str, output: Option<PathBuf>) -> Result<CollectResult> {
    let batch = require_batch(batch_id)?;
    let root = PathBuf::from(&batch.root);
    let jobs = list_jobs(Some(batch_id), None, true);
    let summary = summarize_jobs(&jobs);
    let mut manifest: Vec<Value> = vec![];
    let mut sources: Vec<Value> = vec![];
    let mut seen_urls = std::collections::HashSet::new();
    let mut sections: Vec<String> = vec![];

    for job in &jobs {
        let dir = PathBuf::from(job.out());
        let files = list_files(&dir, 0, 500);
        let result = job_result(&job.id);
        if let Ok(Some(Value::Array(items))) = read_json::<Value>(&dir.join("manifest.json")) {
            for mut item in items {
                let file = item.get("file").and_then(|f| f.as_str()).filter(|f| !f.is_empty()).map(|f| rel(&root, &dir.join(f)));
                if let Some(obj) = item.as_object_mut() {
                    obj.insert("file".into(), file.map(Value::String).unwrap_or(Value::Null));
                    obj.insert("job".into(), Value::String(job.id.clone()));
                }
                manifest.push(item);
            }
        }
        if let Ok(Some(Value::Array(items))) = read_json::<Value>(&dir.join("sources.json")) {
            for mut s in items {
                let Some(url) = s.get("url").and_then(|u| u.as_str()).filter(|u| !u.is_empty()).map(String::from) else { continue };
                if seen_urls.insert(url) {
                    if let Some(obj) = s.as_object_mut() {
                        obj.insert("job".into(), Value::String(job.id.clone()));
                    }
                    sources.push(s);
                }
            }
        }
        let nn = format!("{:02}", job.index.unwrap_or(0));
        let folder = {
            let r = rel(&root, &dir);
            if r.is_empty() {
                ".".to_string()
            } else {
                r
            }
        };
        let mut lines = vec![
            format!("## {nn} · {} ({}, {})", job.name.as_deref().unwrap_or(&job.id), job.status, fmt_duration(job_duration(job))),
            String::new(),
            format!("- Job: `{}`", job.id),
            format!("- Folder: `{folder}`"),
            format!("- Task: {}", excerpt(&job.prompt, 300)),
        ];
        if let Some(e) = &job.error {
            lines.push(format!("- Error: {}", excerpt(e, 300)));
        }
        if files.is_empty() {
            lines.push("- Files: none".into());
        } else {
            let listed = files.iter().take(40).map(|f| format!("`{}`", f.path)).collect::<Vec<_>>().join(", ");
            let more = if files.len() > 40 { format!(" and {} more", files.len() - 40) } else { String::new() };
            lines.push(format!("- Files: {listed}{more}"));
        }
        lines.push(String::new());
        lines.push(result.map(|r| r.trim().to_string()).unwrap_or_else(|| "_No final answer._".into()));
        lines.push(String::new());
        sections.push(lines.join("\n"));
    }

    let counts = summary.counts.iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(", ");
    let created =
        parse_iso_ms(&batch.created_at).and_then(DateTime::<Utc>::from_timestamp_millis).map(|d| d.format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default();
    let mut index: Vec<String> = vec![
        format!("# {}", batch.name.as_deref().unwrap_or(&batch.id)),
        String::new(),
        format!("Batch `{}` · status {} ({counts}) · {} jobs · created {created} UTC", batch.id, summary.status, jobs.len()),
        if manifest.is_empty() { String::new() } else { format!("\nMaterial: {} files, see `manifest.json`.", manifest.len()) },
        if sources.is_empty() { String::new() } else { format!("Sources: {} unique URLs, see `sources.json`.", sources.len()) },
        String::new(),
    ];
    index.extend(sections);

    let index_path = output.unwrap_or_else(|| root.join("INDEX.md"));
    fs::write(&index_path, index.join("\n"))?;
    let mut manifest_path = None;
    let mut sources_path = None;
    if !manifest.is_empty() {
        let p = root.join("manifest.json");
        fs::write(&p, serde_json::to_string_pretty(&json!(manifest))? + "\n")?;
        manifest_path = Some(p.display().to_string());
    }
    if !sources.is_empty() {
        let p = root.join("sources.json");
        fs::write(&p, serde_json::to_string_pretty(&json!(sources))? + "\n")?;
        sources_path = Some(p.display().to_string());
    }
    Ok(CollectResult {
        batch: batch.id,
        status: summary.status,
        counts: summary.counts,
        index: index_path.display().to_string(),
        manifest: manifest_path,
        sources: sources_path,
        material_files: manifest.len(),
        source_count: sources.len(),
    })
}
