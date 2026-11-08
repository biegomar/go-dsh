//! go-dsh command line: parsing, command handlers, text and JSON output.

use std::fs;
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use chrono::{DateTime, Local};
use clap::{Args, CommandFactory, Parser, Subcommand};
use serde_json::{json, Map, Value};

use crate::collect::{collect_batch, fmt_bytes, job_duration, job_result, list_files};
use crate::config::{cfg, key_def, read_config, set_config, CONFIG_KEYS};
use crate::dsh::{active_profile, resolve_model, EFFORTS};
use crate::events::{parse_lines, timeline};
use crate::paths::{default_workspace_root, home, job_dir};
use crate::profile::{doctor, find_python, setup_profile};
use crate::runner::{cancel_job, create_runnable_job, install_signal_forwarding, run_job, spawn_detached};
use crate::skills::install_skill;
use crate::store::{
    create_batch, is_active, is_terminal, list_batches, list_jobs, require_batch, require_job, resolve_ref, summarize_jobs, update_batch, update_job, Batch,
    Job, Ref, RefKind, Summary, WebInfo,
};
use crate::supervisor::{cancel_batch, run_batch};
use crate::templates::{material_description, material_kind_names, materials_prompt, research_prompt, workdir_header, workspace_agents_md, DEPTHS, TEMPLATES};
use crate::util::{
    excerpt, expand_home, fmt_duration, force_kill, now_iso, now_ms, parse_duration, parse_iso_ms, pid_alive, sleep_ms, slug, terminate, DshgoError, Result,
};
use crate::web;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const GUIDE: &str = include_str!("../assets/guide.md");
const CAPTURE_PY: &str = include_str!("../assets/capture.py");
const WORKSPACE_MARKER: &str = ".dshgo-workspace";

// ---------------------------------------------------------------- output ----

static JSON_MODE: AtomicBool = AtomicBool::new(false);

fn json_mode() -> bool {
    JSON_MODE.load(Ordering::Relaxed)
}

fn print(text: &str) {
    if text.ends_with('\n') {
        print!("{text}");
    } else {
        println!("{text}");
    }
}

fn emit(data: Value, text: impl FnOnce() -> String) {
    if json_mode() {
        let mut obj = Map::new();
        obj.insert("ok".into(), Value::Bool(true));
        if let Value::Object(m) = data {
            obj.extend(m);
        }
        print(&serde_json::to_string_pretty(&Value::Object(obj)).unwrap_or_default());
    } else {
        print(&text());
    }
}

fn clock(iso: Option<&str>) -> String {
    iso.and_then(parse_iso_ms)
        .and_then(DateTime::from_timestamp_millis)
        .map(|d| d.with_timezone(&Local).format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "-".into())
}

fn ago(iso: Option<&str>) -> String {
    match iso.and_then(parse_iso_ms) {
        Some(ms) => format!("{} ago", fmt_duration(Some(now_ms() - ms))),
        None => "-".into(),
    }
}

fn pad(s: impl AsRef<str>, n: usize) -> String {
    let s = s.as_ref();
    let len = s.chars().count();
    if len >= n {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(n - len))
    }
}

fn k_tokens(n: u64) -> String {
    if n >= 1000 {
        format!("{}k", (n as f64 / 1000.0).round() as u64)
    } else {
        n.to_string()
    }
}

fn model_label(job: &Job) -> String {
    if job.model.is_some() || job.effort.is_some() {
        let model = resolve_model(job.model.as_deref()).unwrap_or_else(|| "default".into());
        match &job.effort {
            Some(e) => format!("{model} / {e}"),
            None => model,
        }
    } else {
        "default from ~/.dsh/settings.yaml".into()
    }
}

// --------------------------------------------------------------- parsing ----

#[derive(Parser)]
#[command(
    name = "go-dsh",
    version = VERSION,
    about = "Run DeepSeek Harness sessions from any agent harness",
    after_help = "Backend: when the DSH web UI runs (dsh web / go-dsh ui start), jobs run there as sessions,\nlive and with one workspace per batch. Otherwise each job runs in its own headless dsh.\n\nExit: 0 ok, 1 job failed, 2 usage, 3 not found, 4 wait timeout or not finished, 5 environment",
    propagate_version = true
)]
struct Cli {
    /// Machine readable output: one JSON object
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

/// Options shared by every command that starts jobs.
#[derive(Args, Clone, Default)]
struct JobOpts {
    /// Workspace folder (default: a new folder under ~/dsh-workspaces)
    #[arg(short = 'C', long)]
    cwd: Option<String>,
    /// Display name (also the session title in the UI)
    #[arg(long)]
    name: Option<String>,
    /// flash, pro, v4-flash, vision or a full model id
    #[arg(short, long)]
    model: Option<String>,
    /// off, low, high, max
    #[arg(long)]
    effort: Option<String>,
    /// Job time limit, e.g. 90s, 30m, 1h (default 30m)
    #[arg(short, long)]
    timeout: Option<String>,
    /// Block until done and print the answer
    #[arg(short, long)]
    wait: bool,
    /// With --wait: give up waiting after this long (exit 4)
    #[arg(long = "max-wait")]
    max_wait: Option<String>,
    /// auto (default): inside the running DSH web UI, else headless; web forces the UI, headless a separate dsh process
    #[arg(short, long)]
    backend: Option<String>,
}

#[derive(Args)]
struct RunArgs {
    /// The task (use `-` or no text to read stdin)
    task: Vec<String>,
    /// Read the task from a file
    #[arg(short, long)]
    file: Option<String>,
    /// raw (default), research, materials
    #[arg(long)]
    template: Option<String>,
    /// research: quick, standard, deep
    #[arg(long)]
    depth: Option<String>,
    /// materials: screenshots, scroll, articles, stock-video, stock-photo
    #[arg(long)]
    kind: Option<String>,
    /// materials: mandatory URL (repeatable)
    #[arg(long)]
    url: Vec<String>,
    /// materials: target count (default 6)
    #[arg(long)]
    count: Option<String>,
    #[command(flatten)]
    opts: JobOpts,
}

#[derive(Args)]
struct BatchArgs {
    /// Tasks given inline
    tasks: Vec<String>,
    /// One task per line (# = comment), blocks separated by a line "---", .json (array) or .jsonl ({task,name,model,effort,template})
    #[arg(short, long)]
    file: Option<String>,
    /// A task inline (repeatable)
    #[arg(long = "task")]
    task: Vec<String>,
    /// Concurrent jobs (default: all, at most 10)
    #[arg(short, long)]
    parallel: Option<String>,
    /// raw (default) or research
    #[arg(long)]
    template: Option<String>,
    /// research depth: quick, standard, deep
    #[arg(long)]
    depth: Option<String>,
    #[command(flatten)]
    opts: JobOpts,
}

#[derive(Args)]
struct ResearchArgs {
    /// One research question per argument
    questions: Vec<String>,
    /// Questions from a file (as for batch)
    #[arg(short, long)]
    file: Option<String>,
    /// quick, standard (default), deep
    #[arg(long)]
    depth: Option<String>,
    /// Extra instruction for every worker
    #[arg(long)]
    focus: Option<String>,
    /// Concurrent jobs
    #[arg(short, long)]
    parallel: Option<String>,
    #[command(flatten)]
    opts: JobOpts,
}

#[derive(Args)]
struct MaterialsArgs {
    /// Topic, purpose, style
    brief: Vec<String>,
    /// Read the brief from a file
    #[arg(short, long)]
    file: Option<String>,
    /// Comma list of screenshots, scroll, articles, stock-video, stock-photo (default: screenshots,scroll,articles,stock-video)
    #[arg(long)]
    kinds: Option<String>,
    /// Mandatory URL for screenshots, scroll, articles (repeatable)
    #[arg(long)]
    url: Vec<String>,
    /// Target count per kind (default 6)
    #[arg(long)]
    count: Option<String>,
    /// Concurrent jobs
    #[arg(short, long)]
    parallel: Option<String>,
    #[command(flatten)]
    opts: JobOpts,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start one dsh job in the background and return its id at once
    Run(RunArgs),
    /// Start many jobs in parallel, one folder each
    Batch(BatchArgs),
    /// One research job per question, all in parallel (report.md, sources.json)
    Research(ResearchArgs),
    /// Gather material, one parallel job per kind (files plus manifest.json)
    Materials(MaterialsArgs),
    /// Active and recent jobs, or details of a job or batch (ref: id, prefix, name, last, last-batch)
    Status { reference: Option<String> },
    /// List jobs (newest first) or, with --batches, the batches
    #[command(alias = "list")]
    Ls {
        #[arg(short = 'n')]
        n: Option<String>,
        #[arg(long)]
        running: bool,
        #[arg(long)]
        batch: Option<String>,
        #[arg(long)]
        batches: bool,
    },
    /// Block until all (or with --any the first) are finished. Exit 0 all done, 1 failed, 4 timeout
    Wait {
        refs: Vec<String>,
        #[arg(short, long)]
        timeout: Option<String>,
        #[arg(long)]
        any: bool,
    },
    /// Final answer of a job; for a batch every answer in order
    Result {
        reference: Option<String>,
        #[arg(long)]
        files: bool,
    },
    /// Timeline of tool calls and answers (-f follows live)
    Logs {
        job: Option<String>,
        #[arg(short = 'n')]
        n: Option<String>,
        #[arg(short, long)]
        follow: bool,
        #[arg(long)]
        full: bool,
        #[arg(long)]
        raw: bool,
        #[arg(long)]
        stderr: bool,
    },
    /// Files the job created or changed in its folder
    Files {
        reference: Option<String>,
        /// Every file, not only new ones
        #[arg(long)]
        all: bool,
    },
    /// Continue the dsh session of a job, or of every finished job of a batch, with a follow-up task
    Continue {
        /// Job or batch (id, prefix, name, last, last-batch)
        job: Option<String>,
        /// Follow-up text (use `-` or nothing to read stdin)
        text: Vec<String>,
        #[arg(short, long)]
        file: Option<String>,
        #[arg(short, long)]
        wait: bool,
        #[arg(short, long)]
        timeout: Option<String>,
        #[arg(long = "max-wait")]
        max_wait: Option<String>,
        /// Batch only: concurrent jobs (default: all, at most 10)
        #[arg(short, long)]
        parallel: Option<String>,
        /// Batch only: skip jobs that are already done
        #[arg(long = "only-failed")]
        only_failed: bool,
    },
    /// Cancel a job or every unfinished job of a batch
    Cancel { reference: Option<String> },
    /// Write INDEX.md and merge manifest.json and sources.json of a batch
    Collect {
        batch: Option<String>,
        #[arg(short, long)]
        output: Option<String>,
    },
    /// Workspace helpers: `ws init <dir>`
    Ws {
        sub: Option<String>,
        dir: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        description: Option<String>,
    },
    /// Screenshots and scroll videos with Playwright: `capture shot|scroll <url> -o <file>`
    #[command(disable_help_flag = true)]
    Capture {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        args: Vec<String>,
    },
    /// DSH web UI: `ui [ref] [--print]`, `ui start [--port 3080] [--open]`, `ui stop [--force]`, `ui login <url>`
    Ui {
        args: Vec<String>,
        /// Print the URL instead of opening the browser
        #[arg(long)]
        print: bool,
        /// `ui start`: port of the web UI (default 3080)
        #[arg(long)]
        port: Option<String>,
        /// `ui start`: open the browser once the UI is running
        #[arg(long)]
        open: bool,
        /// `ui stop`: stop even if web jobs are still running
        #[arg(long)]
        force: bool,
    },
    /// Install the agent skill: `skill install [--agents claude,codex,pi] [--copy]`
    Skill {
        sub: Option<String>,
        #[arg(long)]
        agents: Option<String>,
        #[arg(long)]
        copy: bool,
    },
    /// One-time setup: dsh profile "concert" and the agent skill
    Setup {
        /// research (default), all, none or a comma list of ids
        #[arg(long)]
        mcp: Option<String>,
        /// Profile whose MCP servers are copied (default web)
        #[arg(long)]
        from: Option<String>,
        /// Comma list of skills from ~/.agents/skills to link (default: none), or "all" for the dsh defaults
        #[arg(long)]
        skills: Option<String>,
        #[arg(long = "no-skill")]
        no_skill: bool,
    },
    /// Check dsh, profile, settings, web UI, Playwright, ffmpeg and yt-dlp
    Doctor,
    /// Show or change persistent settings: `config`, `config set <key> <value>`, `config unset <key>`
    Config {
        #[arg(allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// The full manual for agents (workflows, recipes, rules)
    Guide,
    #[command(name = "__run-job", hide = true)]
    RunJob { id: String },
    #[command(name = "__run-batch", hide = true)]
    RunBatch { id: String },
}

const BACKENDS: &[&str] = &["auto", "web", "headless"];

/// auto: the DSH web UI when it is running (live and grouped there), else headless.
fn resolve_backend(requested: Option<&str>) -> Result<&'static str> {
    let wanted = requested.map(String::from).or_else(|| cfg("backend")).unwrap_or_else(|| "auto".into());
    if !BACKENDS.contains(&wanted.as_str()) {
        return Err(DshgoError::new(format!("--backend must be one of {}", BACKENDS.join(", ")), 2));
    }
    if wanted == "headless" {
        return Ok("headless");
    }
    let up = web::available();
    if wanted == "web" && !up {
        return Err(DshgoError::new(format!("DSH web UI at {} is not reachable", web::base_url()), 5).hint("go-dsh ui start, or use --backend headless"));
    }
    Ok(if up { "web" } else { "headless" })
}

fn base_name(p: &str) -> String {
    Path::new(p).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| p.to_string())
}

fn backend_line(backend: &str, cwd: &str, width: usize) -> String {
    if backend == "web" {
        format!("  {}live at {}, workspace \"{}\"", pad("DSH UI:", width), web::base_url(), base_name(cwd))
    } else {
        format!("  {}headless (DSH web UI not reachable; the session shows up as \"Ungrouped\" there)", pad("Backend:", width))
    }
}

fn read_stdin() -> String {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return String::new();
    }
    let mut s = String::new();
    let _ = stdin.lock().read_to_string(&mut s);
    s
}

fn task_from(positionals: &[String], file: Option<&str>) -> Result<String> {
    if let Some(f) = file {
        return Ok(fs::read_to_string(absolute(f))?);
    }
    if positionals.len() == 1 && positionals[0] == "-" {
        return Ok(read_stdin());
    }
    if !positionals.is_empty() {
        return Ok(positionals.join(" "));
    }
    Ok(read_stdin())
}

fn absolute(p: &str) -> PathBuf {
    let mut text = expand_home(p);
    if cfg!(windows) {
        // `~/x` expands to `C:\Users\me/x`; keep one separator style.
        text = text.replace('/', std::path::MAIN_SEPARATOR_STR);
    }
    let expanded = PathBuf::from(text);
    if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir().unwrap_or_default().join(expanded)
    }
}

fn validate_model_opts(opts: &JobOpts) -> Result<(Option<String>, Option<String>)> {
    if let Some(e) = &opts.effort {
        if !EFFORTS.contains(&e.as_str()) {
            return Err(DshgoError::new(format!("--effort must be one of {}", EFFORTS.join(", ")), 2));
        }
    }
    Ok((opts.model.clone(), opts.effort.clone()))
}

fn timeout_sec(timeout: Option<&str>, fallback: u64) -> Result<u64> {
    match timeout {
        Some(t) => parse_duration(t),
        None => Ok(fallback),
    }
}

fn date_stamp() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

/// True when dir or an ancestor already is a go-dsh workspace.
fn inside_workspace(dir: &Path) -> bool {
    let mut current = Some(dir);
    while let Some(c) = current {
        if c.join(WORKSPACE_MARKER).exists() {
            return true;
        }
        current = c.parent();
    }
    false
}

/// Create dir and, unless it already sits in a workspace, mark it as one.
fn ensure_workspace(dir: &Path, kind: &str, description: Option<&str>, force: bool) -> Result<(PathBuf, bool)> {
    fs::create_dir_all(dir)?;
    let abs = fs::canonicalize(dir).map(strip_unc).unwrap_or_else(|_| dir.to_path_buf());
    if !force && inside_workspace(&abs) {
        return Ok((abs, false));
    }
    fs::write(abs.join(WORKSPACE_MARKER), format!("{}\n", json!({ "kind": kind, "createdAt": now_iso() })))?;
    let agents = abs.join("AGENTS.md");
    if force || !agents.exists() {
        fs::write(&agents, workspace_agents_md(&base_name(&abs.display().to_string()), kind, description))?;
    }
    Ok((abs, true))
}

/// Windows canonicalize yields `\\?\C:\...`; dsh and users expect the plain form.
fn strip_unc(p: PathBuf) -> PathBuf {
    let s = p.display().to_string();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => p,
    }
}

/// -C wins; an existing directory stays untouched, a new one becomes a workspace.
fn resolve_workspace(cwd_opt: Option<&str>, fallback_name: &str, kind: &str) -> Result<String> {
    if let Some(c) = cwd_opt {
        let abs = absolute(c);
        if !abs.exists() {
            ensure_workspace(&abs, kind, None, false)?;
        }
        return Ok(strip_unc(abs).display().to_string());
    }
    let root = cfg("workspaces").map(PathBuf::from).unwrap_or_else(default_workspace_root);
    let first = root.join(format!("{}-{fallback_name}", date_stamp()));
    let mut candidate = first.clone();
    let mut i = 2;
    while candidate.exists() {
        candidate = PathBuf::from(format!("{}-{i}", first.display()));
        i += 1;
    }
    let (abs, _) = ensure_workspace(&candidate, kind, None, false)?;
    Ok(abs.display().to_string())
}

// ------------------------------------------------------------ job views ----

fn job_view(job: &Job) -> Value {
    let result_file = job_dir(&job.id).join("result.md");
    json!({
        "id": job.id,
        "name": job.name,
        "title": job.title,
        "status": job.status,
        "backend": job.backend.clone().unwrap_or_else(|| "headless".into()),
        "batch": job.batch,
        "index": job.index,
        "template": job.template,
        "prompt": job.prompt,
        "cwd": job.cwd,
        "outDir": job.out(),
        "web": job.web,
        "sessionId": job.session_id,
        "model": job.model,
        "effort": job.effort,
        "parent": job.parent,
        "createdAt": job.created_at,
        "startedAt": job.started_at,
        "endedAt": job.ended_at,
        "durationMs": job_duration(job),
        "progress": job.progress,
        "usage": job.usage,
        "exitCode": job.exit_code,
        "error": job.error,
        "resultFile": result_file.exists().then(|| result_file.display().to_string()),
        "eventsFile": job_dir(&job.id).join("events.jsonl").display().to_string(),
    })
}

fn job_detail_text(job: &Job) -> String {
    let p = &job.progress;
    let u = &job.usage;
    let mut lines = vec![
        format!("Job {}  [{}]  {}", job.id, job.status, job.title.as_deref().or(job.name.as_deref()).unwrap_or("")),
        format!("  Task:      {}", excerpt(&job.prompt, 140)),
        format!("  Workspace: {}", job.cwd),
    ];
    if job.out() != job.cwd {
        lines.push(format!("  Folder:    {}", job.out()));
    }
    let backend = if job.backend.as_deref() == Some("web") {
        let ws = job.web.as_ref().and_then(|w| w.workspace_title.as_ref()).map(|t| format!(" (workspace \"{t}\")")).unwrap_or_default();
        format!("web, live in the DSH UI{ws}")
    } else {
        "headless".into()
    };
    lines.push(format!("  Backend:   {backend}"));
    lines.push(format!("  Session:   {}", job.session_id.as_deref().unwrap_or("-")));
    lines.push(format!("  Model:     {}", model_label(job)));
    let started = job.started_at.as_deref().map(|s| format!(", started {}", clock(Some(s)))).unwrap_or_default();
    let ended = job.ended_at.as_deref().map(|s| format!(", ended {}", clock(Some(s)))).unwrap_or_default();
    lines.push(format!("  Runtime:   {} (created {}{started}{ended})", fmt_duration(job_duration(job)), clock(Some(&job.created_at))));
    let last_tool = p.last_tool.as_deref().map(|t| format!(", last {t}")).unwrap_or_default();
    let active =
        if is_active(&job.status) { p.last_activity_at.as_deref().map(|a| format!(", active {}", ago(Some(a)))).unwrap_or_default() } else { String::new() };
    lines.push(format!("  Progress:  {} steps, {} tool calls{last_tool}{active}", p.steps, p.tool_calls));
    lines.push(format!("  Tokens:    in {}, out {}, cache {}", k_tokens(u.input_tokens), k_tokens(u.output_tokens), k_tokens(u.cache_read_tokens)));
    if let Some(b) = &job.batch {
        lines.push(format!("  Batch:     {b} (#{})", job.index.unwrap_or(0)));
    }
    if let Some(p) = &job.parent {
        lines.push(format!("  Follows:   {p}"));
    }
    if let Some(e) = &job.error {
        lines.push(format!("  Error:     {}", excerpt(e, 300)));
    }
    if job.status == "done" {
        lines.push(format!("  Result:    go-dsh result {}", job.id));
    } else if is_active(&job.status) {
        lines.push(format!("  Wait:      go-dsh wait {}    Live: go-dsh logs {} -f", job.id, job.id));
    }
    lines.join("\n")
}

fn job_row(job: &Job) -> String {
    let tail = if job.status == "done" {
        excerpt(&job_result(&job.id).unwrap_or_default(), 60)
    } else if let Some(e) = &job.error {
        excerpt(e, 60)
    } else if let Some(t) = &job.progress.last_tool {
        format!("→ {t}")
    } else {
        String::new()
    };
    format!(
        "{} {} {} {} {} {}",
        pad(&job.id, 16),
        pad(&job.status, 9),
        pad(fmt_duration(job_duration(job)), 8),
        pad(job.progress.tool_calls.to_string(), 5),
        pad(excerpt(job.name.as_deref().unwrap_or(&job.prompt), 28), 28),
        tail
    )
}

fn job_header() -> String {
    format!("{} {} {} {} {} ANSWER / ERROR", pad("JOB", 16), pad("STATUS", 9), pad("TIME", 8), pad("TOOLS", 5), pad("NAME", 28))
}

struct BatchView {
    batch: Batch,
    jobs: Vec<Job>,
    summary: Summary,
}

fn batch_view(batch: Batch) -> BatchView {
    let jobs = list_jobs(Some(&batch.id), None, true);
    let summary = summarize_jobs(&jobs);
    BatchView { batch, jobs, summary }
}

fn batch_text(v: &BatchView) -> String {
    let c = &v.summary.counts;
    let n = |k: &str| c.get(k).copied().unwrap_or(0);
    let mut parts = vec![format!("{}/{} done", n("done"), v.summary.total)];
    if n("running") > 0 {
        parts.push(format!("{} running", n("running")));
    }
    if n("queued") > 0 {
        parts.push(format!("{} queued", n("queued")));
    }
    for k in ["failed", "timeout", "cancelled", "lost"] {
        if n(k) > 0 {
            parts.push(format!("{} {k}", n(k)));
        }
    }
    let backend = match v.jobs.first() {
        Some(j) if j.backend.as_deref() == Some("web") => {
            let t = j.web.as_ref().and_then(|w| w.workspace_title.clone()).unwrap_or_else(|| base_name(&v.batch.root));
            format!("web UI workspace \"{t}\"")
        }
        _ => "headless".into(),
    };
    let mut lines = vec![
        format!("Batch {}  [{}]  {}  {}", v.batch.id, v.summary.status, v.batch.name.as_deref().unwrap_or(""), parts.join(", ")),
        format!("  Root:     {}", v.batch.root),
        format!("  Parallel: {} · template: {} · {backend} · created {}", v.batch.parallel, v.batch.template, clock(Some(&v.batch.created_at))),
        String::new(),
        format!("   # {}", job_header()),
    ];
    for j in &v.jobs {
        lines.push(format!("  {:02} {}", j.index.unwrap_or(0), job_row(j)));
    }
    lines.push(String::new());
    lines.push(if v.summary.status == "running" { format!("Wait: go-dsh wait {}", v.batch.id) } else { format!("Summarize: go-dsh collect {}", v.batch.id) });
    lines.join("\n")
}

// --------------------------------------------------------------- waiting ----

fn ref_jobs(r: &Ref) -> Result<Vec<Job>> {
    Ok(match r.kind {
        RefKind::Job => vec![require_job(&r.id)?],
        RefKind::Batch => list_jobs(Some(&r.id), None, true),
    })
}

fn wait_for(refs: &[Ref], timeout_ms: Option<u64>, any: bool) -> Result<(Vec<Job>, bool)> {
    let start = Instant::now();
    let tty = std::io::stderr().is_terminal() && !json_mode();
    loop {
        let mut jobs = vec![];
        for r in refs {
            jobs.extend(ref_jobs(r)?);
        }
        let finished = jobs.iter().filter(|j| is_terminal(&j.status)).count();
        let done = if any { finished > 0 } else { finished == jobs.len() };
        if done {
            return Ok((jobs, false));
        }
        if timeout_ms.map(|t| start.elapsed().as_millis() as u64 >= t).unwrap_or(false) {
            return Ok((jobs, true));
        }
        if tty {
            eprint!("\r{finished}/{} done ...   ", jobs.len());
        }
        sleep_ms(1000);
    }
}

fn wait_outcome(jobs: &[Job], timed_out: bool) -> i32 {
    let failed = jobs.iter().any(|j| matches!(j.status.as_str(), "failed" | "timeout" | "cancelled" | "lost"));
    if timed_out {
        4
    } else if failed {
        1
    } else {
        0
    }
}

fn clear_progress_line() {
    if std::io::stderr().is_terminal() && !json_mode() {
        eprint!("\r");
    }
}

// ---------------------------------------------------------------- start ----

struct JobSpec {
    name: Option<String>,
    title: Option<String>,
    template: String,
    prompt: String,
    cwd: String,
    out_dir: Option<String>,
    backend: String,
    model: Option<String>,
    effort: Option<String>,
    timeout_sec: u64,
    batch: Option<String>,
    index: Option<u32>,
    parent: Option<String>,
    resume: Option<String>,
    web: Option<WebInfo>,
}

fn make_job(spec: JobSpec, task_text: &str) -> Result<Job> {
    create_runnable_job(
        |j| {
            j.name = spec.name;
            j.title = spec.title;
            j.template = spec.template;
            j.prompt = spec.prompt;
            j.cwd = spec.cwd;
            j.out_dir = spec.out_dir;
            j.backend = Some(spec.backend);
            j.model = spec.model;
            j.effort = spec.effort;
            j.timeout_sec = spec.timeout_sec;
            j.batch = spec.batch;
            j.index = spec.index;
            j.parent = spec.parent;
            j.resume = spec.resume;
            j.web = spec.web;
        },
        task_text,
    )
}

fn start_job(spec: JobSpec, task_text: &str, header: bool) -> Result<Job> {
    let text = if header { format!("{}{task_text}", workdir_header(spec.out_dir.as_deref().unwrap_or(&spec.cwd))) } else { task_text.to_string() };
    let job = make_job(spec, &text)?;
    let pid = spawn_detached(&["__run-job", &job.id])?;
    update_job(&job.id, |j| {
        if j.owner_pid.is_none() {
            j.owner_pid = Some(pid);
        }
    })?;
    require_job(&job.id)
}

struct TaskItem {
    prompt: String,
    text: String,
    slug: Option<String>,
    title: Option<String>,
    template: Option<String>,
    model: Option<String>,
    effort: Option<String>,
}

struct StartBatch<'a> {
    name: &'a str,
    root: &'a str,
    parallel: u32,
    template: &'a str,
    tasks: Vec<TaskItem>,
    timeout_sec: u64,
    model: Option<String>,
    effort: Option<String>,
    backend: &'a str,
}

fn start_batch(sb: StartBatch) -> Result<Batch> {
    let batch = create_batch(|b| {
        b.name = Some(sb.name.to_string());
        b.root = sb.root.to_string();
        b.parallel = sb.parallel;
        b.template = sb.template.to_string();
    })?;
    let width = sb.tasks.len().to_string().len().max(2);
    let mut ids = vec![];
    for (i, t) in sb.tasks.iter().enumerate() {
        let nn = format!("{:0width$}", i + 1);
        let job_name = format!("{nn}-{}", t.slug.clone().unwrap_or_else(|| slug(&t.prompt, 32)));
        let out_dir = Path::new(sb.root).join(&job_name).display().to_string();
        let title = format!("{nn} · {}", t.title.clone().unwrap_or_else(|| excerpt(&t.prompt, 70)));
        let spec = JobSpec {
            name: Some(job_name),
            title: Some(title),
            template: t.template.clone().unwrap_or_else(|| sb.template.to_string()),
            prompt: t.prompt.clone(),
            // Web sessions of one batch share the batch workspace directory.
            cwd: if sb.backend == "web" { sb.root.to_string() } else { out_dir.clone() },
            out_dir: Some(out_dir.clone()),
            backend: sb.backend.to_string(),
            model: t.model.clone().or_else(|| sb.model.clone()),
            effort: t.effort.clone().or_else(|| sb.effort.clone()),
            timeout_sec: sb.timeout_sec,
            batch: Some(batch.id.clone()),
            index: Some(i as u32 + 1),
            parent: None,
            resume: None,
            web: None,
        };
        let job = make_job(spec, &format!("{}{}", workdir_header(&out_dir), t.text))?;
        ids.push(job.id);
    }
    launch_batch(&batch.id, ids)
}

/// Register the job ids on the batch and hand the batch to a detached runner.
fn launch_batch(batch_id: &str, ids: Vec<String>) -> Result<Batch> {
    update_batch(batch_id, |b| b.jobs = ids.clone())?;
    let pid = spawn_detached(&["__run-batch", batch_id])?;
    update_batch(batch_id, |b| {
        if b.owner_pid.is_none() {
            b.owner_pid = Some(pid);
        }
    })?;
    for id in &ids {
        update_job(id, |j| {
            if j.owner_pid.is_none() {
                j.owner_pid = Some(pid);
            }
        })?;
    }
    require_batch(batch_id)
}

fn finish_start(r: Ref, opts_wait: bool, max_wait: Option<&str>, started_text: impl FnOnce() -> String) -> Result<i32> {
    if !opts_wait {
        let data = match r.kind {
            RefKind::Job => json!({ "job": job_view(&require_job(&r.id)?) }),
            RefKind::Batch => json!({
                "batch": require_batch(&r.id)?,
                "jobs": list_jobs(Some(&r.id), None, true).iter().map(job_view).collect::<Vec<_>>(),
            }),
        };
        emit(data, started_text);
        return Ok(0);
    }
    let max_ms = match max_wait {
        Some(m) => Some(parse_duration(m)? * 1000),
        None => None,
    };
    let (jobs, timed_out) = wait_for(std::slice::from_ref(&r), max_ms, false)?;
    clear_progress_line();
    let code = wait_outcome(&jobs, timed_out);
    match r.kind {
        RefKind::Job => {
            let job = &jobs[0];
            let result = job_result(&job.id);
            emit(json!({ "job": job_view(job), "result": result, "timedOut": timed_out }), || {
                if timed_out {
                    format!("{}\n\nMax wait reached; the job keeps running.", job_detail_text(job))
                } else {
                    format!(
                        "# {} · {} · {} · {}\n\n{}",
                        job.id,
                        job.status,
                        fmt_duration(job_duration(job)),
                        job.cwd,
                        result.clone().or_else(|| job.error.clone()).unwrap_or_else(|| "(no answer)".into())
                    )
                }
            });
        }
        RefKind::Batch => {
            let view = batch_view(require_batch(&r.id)?);
            let jobs_json: Vec<Value> = view
                .jobs
                .iter()
                .map(|j| {
                    let mut v = job_view(j);
                    v["result"] = json!(job_result(&j.id));
                    v
                })
                .collect();
            emit(json!({ "batch": view.batch, "summary": view.summary, "jobs": jobs_json, "timedOut": timed_out }), || batch_text(&view));
        }
    }
    Ok(code)
}

// -------------------------------------------------------------- commands ----

fn cmd_run(a: RunArgs) -> Result<i32> {
    let template = a.template.clone().unwrap_or_else(|| "raw".into());
    if !TEMPLATES.contains(&template.as_str()) {
        return Err(DshgoError::new(format!("--template must be one of {}", TEMPLATES.join(", ")), 2));
    }
    let prompt = task_from(&a.task, a.file.as_deref())?.trim().to_string();
    if prompt.is_empty() {
        return Err(DshgoError::new("No task given", 2).hint("go-dsh run \"task\" or echo \"task\" | go-dsh run -"));
    }
    let (model, effort) = validate_model_opts(&a.opts)?;
    let mut text = prompt.clone();
    if template == "research" {
        text = research_prompt(&prompt, a.depth.as_deref().unwrap_or("standard"), None);
    }
    if template == "materials" {
        let kind = a.kind.clone().unwrap_or_else(|| "screenshots".into());
        let count = a.count.as_deref().map(|c| c.parse::<u32>().unwrap_or(6)).unwrap_or(6);
        text = materials_prompt(&prompt, &kind, &a.url, count).ok_or_else(|| DshgoError::new(format!("--kind must be one of {}", material_kind_names()), 2))?;
    }
    let backend = resolve_backend(a.opts.backend.as_deref())?;
    let name = a.opts.name.clone().unwrap_or_else(|| slug(&prompt, 32));
    let cwd = resolve_workspace(a.opts.cwd.as_deref(), &name, if template == "raw" { "general" } else { &template })?;
    let title = a.opts.name.clone().unwrap_or_else(|| excerpt(&prompt, 70));
    let job = start_job(
        JobSpec {
            name: Some(name),
            title: Some(title),
            template,
            prompt,
            cwd: cwd.clone(),
            out_dir: Some(cwd.clone()),
            backend: backend.into(),
            model,
            effort,
            timeout_sec: timeout_sec(a.opts.timeout.as_deref(), 1800)?,
            batch: None,
            index: None,
            parent: None,
            resume: None,
            web: None,
        },
        &text,
        true,
    )?;
    let id = job.id.clone();
    finish_start(Ref { kind: RefKind::Job, id: id.clone() }, a.opts.wait, a.opts.max_wait.as_deref(), || {
        [
            format!(
                "Job {id} started ({})",
                if backend == "web" { "in the DSH web UI".to_string() } else { format!("headless, dsh profile {}", active_profile()) }
            ),
            format!("  Workspace: {cwd}"),
            backend_line(backend, &cwd, 11),
            format!("  Wait:      go-dsh wait {id} --timeout 9m"),
            format!("  Result:    go-dsh result {id}"),
            format!("  Live:      go-dsh logs {id} -f"),
        ]
        .join("\n")
    })
}

struct RawTask {
    prompt: String,
    name: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    template: Option<String>,
}

fn raw(prompt: impl Into<String>) -> RawTask {
    RawTask { prompt: prompt.into(), name: None, model: None, effort: None, template: None }
}

fn norm_task(v: &Value) -> Result<RawTask> {
    match v {
        Value::String(s) => Ok(raw(s.clone())),
        Value::Object(_) => {
            let g = |k: &str| v.get(k).and_then(|x| x.as_str()).map(String::from);
            Ok(RawTask {
                prompt: g("task").or_else(|| g("prompt")).unwrap_or_default(),
                name: g("name"),
                model: g("model"),
                effort: g("effort"),
                template: g("template"),
            })
        }
        _ => Err(DshgoError::new("Invalid task entry in the list", 2)),
    }
}

fn parse_task_file(file: &str) -> Result<Vec<RawTask>> {
    let text = if file == "-" { read_stdin() } else { fs::read_to_string(absolute(file))? };
    let ext = file.rsplit('.').next().unwrap_or("").to_lowercase();
    if ext == "json" {
        let v: Value = serde_json::from_str(&text)?;
        return v.as_array().ok_or_else(|| DshgoError::new("JSON task file must be an array", 2))?.iter().map(norm_task).collect();
    }
    if ext == "jsonl" {
        return text.lines().filter(|l| !l.trim().is_empty()).map(|l| norm_task(&serde_json::from_str::<Value>(l)?)).collect();
    }
    if text.lines().any(|l| l.trim_end() == "---") {
        let mut blocks: Vec<String> = vec![String::new()];
        for line in text.lines() {
            if line.trim_end() == "---" {
                blocks.push(String::new());
            } else {
                let last = blocks.last_mut().unwrap();
                last.push_str(line);
                last.push('\n');
            }
        }
        return Ok(blocks.into_iter().map(|b| b.trim().to_string()).filter(|b| !b.is_empty()).map(raw).collect());
    }
    Ok(text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(raw).collect())
}

fn batch_common(opts: &JobOpts, parallel_opt: Option<&str>, tasks: Vec<TaskItem>, template: &str, default_name: String, kind: &str) -> Result<i32> {
    if tasks.is_empty() {
        return Err(DshgoError::new("No tasks given", 2).hint("go-dsh batch -f tasks.txt or --task \"...\" repeated"));
    }
    if tasks.iter().any(|t| t.prompt.trim().is_empty()) {
        return Err(DshgoError::new("Empty task in the list", 2));
    }
    let (model, effort) = validate_model_opts(opts)?;
    let parallel: u32 = match parallel_opt {
        Some(p) => p.parse().ok().filter(|n| *n >= 1).ok_or_else(|| DshgoError::new("--parallel must be a positive number", 2))?,
        None => tasks.len().min(10) as u32,
    };
    let backend = resolve_backend(opts.backend.as_deref())?;
    let name = opts.name.clone().unwrap_or(default_name);
    let root = resolve_workspace(opts.cwd.as_deref(), &slug(&name, 40), kind)?;
    let count = tasks.len();
    let batch = start_batch(StartBatch {
        name: &name,
        root: &root,
        parallel,
        template,
        tasks,
        timeout_sec: timeout_sec(opts.timeout.as_deref(), 1800)?,
        model,
        effort,
        backend,
    })?;
    let id = batch.id.clone();
    finish_start(Ref { kind: RefKind::Batch, id: id.clone() }, opts.wait, opts.max_wait.as_deref(), || {
        let mut lines = vec![
            format!(
                "Batch {id} started: {count} jobs, {parallel} in parallel ({})",
                if backend == "web" { "in the DSH web UI".to_string() } else { format!("headless, dsh profile {}", active_profile()) }
            ),
            format!("  Root:     {root}"),
            backend_line(backend, &root, 10),
        ];
        for j in list_jobs(Some(&id), None, true) {
            lines.push(format!("  {}  {}", j.id, j.name.unwrap_or_default()));
        }
        lines.push(format!("  Status:   go-dsh status {id}"));
        lines.push(format!("  Wait:     go-dsh wait {id} --timeout 9m"));
        lines.push(format!("  Then:     go-dsh collect {id}  (writes {})", Path::new(&root).join("INDEX.md").display()));
        lines.join("\n")
    })
}

fn cmd_batch(a: BatchArgs) -> Result<i32> {
    let template = a.template.clone().unwrap_or_else(|| "raw".into());
    if !TEMPLATES.contains(&template.as_str()) || template == "materials" {
        return Err(DshgoError::new("--template for batch: raw or research (material: go-dsh materials)", 2));
    }
    let mut raws: Vec<RawTask> = vec![];
    if let Some(f) = &a.file {
        raws.extend(parse_task_file(f)?);
    }
    raws.extend(a.task.iter().map(|t| raw(t.clone())));
    raws.extend(a.tasks.iter().map(|t| raw(t.clone())));
    if raws.is_empty() && !std::io::stdin().is_terminal() {
        raws = parse_task_file("-")?;
    }
    let depth = a.depth.clone().unwrap_or_else(|| "standard".into());
    let default_name = format!("batch-{}", slug(raws.first().map(|t| t.prompt.as_str()).unwrap_or("tasks"), 24));
    let tasks: Vec<TaskItem> = raws
        .into_iter()
        .map(|t| {
            let tpl = t.template.clone().unwrap_or_else(|| template.clone());
            let text = if tpl == "research" { research_prompt(&t.prompt, &depth, None) } else { t.prompt.clone() };
            TaskItem {
                slug: t.name.as_deref().map(|n| slug(n, 32)),
                title: t.name.clone(),
                text,
                prompt: t.prompt,
                template: t.template,
                model: t.model,
                effort: t.effort,
            }
        })
        .collect();
    batch_common(&a.opts, a.parallel.as_deref(), tasks, &template, default_name, "general")
}

fn cmd_research(a: ResearchArgs) -> Result<i32> {
    let depth = a.depth.clone().unwrap_or_else(|| "standard".into());
    if !DEPTHS.contains(&depth.as_str()) {
        return Err(DshgoError::new(format!("--depth must be one of {}", DEPTHS.join(", ")), 2));
    }
    let mut questions: Vec<RawTask> = vec![];
    if let Some(f) = &a.file {
        questions.extend(parse_task_file(f)?);
    }
    questions.extend(a.questions.iter().map(|q| raw(q.clone())));
    if questions.is_empty() && !std::io::stdin().is_terminal() {
        questions = parse_task_file("-")?;
    }
    let default_name = format!("research-{}", slug(questions.first().map(|q| q.prompt.as_str()).unwrap_or(""), 28));
    let tasks: Vec<TaskItem> = questions
        .into_iter()
        .map(|q| TaskItem {
            title: Some(format!("Research: {}", excerpt(q.name.as_deref().unwrap_or(&q.prompt), 70))),
            slug: q.name.as_deref().map(|n| slug(n, 32)),
            text: research_prompt(&q.prompt, &depth, a.focus.as_deref()),
            prompt: q.prompt,
            template: q.template,
            model: q.model,
            effort: q.effort,
        })
        .collect();
    batch_common(&a.opts, a.parallel.as_deref(), tasks, "research", default_name, "research")
}

fn cmd_materials(a: MaterialsArgs) -> Result<i32> {
    let brief = task_from(&a.brief, a.file.as_deref())?.trim().to_string();
    if brief.is_empty() {
        return Err(DshgoError::new("No brief given", 2).hint("go-dsh materials \"topic, purpose, style\""));
    }
    let kinds: Vec<String> =
        a.kinds.as_deref().unwrap_or("screenshots,scroll,articles,stock-video").split(',').map(|k| k.trim().to_string()).filter(|k| !k.is_empty()).collect();
    for k in &kinds {
        if material_description(k).is_none() {
            return Err(DshgoError::new(format!("Unknown kind \"{k}\". Allowed: {}", material_kind_names()), 2));
        }
    }
    let count = a.count.as_deref().map(|c| c.parse::<u32>().unwrap_or(6)).unwrap_or(6);
    let tasks: Vec<TaskItem> = kinds
        .iter()
        .map(|kind| {
            let urls: &[String] = if matches!(kind.as_str(), "screenshots" | "scroll" | "articles") { &a.url } else { &[] };
            let desc = material_description(kind).unwrap_or("");
            TaskItem {
                prompt: format!("{desc}: {brief}"),
                slug: Some(kind.clone()),
                title: Some(format!("Material {kind}: {}", excerpt(&brief, 50))),
                text: materials_prompt(&brief, kind, urls, count).unwrap_or_default(),
                template: None,
                model: None,
                effort: None,
            }
        })
        .collect();
    batch_common(&a.opts, a.parallel.as_deref(), tasks, "materials", format!("material-{}", slug(&brief, 28)), "materials")
}

fn batch_summary_json(b: &Batch, s: &Summary) -> Value {
    json!({ "id": b.id, "name": b.name, "root": b.root, "status": s.status, "counts": s.counts, "total": s.total, "createdAt": b.created_at })
}

fn cmd_status(reference: Option<String>) -> Result<i32> {
    if let Some(r) = reference {
        let r = resolve_ref(Some(&r))?;
        match r.kind {
            RefKind::Job => {
                let job = require_job(&r.id)?;
                emit(json!({ "job": job_view(&job) }), || job_detail_text(&job));
            }
            RefKind::Batch => {
                let view = batch_view(require_batch(&r.id)?);
                emit(json!({ "batch": view.batch, "summary": view.summary, "jobs": view.jobs.iter().map(job_view).collect::<Vec<_>>() }), || batch_text(&view));
            }
        }
        return Ok(0);
    }
    let jobs = list_jobs(None, Some(200), false);
    let running: Vec<&Job> = jobs.iter().filter(|j| is_active(&j.status)).collect();
    let recent: Vec<&Job> = jobs.iter().filter(|j| !is_active(&j.status)).take(8).collect();
    let batches: Vec<(Batch, Summary)> = list_batches(Some(5))
        .into_iter()
        .map(|b| {
            let s = summarize_jobs(&list_jobs(Some(&b.id), None, false));
            (b, s)
        })
        .collect();
    let ui_up = web::available();
    let data = json!({
        "webUi": if ui_up { Some(web::base_url()) } else { None },
        "running": running.iter().map(|j| job_view(j)).collect::<Vec<_>>(),
        "recent": recent.iter().map(|j| job_view(j)).collect::<Vec<_>>(),
        "batches": batches.iter().map(|(b, s)| batch_summary_json(b, s)).collect::<Vec<_>>(),
    });
    emit(data, || {
        let mut out = vec![format!(
            "go-dsh · {} active · new jobs run {}",
            running.len(),
            if ui_up { format!("in the DSH web UI {}", web::base_url()) } else { format!("headless (profile {})", active_profile()) }
        )];
        if !running.is_empty() {
            out.push(String::new());
            out.push("Active".into());
            out.push(format!("  {}", job_header()));
            out.extend(running.iter().map(|j| format!("  {}", job_row(j))));
        }
        if !recent.is_empty() {
            out.push(String::new());
            out.push("Recent".into());
            out.push(format!("  {}", job_header()));
            out.extend(recent.iter().map(|j| format!("  {}", job_row(j))));
        }
        if !batches.is_empty() {
            out.push(String::new());
            out.push("Batches".into());
            for (b, s) in &batches {
                out.push(format!(
                    "  {} {} {} {} {}",
                    pad(&b.id, 16),
                    pad(&s.status, 9),
                    pad(format!("{}/{}", s.counts.get("done").copied().unwrap_or(0), s.total), 7),
                    pad(clock(Some(&b.created_at)), 9),
                    b.name.as_deref().unwrap_or("")
                ));
            }
        }
        if jobs.is_empty() {
            out.push(String::new());
            out.push("No jobs yet. Start with go-dsh run \"task\", or read go-dsh guide".into());
        }
        out.join("\n")
    });
    Ok(0)
}

fn cmd_ls(n: Option<String>, running: bool, batch: Option<String>, batches: bool) -> Result<i32> {
    let limit: usize = n.as_deref().map(|v| v.parse().unwrap_or(20)).unwrap_or(20);
    if batches {
        let list: Vec<(Batch, Summary)> = list_batches(Some(limit))
            .into_iter()
            .map(|b| {
                let s = summarize_jobs(&list_jobs(Some(&b.id), None, false));
                (b, s)
            })
            .collect();
        emit(json!({ "batches": list.iter().map(|(b, s)| batch_summary_json(b, s)).collect::<Vec<_>>() }), || {
            if list.is_empty() {
                return "No batches.".into();
            }
            list.iter()
                .map(|(b, s)| {
                    format!(
                        "{} {} {} {} {}  {}",
                        pad(&b.id, 16),
                        pad(&s.status, 9),
                        pad(format!("{}/{}", s.counts.get("done").copied().unwrap_or(0), s.total), 7),
                        pad(b.created_at.replace('T', " ").chars().take(16).collect::<String>(), 17),
                        b.name.as_deref().unwrap_or(""),
                        b.root
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        });
        return Ok(0);
    }
    let batch_id = match &batch {
        Some(b) => Some(resolve_ref(Some(b))?.id),
        None => None,
    };
    let mut jobs = list_jobs(batch_id.as_deref(), None, batch_id.is_some());
    if running {
        jobs.retain(|j| is_active(&j.status));
    }
    jobs.truncate(limit);
    emit(json!({ "jobs": jobs.iter().map(job_view).collect::<Vec<_>>() }), || {
        if jobs.is_empty() {
            "No jobs.".into()
        } else {
            std::iter::once(job_header()).chain(jobs.iter().map(job_row)).collect::<Vec<_>>().join("\n")
        }
    });
    Ok(0)
}

fn cmd_wait(refs: Vec<String>, timeout: Option<String>, any: bool) -> Result<i32> {
    if refs.is_empty() {
        return Err(DshgoError::new("wait needs at least one reference", 2));
    }
    let resolved: Vec<Ref> = refs.iter().map(|r| resolve_ref(Some(r))).collect::<Result<_>>()?;
    let timeout_ms = match timeout {
        Some(t) => Some(parse_duration(&t)? * 1000),
        None => None,
    };
    let (jobs, timed_out) = wait_for(&resolved, timeout_ms, any)?;
    clear_progress_line();
    let code = wait_outcome(&jobs, timed_out);
    let jobs_json: Vec<Value> = jobs
        .iter()
        .map(|j| {
            let mut v = job_view(j);
            v["result"] = if j.status == "done" { json!(job_result(&j.id)) } else { Value::Null };
            v
        })
        .collect();
    emit(json!({ "timedOut": timed_out, "summary": summarize_jobs(&jobs), "jobs": jobs_json }), || {
        let mut lines = vec![if timed_out { "Timeout reached, unfinished jobs keep running.".to_string() } else { "Done.".to_string() }, job_header()];
        lines.extend(jobs.iter().map(job_row));
        if !timed_out {
            let single = jobs.len() == 1 && jobs[0].status == "done";
            let batch = resolved.iter().find(|r| r.kind == RefKind::Batch);
            lines.push(String::new());
            lines.push(if single {
                format!("Answer: go-dsh result {}", jobs[0].id)
            } else if let Some(b) = batch {
                format!("Summarize: go-dsh collect {}", b.id)
            } else {
                "Answers: go-dsh result <job>".into()
            });
        } else {
            lines.push(String::new());
            lines.push(format!("Keep waiting: go-dsh wait {} --timeout 9m", resolved.iter().map(|r| r.id.clone()).collect::<Vec<_>>().join(" ")));
        }
        lines.join("\n")
    });
    Ok(code)
}

fn cmd_result(reference: Option<String>, files: bool) -> Result<i32> {
    let r = resolve_ref(reference.as_deref())?;
    let jobs = ref_jobs(&r)?;
    if r.kind == RefKind::Job {
        let job = &jobs[0];
        if is_active(&job.status) {
            return Err(DshgoError::new(format!("Job {} is still {}", job.id, job.status), 4).hint(format!("go-dsh wait {}", job.id)));
        }
        let result = job_result(&job.id);
        let dir = job.out().to_string();
        let file_list = files.then(|| list_files(Path::new(&dir), parse_iso_ms(&job.created_at).unwrap_or(0) - 2000, 500));
        emit(json!({ "job": job_view(job), "result": result, "files": file_list }), || {
            let mut parts = vec![result
                .clone()
                .unwrap_or_else(|| format!("(no answer) status {}{}", job.status, job.error.as_ref().map(|e| format!(": {e}")).unwrap_or_default()))];
            if let Some(fl) = &file_list {
                parts.push(String::new());
                parts.push(format!("Files in {dir}:"));
                parts.extend(fl.iter().map(|f| format!("  {} ({})", f.path, fmt_bytes(f.bytes))));
            }
            parts.join("\n")
        });
        return Ok(if job.status == "done" { 0 } else { 1 });
    }
    let jobs_json: Vec<Value> = jobs
        .iter()
        .map(|j| {
            let mut v = job_view(j);
            v["result"] = json!(job_result(&j.id));
            v
        })
        .collect();
    emit(json!({ "jobs": jobs_json }), || {
        jobs.iter()
            .map(|j| {
                format!(
                    "## {} [{}] ({})\n\n{}\n",
                    j.title.as_deref().or(j.name.as_deref()).unwrap_or(&j.id),
                    j.status,
                    j.id,
                    job_result(&j.id).or_else(|| j.error.clone()).unwrap_or_else(|| "(no answer)".into())
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    });
    Ok(if jobs.iter().all(|j| j.status == "done") { 0 } else { 1 })
}

fn cmd_logs(job: Option<String>, n: Option<String>, follow: bool, full: bool, raw_mode: bool, stderr: bool) -> Result<i32> {
    let r = resolve_ref(job.as_deref())?;
    if r.kind != RefKind::Job {
        return Err(DshgoError::new("logs needs a job, not a batch", 2).hint(format!("go-dsh status {} lists the jobs of the batch", r.id)));
    }
    let dir = job_dir(&r.id);
    if stderr {
        let f = dir.join("stderr.log");
        print(&if f.exists() { fs::read_to_string(&f)? } else { "(empty)".into() });
        return Ok(0);
    }
    let events_file = dir.join("events.jsonl");
    let read = || fs::read_to_string(&events_file).unwrap_or_default();
    let render = |text: &str| -> Vec<String> {
        if raw_mode {
            text.lines().filter(|l| !l.is_empty()).map(String::from).collect()
        } else {
            timeline(&parse_lines(text), full)
        }
    };
    let limit: usize = n.as_deref().map(|v| v.parse().unwrap_or(40)).unwrap_or(if follow { 15 } else { 40 });
    let mut lines = render(&read());
    let tail = |l: &[String]| l[l.len().saturating_sub(limit)..].join("\n");
    if json_mode() && !follow {
        let start = lines.len().saturating_sub(limit);
        emit(json!({ "job": r.id, "lines": lines[start..] }), String::new);
        return Ok(0);
    }
    let shown_text = tail(&lines);
    print(&if shown_text.is_empty() { "(no events yet)".to_string() } else { shown_text });
    if !follow {
        return Ok(0);
    }
    let mut shown = lines.len();
    loop {
        sleep_ms(1000);
        lines = render(&read());
        if lines.len() > shown {
            print(&lines[shown..].join("\n"));
            shown = lines.len();
        }
        let j = require_job(&r.id)?;
        if is_terminal(&j.status) {
            print(&format!("[{} {} after {}]", j.id, j.status, fmt_duration(job_duration(&j))));
            return Ok(if j.status == "done" { 0 } else { 1 });
        }
    }
}

fn cmd_files(reference: Option<String>, all: bool) -> Result<i32> {
    let r = resolve_ref(reference.as_deref())?;
    let jobs = ref_jobs(&r)?;
    let data: Vec<(String, String, Vec<crate::collect::FileInfo>)> = jobs
        .iter()
        .map(|j| {
            (j.id.clone(), j.out().to_string(), list_files(Path::new(j.out()), if all { 0 } else { parse_iso_ms(&j.created_at).unwrap_or(0) - 2000 }, 500))
        })
        .collect();
    emit(json!({ "jobs": data.iter().map(|(id, cwd, files)| json!({ "job": id, "cwd": cwd, "files": files })).collect::<Vec<_>>() }), || {
        data.iter()
            .map(|(id, cwd, files)| {
                let mut lines = vec![format!("{id}  {cwd}")];
                if files.is_empty() {
                    lines.push("  (no files)".into());
                } else {
                    lines.extend(files.iter().map(|f| format!("  {} {}", pad(fmt_bytes(f.bytes), 10), f.path)));
                }
                lines.join("\n")
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    });
    Ok(0)
}

struct ContinueArgs {
    text: Vec<String>,
    file: Option<String>,
    wait: bool,
    timeout: Option<String>,
    max_wait: Option<String>,
    parallel: Option<String>,
    only_failed: bool,
}

/// Continue every finished job of a batch with the same follow-up; the continuations form a new batch.
fn continue_batch(old_id: &str, prompt: &str, a: &ContinueArgs) -> Result<i32> {
    let old = require_batch(old_id)?;
    let mut targets: Vec<Job> = vec![];
    let mut skipped: Vec<(String, &str)> = vec![];
    for j in list_jobs(Some(old_id), None, true) {
        if is_active(&j.status) {
            skipped.push((j.id, "still running"));
        } else if a.only_failed && j.status == "done" {
            skipped.push((j.id, "already done"));
        } else if j.session_id.is_none() {
            skipped.push((j.id, "no session id"));
        } else {
            targets.push(j);
        }
    }
    if targets.is_empty() {
        let why = skipped.iter().map(|(id, r)| format!("{id}: {r}")).collect::<Vec<_>>().join("; ");
        return Err(DshgoError::new(format!("No job of batch {old_id} can be continued ({why})"), 1).hint(format!("go-dsh status {old_id}")));
    }
    if targets.iter().any(|j| j.backend.as_deref() == Some("web")) && !web::available() {
        return Err(DshgoError::new("The sessions live in the DSH web UI, which is not reachable right now", 5).hint("go-dsh ui start"));
    }
    let parallel: u32 = match &a.parallel {
        Some(p) => p.parse().ok().filter(|n| *n >= 1).ok_or_else(|| DshgoError::new("--parallel must be a positive number", 2))?,
        None => targets.len().min(10) as u32,
    };
    let new_batch = create_batch(|b| {
        b.name = Some(format!("{}+", old.name.as_deref().unwrap_or(&old.id)));
        b.root = old.root.clone();
        b.parallel = parallel;
        b.template = "raw".into();
    })?;
    let mut ids = vec![];
    for (i, p) in targets.iter().enumerate() {
        let job = make_job(
            JobSpec {
                name: Some(format!("{}+", p.name.as_deref().unwrap_or(&p.id))),
                title: p.title.clone(),
                template: "raw".into(),
                prompt: prompt.to_string(),
                cwd: p.cwd.clone(),
                out_dir: Some(p.out().to_string()),
                backend: p.backend.clone().unwrap_or_else(|| "headless".into()),
                model: p.model.clone(),
                effort: p.effort.clone(),
                timeout_sec: timeout_sec(a.timeout.as_deref(), p.timeout_sec)?,
                batch: Some(new_batch.id.clone()),
                index: Some(i as u32 + 1),
                parent: Some(p.id.clone()),
                resume: p.session_id.clone(),
                web: p.web.clone(),
            },
            prompt,
        )?;
        ids.push(job.id);
    }
    let batch = launch_batch(&new_batch.id, ids)?;
    let id = batch.id.clone();
    finish_start(Ref { kind: RefKind::Batch, id: id.clone() }, a.wait, a.max_wait.as_deref(), || {
        let mut lines = vec![format!("Batch {id} continues {} job(s) of {old_id}, {parallel} in parallel", targets.len())];
        for j in list_jobs(Some(&id), None, true) {
            lines.push(format!("  {}  {} (follows {})", j.id, j.name.unwrap_or_default(), j.parent.unwrap_or_default()));
        }
        for (sid, why) in &skipped {
            lines.push(format!("  skipped {sid}: {why}"));
        }
        lines.push(format!("  Wait:     go-dsh wait {id} --timeout 9m"));
        lines.push(format!("  Then:     go-dsh collect {id}"));
        lines.join("\n")
    })
}

fn cmd_continue(job: Option<String>, a: ContinueArgs) -> Result<i32> {
    let ContinueArgs { text, file, wait, timeout, max_wait, .. } = &a;
    let (wait, timeout, max_wait) = (*wait, timeout.clone(), max_wait.clone());
    let r = resolve_ref(job.as_deref())?;
    if r.kind == RefKind::Batch {
        let prompt = task_from(text, file.as_deref())?.trim().to_string();
        if prompt.is_empty() {
            return Err(DshgoError::new("No follow-up task given", 2));
        }
        return continue_batch(&r.id, &prompt, &a);
    }
    let (text, file) = (text.clone(), file.clone());
    let parent = require_job(&r.id)?;
    if is_active(&parent.status) {
        return Err(DshgoError::new(format!("Job {} is still running", parent.id), 4).hint(format!("go-dsh wait {}", parent.id)));
    }
    let session = parent.session_id.clone().ok_or_else(|| DshgoError::new(format!("Job {} has no session id, it cannot be continued", parent.id), 1))?;
    let prompt = task_from(&text, file.as_deref())?.trim().to_string();
    if prompt.is_empty() {
        return Err(DshgoError::new("No follow-up task given", 2));
    }
    let backend = parent.backend.clone().unwrap_or_else(|| "headless".into());
    if backend == "web" && !web::available() {
        return Err(DshgoError::new("The session lives in the DSH web UI, which is not reachable right now", 5).hint("go-dsh ui start"));
    }
    let new_job = start_job(
        JobSpec {
            name: Some(format!("{}+", parent.name.as_deref().unwrap_or(&parent.id))),
            title: parent.title.clone(),
            template: "raw".into(),
            prompt: prompt.clone(),
            cwd: parent.cwd.clone(),
            out_dir: Some(parent.out().to_string()),
            backend,
            model: parent.model.clone(),
            effort: parent.effort.clone(),
            timeout_sec: timeout_sec(timeout.as_deref(), parent.timeout_sec)?,
            batch: None,
            index: None,
            parent: Some(parent.id.clone()),
            resume: Some(session.clone()),
            web: parent.web.clone(),
        },
        &prompt,
        false,
    )?;
    let id = new_job.id.clone();
    finish_start(Ref { kind: RefKind::Job, id: id.clone() }, wait, max_wait.as_deref(), || {
        format!("Job {id} continues session {session} (workspace {})\n  Wait: go-dsh wait {id} --timeout 9m", parent.cwd)
    })
}

fn cmd_cancel(reference: Option<String>) -> Result<i32> {
    let r = resolve_ref(reference.as_deref())?;
    let n = match r.kind {
        RefKind::Job => usize::from(cancel_job(&r.id)?),
        RefKind::Batch => cancel_batch(&r.id)?,
    };
    let (jobs, _) = wait_for(&[r], Some(8000), false)?;
    clear_progress_line();
    emit(json!({ "cancelled": n, "jobs": jobs.iter().map(job_view).collect::<Vec<_>>() }), || {
        let mut lines = vec![format!("{n} job(s) cancelled."), job_header()];
        lines.extend(jobs.iter().map(job_row));
        lines.join("\n")
    });
    Ok(0)
}

fn cmd_collect(batch: Option<String>, output: Option<String>) -> Result<i32> {
    let r = resolve_ref(Some(batch.as_deref().unwrap_or("last-batch")))?;
    if r.kind != RefKind::Batch {
        return Err(DshgoError::new("collect needs a batch", 2));
    }
    let res = collect_batch(&r.id, output.map(|o| absolute(&o)))?;
    emit(serde_json::to_value(&res)?, || {
        let mut lines = vec![format!("INDEX written:  {}", res.index)];
        if let Some(m) = &res.manifest {
            lines.push(format!("Manifest:       {m} ({} files)", res.material_files));
        }
        if let Some(s) = &res.sources {
            lines.push(format!("Sources:        {s} ({} URLs)", res.source_count));
        }
        lines.push(format!("Status: {} {}", res.status, serde_json::to_string(&res.counts).unwrap_or_default()));
        lines.join("\n")
    });
    Ok(0)
}

const WS_HELP: &str = "go-dsh ws init <dir> [--kind research|materials|general] [--description text]\n  Creates a workspace: folder, marker .dshgo-workspace and an AGENTS.md with conventions.";

fn cmd_ws(sub: Option<String>, dir: Option<String>, kind: Option<String>, description: Option<String>) -> Result<i32> {
    if sub.as_deref() != Some("init") {
        print(WS_HELP);
        return Ok(if sub.is_none() { 2 } else { 0 });
    }
    let dir = dir.ok_or_else(|| DshgoError::new("ws init needs a folder", 2))?;
    let (path, _) = ensure_workspace(&absolute(&dir), kind.as_deref().unwrap_or("general"), description.as_deref(), true)?;
    emit(json!({ "workspace": path.display().to_string(), "agentsMd": path.join("AGENTS.md").display().to_string() }), || {
        format!("Workspace ready: {}\n  Start: go-dsh run -C {} \"task\"", path.display(), path.display())
    });
    Ok(0)
}

const CAPTURE_HELP: &str = "go-dsh capture shot <url> -o <file.png> [--full] [--width 1440] [--height 900] [--wait 2] [--selector css]\ngo-dsh capture scroll <url> -o <file.mp4> [--duration 12] [--width 1920] [--height 1080] [--fps 30] [--speed 450]\n  Deterministic captures with Playwright (bundled Chromium). Prints JSON.\n  Default: dismiss cookie banners, hide overlays, block ad networks, WebGL off.\n  --no-dismiss, --keep-overlays, --allow-ads, --hide <css> (repeatable), --dark, --scale 2 (shot)\n  --max-time <s> hard limit (default: shot 90, scroll 180 plus 12 per second of video)";

fn cmd_capture(args: Vec<String>) -> Result<i32> {
    if args.is_empty() || args[0] == "-h" || args[0] == "--help" {
        print(CAPTURE_HELP);
        return Ok(if args.is_empty() { 2 } else { 0 });
    }
    let py = find_python();
    if py.playwright.is_none() {
        return Err(
            DshgoError::new("Python Playwright not found", 5).hint("pip install playwright && python -m playwright install chromium, or set DSHGO_PYTHON")
        );
    }
    let script = home().join("capture.py");
    fs::create_dir_all(home())?;
    if fs::read_to_string(&script).map(|s| s != CAPTURE_PY).unwrap_or(true) {
        fs::write(&script, CAPTURE_PY)?;
    }
    let passthrough: Vec<&String> = args.iter().filter(|a| a.as_str() != "--json").collect();
    let status =
        Command::new(&py.python).arg(&script).args(passthrough).status().map_err(|e| DshgoError::new(format!("cannot start {}: {e}", py.python), 5))?;
    Ok(status.code().unwrap_or(1))
}

fn open_in_browser(url: &str) {
    #[cfg(windows)]
    {
        let _ = Command::new("cmd").args(["/C", "start", "", url]).status();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("open").arg(url).status();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = Command::new("xdg-open").arg(url).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
    }
}

/// Stop the local DSH web UI: find the process listening on its port and end it (with its children).
fn cmd_ui_stop(force: bool) -> Result<i32> {
    let url = web::base_url();
    if !web::base_is_local() {
        return Err(DshgoError::new(format!("{url} is not on this machine, go-dsh can only stop a local DSH web UI"), 2));
    }
    // Only a port that really answers as the DSH web UI is touched, never an unrelated listener.
    if !web::available() {
        emit(json!({ "url": url, "stopped": false, "running": false }), || format!("DSH web UI at {url} is not running"));
        return Ok(0);
    }
    let port = web::base_port().ok_or_else(|| DshgoError::new(format!("no port in {url}"), 2))?;
    let pids = web::listener_pids(port);
    if pids.is_empty() {
        return Err(DshgoError::new(format!("DSH web UI answers at {url}, but no local process listens on port {port}"), 1)
            .hint("stop it where it was started (Ctrl+C in its terminal)"));
    }
    let running: Vec<Job> = list_jobs(None, Some(500), false).into_iter().filter(|j| is_active(&j.status) && j.backend.as_deref() == Some("web")).collect();
    if !running.is_empty() && !force {
        let ids = running.iter().map(|j| j.id.clone()).collect::<Vec<_>>().join(", ");
        return Err(DshgoError::new(format!("{} web job(s) are still running and would be lost: {ids}", running.len()), 1)
            .hint("wait for them (go-dsh wait <id>), cancel them (go-dsh cancel <id>), or use --force"));
    }
    for pid in &pids {
        terminate(*pid);
    }
    let deadline = Instant::now() + std::time::Duration::from_secs(8);
    while Instant::now() < deadline && (web::available() || pids.iter().any(|p| pid_alive(*p))) {
        sleep_ms(250);
    }
    for pid in &pids {
        if pid_alive(*pid) {
            force_kill(*pid);
        }
    }
    sleep_ms(300);
    if web::available() {
        return Err(DshgoError::new(
            format!("DSH web UI at {url} still answers after stopping pid {}", pids.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ")),
            1,
        ));
    }
    emit(json!({ "url": url, "stopped": true, "pids": pids, "interruptedJobs": running.len() }), || {
        let list = pids.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ");
        format!("DSH web UI at {url} stopped (pid {list}). New jobs run headless; start it again with go-dsh ui start.")
    });
    Ok(0)
}

fn cmd_ui(args: Vec<String>, print_only: bool, port: Option<String>, open: bool, force: bool) -> Result<i32> {
    match args.first().map(String::as_str) {
        Some("stop") => cmd_ui_stop(force),
        Some("start") => {
            if web::available() {
                if open {
                    open_in_browser(&web::browser_url());
                }
                emit(json!({ "url": web::base_url(), "started": false, "opened": open }), || {
                    format!("DSH web UI already running at {}{}", web::base_url(), if open { " (opened in the browser)" } else { "" })
                });
                return Ok(0);
            }
            let port: u16 = port.as_deref().map(|p| p.parse().unwrap_or(3080)).unwrap_or(3080);
            let res = web::start_server(port, 120_000)?;
            if open {
                open_in_browser(&web::browser_url());
            }
            emit(json!({ "url": res.url, "started": true, "opened": open, "pid": res.pid, "log": res.log }), || {
                format!(
                    "DSH web UI started at {} (pid {}); jobs now run there.{}\n  Log: {}",
                    res.url,
                    res.pid,
                    if open { " Opened in the browser." } else { "" },
                    res.log
                )
            });
            Ok(0)
        }
        Some("login") => {
            let Some(url) = args.get(1) else {
                print("go-dsh ui login <url>   remember the token URL that `dsh web` printed");
                return Ok(2);
            };
            let origin = web::save_token_url(url)?;
            std::env::set_var("DSHGO_WEB_URL", &origin);
            if web::exchange_token(Some(url)).is_none() {
                return Err(DshgoError::new(format!("the token was not accepted by {origin}"), 5).hint("copy the newest URL printed by dsh web"));
            }
            emit(json!({ "url": origin }), || format!("Logged in to the DSH web UI at {origin}; the cookie stays valid for 30 days."));
            Ok(0)
        }
        _ => {
            if !web::available() {
                return Err(DshgoError::new(format!("DSH web UI at {} is not running", web::base_url()), 5).hint("go-dsh ui start"));
            }
            let mut where_: Option<String> = None;
            if let Some(reference) = args.first() {
                let r = resolve_ref(Some(reference))?;
                let jobs = ref_jobs(&r)?;
                where_ = Some(match jobs.first() {
                    Some(first) if first.backend.as_deref() == Some("web") => {
                        let ws = first.web.as_ref().and_then(|w| w.workspace_title.clone()).unwrap_or_else(|| base_name(&first.cwd));
                        let titles =
                            jobs.iter().map(|j| format!("\"{}\"", j.title.as_deref().or(j.name.as_deref()).unwrap_or(""))).collect::<Vec<_>>().join(", ");
                        format!("workspace \"{ws}\", session{} {titles}", if jobs.len() > 1 { "s" } else { "" })
                    }
                    _ => "This job ran headless: look under \"Ungrouped\".".into(),
                });
            }
            if !print_only {
                open_in_browser(&web::browser_url());
            }
            emit(json!({ "url": web::base_url(), "where": where_ }), || {
                let mut lines = vec![format!("DSH web UI: {}{}", web::base_url(), if print_only { "" } else { " (opened in the browser)" })];
                if let Some(w) = &where_ {
                    lines.push(format!("  Look for: {w}"));
                }
                lines.join("\n")
            });
            Ok(0)
        }
    }
}

fn cmd_skill(sub: Option<String>, agents: Option<String>, copy: bool) -> Result<i32> {
    if sub.as_deref() != Some("install") {
        print("go-dsh skill install [--agents claude,codex,pi] [--copy]\n  Installs the go-dsh-delegation skill into ~/.agents/skills (read by OpenCode and others)\n  and links it into Claude Code, Codex and Pi when they are installed.");
        return Ok(if sub.is_none() { 2 } else { 0 });
    }
    let list: Option<Vec<String>> = agents.map(|a| a.split(',').map(|s| s.trim().to_string()).collect());
    let res = install_skill(list.as_deref(), copy)?;
    emit(serde_json::to_value(&res)?, || {
        let mut lines = vec![format!("Skill go-dsh-delegation installed: {}", res.canonical)];
        lines.extend(res.rows.iter().skip(1).map(|r| format!("  {} {}: {}", pad(&r.agent, 8), r.action, r.path)));
        lines.join("\n")
    });
    Ok(0)
}

fn cmd_setup(mcp: Option<String>, from: Option<String>, skills: Option<String>, no_skill: bool) -> Result<i32> {
    let profile = setup_profile(mcp.as_deref().unwrap_or("research"), from.as_deref().unwrap_or("web"), skills.as_deref())?;
    let skill = if no_skill { None } else { Some(install_skill(None, false)?) };
    let up = web::available();
    emit(json!({ "profile": profile, "skill": skill, "webUi": if up { Some(web::base_url()) } else { None } }), || {
        let mut lines = vec![
            format!("dsh profile \"{}\" ready: {}", profile.profile, profile.profile_dir),
            format!("  MCP servers: {}", if profile.mcp_servers.is_empty() { "none".to_string() } else { profile.mcp_servers.join(", ") }),
            format!(
                "  Skills:      {}",
                match &profile.skills_dir {
                    Some(d) => format!("{} linked in {d}", profile.skills.len()),
                    None => "dsh defaults".into(),
                }
            ),
        ];
        lines.extend(profile.notes.iter().map(|n| format!("  Note:        {n}")));
        if let Some(s) = &skill {
            lines.push(format!("Agent skill installed: {}", s.canonical));
            lines.extend(s.rows.iter().skip(1).map(|r| format!("  {} {}", pad(&r.agent, 8), r.action)));
        }
        lines.push(if up {
            format!("DSH web UI found at {}: jobs will run there.", web::base_url())
        } else {
            "DSH web UI not running: jobs run headless. Start it with go-dsh ui start to watch them live.".into()
        });
        lines.join("\n")
    });
    Ok(0)
}

fn cmd_doctor() -> Result<i32> {
    let mut checks = doctor();
    let up = web::available();
    checks.push(crate::profile::Check {
        name: "web-ui".into(),
        ok: up,
        optional: true,
        detail: if up {
            format!("{} reachable: jobs run live in the DSH UI", web::base_url())
        } else {
            format!("{} not reachable: jobs run headless (go-dsh ui start)", web::base_url())
        },
    });
    let ok = checks.iter().all(|c| c.ok || c.optional);
    emit(json!({ "healthy": ok, "checks": checks }), || {
        checks
            .iter()
            .map(|c| {
                format!(
                    "{} {} {}",
                    if c.ok {
                        "✓"
                    } else if c.optional {
                        "!"
                    } else {
                        "✗"
                    },
                    pad(&c.name, 12),
                    c.detail
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    });
    Ok(if ok { 0 } else { 5 })
}

fn cmd_config(args: Vec<String>) -> Result<i32> {
    let args: Vec<String> = args.into_iter().filter(|a| a != "--json").collect();
    match args.first().map(String::as_str) {
        Some("-h") | Some("--help") => {
            let keys = CONFIG_KEYS.iter().map(|k| format!("{} ({})", k.key, k.help)).collect::<Vec<_>>().join("; ");
            print(&format!("go-dsh config                       show settings\ngo-dsh config set <key> <value>      e.g. go-dsh config set dshRepo ~/deepseek-harness\ngo-dsh config unset <key>\n  Keys: {keys}\n  Environment variables (DSHGO_*) win over config.json."));
            return Ok(0);
        }
        Some("set") => {
            if args.len() < 3 {
                return Err(DshgoError::new("usage: go-dsh config set <key> <value>", 2));
            }
            set_config(&args[1], Some(&args[2..].join(" ")))?;
        }
        Some("unset") => {
            if args.len() < 2 {
                return Err(DshgoError::new("usage: go-dsh config unset <key>", 2));
            }
            set_config(&args[1], None)?;
        }
        Some(other) => return Err(DshgoError::new(format!("Unknown config subcommand \"{other}\""), 2).hint("go-dsh config --help")),
        None => {}
    }
    let stored = read_config();
    let mut effective = Map::new();
    for k in CONFIG_KEYS {
        effective.insert(k.key.into(), cfg(k.key).map(Value::String).unwrap_or(Value::Null));
    }
    emit(json!({ "stored": stored, "effective": effective }), || {
        CONFIG_KEYS
            .iter()
            .map(|k| {
                let def = key_def(k.key);
                let from_env = def.map(|d| std::env::var(d.env).map(|v| !v.is_empty()).unwrap_or(false)).unwrap_or(false);
                format!(
                    "{} {}{}",
                    pad(k.key, 11),
                    cfg(k.key).unwrap_or_else(|| "-".into()),
                    if from_env { format!("  (from {})", k.env) } else { String::new() }
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    });
    Ok(0)
}

fn dispatch(cmd: Cmd) -> Result<i32> {
    match cmd {
        Cmd::Run(a) => cmd_run(a),
        Cmd::Batch(a) => cmd_batch(a),
        Cmd::Research(a) => cmd_research(a),
        Cmd::Materials(a) => cmd_materials(a),
        Cmd::Status { reference } => cmd_status(reference),
        Cmd::Ls { n, running, batch, batches } => cmd_ls(n, running, batch, batches),
        Cmd::Wait { refs, timeout, any } => cmd_wait(refs, timeout, any),
        Cmd::Result { reference, files } => cmd_result(reference, files),
        Cmd::Logs { job, n, follow, full, raw, stderr } => cmd_logs(job, n, follow, full, raw, stderr),
        Cmd::Files { reference, all } => cmd_files(reference, all),
        Cmd::Continue { job, text, file, wait, timeout, max_wait, parallel, only_failed } => {
            cmd_continue(job, ContinueArgs { text, file, wait, timeout, max_wait, parallel, only_failed })
        }
        Cmd::Cancel { reference } => cmd_cancel(reference),
        Cmd::Collect { batch, output } => cmd_collect(batch, output),
        Cmd::Ws { sub, dir, kind, description } => cmd_ws(sub, dir, kind, description),
        Cmd::Capture { args } => cmd_capture(args),
        Cmd::Ui { args, print, port, open, force } => cmd_ui(args, print, port, open, force),
        Cmd::Skill { sub, agents, copy } => cmd_skill(sub, agents, copy),
        Cmd::Setup { mcp, from, skills, no_skill } => cmd_setup(mcp, from, skills, no_skill),
        Cmd::Doctor => cmd_doctor(),
        Cmd::Config { args } => cmd_config(args),
        Cmd::Guide => {
            print(GUIDE);
            Ok(0)
        }
        Cmd::RunJob { id } => {
            install_signal_forwarding();
            run_job(&id)?;
            Ok(0)
        }
        Cmd::RunBatch { id } => {
            install_signal_forwarding();
            run_batch(&id, 400)?;
            Ok(0)
        }
    }
}

fn report_error(err: &DshgoError) {
    if json_mode() {
        print(&json!({ "ok": false, "error": err.message, "code": err.code, "hint": err.hint }).to_string());
    } else {
        eprintln!("go-dsh: {}", err.message);
        if let Some(h) = &err.hint {
            eprintln!("  hint: {h}");
        }
    }
}

/// Entry point shared by the `go-dsh` and `dshgo` binaries. Returns the exit code.
pub fn main_with_args(args: Vec<String>) -> i32 {
    JSON_MODE.store(args.iter().any(|a| a == "--json"), Ordering::Relaxed);
    let cli = match Cli::try_parse_from(&args) {
        Ok(c) => c,
        Err(e) => {
            use clap::error::ErrorKind;
            return match e.kind() {
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
                    let _ = e.print();
                    0
                }
                ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
                    let _ = e.print();
                    2
                }
                _ => {
                    if json_mode() {
                        report_error(
                            &DshgoError::new(e.render().to_string().lines().next().unwrap_or("usage error").trim_start_matches("error: ").to_string(), 2)
                                .hint("go-dsh <command> --help lists the options"),
                        );
                    } else {
                        let _ = e.print();
                    }
                    2
                }
            };
        }
    };
    match dispatch(cli.cmd) {
        Ok(code) => code,
        Err(err) => {
            report_error(&err);
            err.code
        }
    }
}

/// Print the top-level help (used when no command is given).
pub fn print_usage() {
    let _ = Cli::command().print_help();
}
