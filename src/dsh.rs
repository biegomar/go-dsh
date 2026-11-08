//! Locating and launching DeepSeek Harness (dsh).
//!
//! Resolution order:
//!   1. `DSHGO_DSH_CMD`  a replacement executable taking `--json [--session-id id] -` (tests)
//!   2. `dshRepo`       a source checkout, launched through its own tsx
//!   3. `dshBin` or `dsh` on PATH (`npm i -g @deepseek-ai/dsh`)
//!   4. `~/deepseek-harness` as a source checkout

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::config::cfg;
use crate::paths::{dsh_home, profile_dir, PROFILE};
use crate::util::{home_dir, DshgoError, Result};

pub const EFFORTS: &[&str] = &["off", "low", "high", "max"];

pub fn resolve_model(model: Option<&str>) -> Option<String> {
    let m = model.filter(|m| !m.is_empty())?;
    Some(
        match m {
            "flash" => "deepseek-flash",
            "v4-flash" => "deepseek-v4-flash",
            "pro" | "v4-pro" => "deepseek-v4-pro",
            "vision" => "deepseek-v4-flash-vision-exp",
            other => other,
        }
        .to_string(),
    )
}

#[derive(Debug, Clone)]
pub struct Dsh {
    pub kind: &'static str,
    pub where_: String,
    pub cmd: PathBuf,
    pub prefix: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// First executable named `name` on PATH (honours PATHEXT on Windows).
pub fn which(name: &str) -> Option<PathBuf> {
    ::which::which(name).ok()
}

fn file_url(path: &Path) -> String {
    let p = path.display().to_string().replace('\\', "/");
    let encoded = p.replace('%', "%25").replace(' ', "%20");
    if encoded.starts_with('/') {
        format!("file://{encoded}")
    } else {
        format!("file:///{encoded}")
    }
}

fn source_launch(repo: &str) -> Dsh {
    let repo_path = PathBuf::from(repo);
    let tsx = repo_path.join("node_modules").join("tsx").join("dist").join("esm").join("index.mjs");
    Dsh {
        kind: "source",
        where_: repo.to_string(),
        cmd: which("node").unwrap_or_else(|| PathBuf::from("node")),
        prefix: vec!["--import".into(), file_url(&tsx), repo_path.join("apps").join("cli").join("src").join("bin.ts").display().to_string()],
        // Without this tsx resolves stale lib/ builds whenever the cwd is outside the repo.
        env: vec![("TSX_TSCONFIG_PATH".into(), repo_path.join("tsconfig.json").display().to_string())],
    }
}

fn is_checkout(repo: &Path) -> bool {
    repo.join("apps").join("cli").join("src").join("bin.ts").exists() && repo.join("node_modules").join("tsx").exists()
}

/// How to start dsh on this machine, or `None` when it is not installed.
pub fn resolve_dsh() -> Option<Dsh> {
    if let Some(custom) = std::env::var("DSHGO_DSH_CMD").ok().filter(|v| !v.is_empty()) {
        return Some(Dsh { kind: "custom", where_: custom.clone(), cmd: PathBuf::from(custom), prefix: vec![], env: vec![] });
    }
    if let Some(repo) = cfg("dshRepo") {
        return Some(source_launch(&repo));
    }
    if let Some(bin) = cfg("dshBin").map(PathBuf::from).or_else(|| which("dsh")) {
        return Some(Dsh { kind: "binary", where_: bin.display().to_string(), cmd: bin, prefix: vec![], env: vec![] });
    }
    let fallback = home_dir()?.join("deepseek-harness");
    if is_checkout(&fallback) {
        return Some(source_launch(&fallback.display().to_string()));
    }
    None
}

pub fn require_dsh() -> Result<Dsh> {
    resolve_dsh().ok_or_else(|| {
        DshgoError::new("DeepSeek Harness (dsh) not found", 5).hint("npm install -g @deepseek-ai/dsh, or go-dsh config set dshBin|dshRepo <path>")
    })
}

pub struct DshCommand {
    pub cmd: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub dsh: Dsh,
}

/// Full command line for `dsh <args>`.
pub fn dsh_command(args: &[&str]) -> Result<DshCommand> {
    let dsh = require_dsh()?;
    let mut all = dsh.prefix.clone();
    all.extend(args.iter().map(|a| a.to_string()));
    Ok(DshCommand { cmd: dsh.cmd.clone(), args: all, env: dsh.env.clone(), dsh })
}

/// dsh `settings.yaml` as JSON (an absent file yields an empty object).
pub fn read_dsh_settings() -> Result<Value> {
    let file = dsh_home().join("settings.yaml");
    if !file.exists() {
        return Ok(Value::Object(Map::new()));
    }
    let text = fs::read_to_string(&file)?;
    let v: Value = serde_yaml::from_str(&text).map_err(|e| DshgoError::new(format!("{}: {e}", file.display()), 1))?;
    Ok(if v.is_object() { v } else { Value::Object(Map::new()) })
}

pub fn active_profile() -> &'static str {
    if profile_dir(PROFILE).join("package.json").exists() {
        PROFILE
    } else {
        "headless"
    }
}

/// The `agent-default-model` block for a job with overrides applied.
pub fn model_selection(model: Option<&str>, effort: Option<&str>) -> Result<Value> {
    if let Some(e) = effort {
        if !EFFORTS.contains(&e) {
            return Err(DshgoError::new(format!("Invalid effort \"{e}\". Allowed: {}", EFFORTS.join(", ")), 2));
        }
    }
    let settings = read_dsh_settings()?;
    let current = settings.get("agent-default-model").cloned().unwrap_or(Value::Null);
    let cur = |k: &str| current.get(k).and_then(|v| v.as_str()).map(String::from);
    let reasoning = effort.map(String::from).or_else(|| cur("reasoningEffort"));
    let mut sel = json!({
        "provider": cur("provider").unwrap_or_else(|| "deepseek-official".into()),
        "model": resolve_model(model).or_else(|| cur("model")).unwrap_or_else(|| "deepseek-flash".into()),
    });
    if let Some(r) = reasoning {
        sel["reasoningEffort"] = Value::String(r);
    }
    Ok(sel)
}

/// Per-job settings copy with a different default model; returns the patch path.
fn write_settings_override(job_dir: &Path, model: Option<&str>, effort: Option<&str>) -> Result<PathBuf> {
    let mut settings = read_dsh_settings()?;
    settings["agent-default-model"] = model_selection(model, effort)?;
    let settings_path = job_dir.join("settings.yaml");
    // JSON is valid YAML, so the dsh settings loader reads it unchanged.
    crate::util::write_private(&settings_path, &(serde_json::to_string_pretty(&settings)? + "\n"))?;
    let patch_path = job_dir.join("settings.patch.yml");
    fs::write(
        &patch_path,
        format!(
            "# go-dsh: per-job settings with a model override\n- id: settings\n  config:\n    path: {}\n    watch: false\n",
            serde_json::to_string(&settings_path.display().to_string())?
        ),
    )?;
    Ok(patch_path)
}

pub struct SpawnSpec {
    pub cmd: PathBuf,
    pub args: Vec<String>,
    pub extra_env: Vec<(String, String)>,
    pub profile: String,
}

/// Command, args and env for one headless run. The task always goes through
/// stdin (positional `-`), so leading dashes and long prompts are safe.
pub fn spawn_spec(session_id: Option<&str>, model: Option<&str>, effort: Option<&str>, job_dir: &Path) -> Result<SpawnSpec> {
    let mut tail = vec!["--json".to_string()];
    if let Some(s) = session_id {
        tail.push("--session-id".into());
        tail.push(s.into());
    }
    tail.push("-".into());
    let dsh = require_dsh()?;
    if dsh.kind == "custom" {
        return Ok(SpawnSpec { cmd: dsh.cmd, args: tail, extra_env: vec![], profile: "custom".into() });
    }
    let profile = active_profile();
    let mut args = dsh.prefix.clone();
    args.push("--profile".into());
    args.push(profile.into());
    if model.is_some() || effort.is_some() {
        args.push("--patch".into());
        args.push(write_settings_override(job_dir, model, effort)?.display().to_string());
    }
    args.extend(tail);
    let mut env = dsh.env.clone();
    env.push(("DSHGO_JOB".into(), "1".into()));
    Ok(SpawnSpec { cmd: dsh.cmd, args, extra_env: env, profile: profile.into() })
}
