//! `go-dsh setup` and `go-dsh doctor`. The dedicated dsh profile "concert" is
//! the shipped headless profile plus research MCP servers copied from another
//! profile (default: web) and only the skills you name, which keeps the worker
//! prompt smaller, faster and cheaper than the full skill pool.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_yaml::{Mapping, Value as Yaml};

use crate::config::cfg;
use crate::dsh::{active_profile, dsh_command, read_dsh_settings, resolve_dsh, which};
use crate::paths::{agents_skills_dir, curated_skills_dir, dsh_home, home, profile_dir, PROFILE};
use crate::util::{link_or_copy, remove_any, write_private, DshgoError, Result};

const MCP_CLIENT: &str = "@deepseek-ai/dsh-mcp-client";

const RESEARCH_MCP_WORDS: &[&str] = &["firecrawl", "brave", "context7", "exa", "tavily", "perplexity", "jina", "serper", "searx", "kagi"];

/// Word-boundary-ish match of research MCP names ("exa" must not match "example").
fn is_research_name(text: &str) -> bool {
    let lower = text.to_lowercase();
    RESEARCH_MCP_WORDS.iter().any(|w| {
        let mut start = 0;
        while let Some(pos) = lower[start..].find(w) {
            let at = start + pos;
            let end = at + w.len();
            // only "exa" carries the trailing word boundary in the original pattern
            let boundary_ok = *w != "exa" || lower[end..].chars().next().map(|c| !(c.is_alphanumeric() || c == '_')).unwrap_or(true);
            if boundary_ok {
                return true;
            }
            start = end;
        }
        false
    })
}

fn str_at<'a>(row: &'a Yaml, path: &[&str]) -> Option<&'a str> {
    let mut cur = row;
    for key in path {
        cur = cur.get(*key)?;
    }
    cur.as_str()
}

/// Row filter for `--mcp research|all|none|id,id`.
pub fn mcp_matches(mode: &str, row: &Yaml) -> bool {
    match mode {
        "none" => false,
        "all" => true,
        "research" => {
            let id = str_at(row, &["id"]).unwrap_or("");
            let server = str_at(row, &["config", "serverName"]).unwrap_or("");
            is_research_name(&format!("{id} {server}"))
        }
        list => {
            let wanted: Vec<&str> = list.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
            let id = str_at(row, &["id"]).unwrap_or("");
            let server = str_at(row, &["config", "serverName"]).unwrap_or("");
            wanted.contains(&id) || wanted.contains(&server)
        }
    }
}

const TAG_PLACEHOLDER: &str = "!dshtag-";

/// dsh patch files use `!!js <expression>` scalars. serde_yaml silently drops secondary-handle
/// (`!!`) tags, which would turn the expressions into plain strings. Rewrite them to a local tag
/// before parsing so they survive, and back again before writing (see `restore_tags`).
fn protect_tags(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + 16);
    let mut i = 0;
    while i < chars.len() {
        let at_boundary = i == 0 || chars[i - 1].is_whitespace();
        if at_boundary && chars[i] == '!' && chars.get(i + 1) == Some(&'!') && chars.get(i + 2).map(|c| c.is_alphanumeric()).unwrap_or(false) {
            out.push_str(TAG_PLACEHOLDER);
            i += 2;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

fn restore_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find(TAG_PLACEHOLDER) {
        let boundary = pos == 0 || rest[..pos].chars().last().map(|c| c.is_whitespace()).unwrap_or(true);
        out.push_str(&rest[..pos]);
        out.push_str(if boundary { "!!" } else { TAG_PLACEHOLDER });
        rest = &rest[pos + TAG_PLACEHOLDER.len()..];
    }
    out.push_str(rest);
    out
}

/// MCP client insert rows of a dsh patch file that pass the filter. Values are never printed.
pub fn extract_rows(patch_text: &str, mode: &str) -> Result<Vec<Yaml>> {
    let doc: Yaml = serde_yaml::from_str(&protect_tags(patch_text))?;
    let mut rows = vec![];
    if let Yaml::Sequence(entries) = doc {
        for entry in entries {
            if let Some(Yaml::Sequence(insert)) = entry.get("insert") {
                for row in insert {
                    if str_at(row, &["name"]) == Some(MCP_CLIENT) && mcp_matches(mode, row) {
                        rows.push(row.clone());
                    }
                }
            }
        }
    }
    Ok(rows)
}

fn yaml_map(pairs: Vec<(&str, Yaml)>) -> Yaml {
    let mut m = Mapping::new();
    for (k, v) in pairs {
        m.insert(Yaml::String(k.into()), v);
    }
    Yaml::Mapping(m)
}

pub fn render_patch(rows: Vec<Yaml>, skills_dir: Option<&str>, source_profile: &str) -> Result<String> {
    let header = format!(
        "# Managed by go-dsh (go-dsh setup). The next `go-dsh setup` overwrites this file.\n# MCP rows are copied from the \"{source_profile}\" profile.\n#\n# Only the skills named in `go-dsh setup --skills` are linked, which keeps the worker prompt small;\n# the .dshgo-workspace marker lets workers in batch subfolders load the workspace AGENTS.md.\n"
    );
    let mut base: Vec<Yaml> = vec![];
    if let Some(dir) = skills_dir {
        base.push(yaml_map(vec![
            ("id", "skill-filesystem".into()),
            (
                "config",
                yaml_map(vec![("includeDefaultRoots", Yaml::Bool(false)), ("customSkillDirs", Yaml::Sequence(vec![dir.into()])), ("watch", Yaml::Bool(false))]),
            ),
        ]));
    }
    base.push(yaml_map(vec![
        ("id", "agent-instructions".into()),
        (
            "config",
            yaml_map(vec![("maxBytes", Yaml::Number(65536.into())), ("projectRootMarkers", Yaml::Sequence(vec![".git".into(), ".dshgo-workspace".into()]))]),
        ),
    ]));
    if !rows.is_empty() {
        base.push(yaml_map(vec![("insert", Yaml::Sequence(rows))]));
    }
    Ok(format!("{header}\n{}", restore_tags(&serde_yaml::to_string(&Yaml::Sequence(base))?)))
}

/// Link the named skills from the shared skills folder into the managed skill folder of the profile.
/// Returns the folder, the linked names and the names that do not exist in the shared folder.
fn link_skills(names: &[String]) -> Result<(PathBuf, Vec<String>, Vec<String>)> {
    let dir = curated_skills_dir();
    fs::create_dir_all(&dir)?;
    // Links from an earlier setup that are no longer wanted must not keep skills alive.
    if let Ok(rd) = fs::read_dir(&dir) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let is_link = entry.file_type().map(|t| t.is_symlink()).unwrap_or(false);
            if is_link && !names.contains(&name) {
                remove_any(&entry.path());
            }
        }
    }
    let (mut linked, mut missing) = (vec![], vec![]);
    for name in names {
        let source = agents_skills_dir().join(name);
        let target = dir.join(name);
        if !source.exists() {
            missing.push(name.clone());
            continue;
        }
        if fs::symlink_metadata(&target).is_err() {
            link_or_copy(&source, &target, false)?;
        }
        linked.push(name.clone());
    }
    Ok((dir, linked, missing))
}

fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(3)..].join(" ")
}

struct RunOutput {
    ok: bool,
    stdout: String,
    stderr: String,
}

/// Run a command to completion with a timeout; a missing binary counts as failure.
fn run_capture(cmd: &PathBuf, args: &[String], env: &[(String, String)], cwd: &PathBuf, timeout: Duration) -> RunOutput {
    let mut c = Command::new(cmd);
    c.args(args).envs(env.iter().cloned()).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if cwd.is_dir() {
        c.current_dir(cwd);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000);
    }
    let Ok(mut child) = c.spawn() else { return RunOutput { ok: false, stdout: String::new(), stderr: format!("cannot start {}", cmd.display()) } };
    let mut out = child.stdout.take();
    let mut err = child.stderr.take();
    let t_out = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(o) = out.as_mut() {
            let _ = std::io::Read::read_to_string(o, &mut s);
        }
        s
    });
    let t_err = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(o) = err.as_mut() {
            let _ = std::io::Read::read_to_string(o, &mut s);
        }
        s
    });
    let start = Instant::now();
    let ok = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st.success(),
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => break false,
        }
    };
    RunOutput { ok, stdout: t_out.join().unwrap_or_default(), stderr: t_err.join().unwrap_or_default() }
}

#[derive(Debug, Serialize)]
pub struct ProfileSetup {
    pub profile: String,
    #[serde(rename = "profileDir")]
    pub profile_dir: String,
    #[serde(rename = "mcpServers")]
    pub mcp_servers: Vec<String>,
    #[serde(rename = "skillsDir")]
    pub skills_dir: Option<String>,
    pub skills: Vec<String>,
    pub notes: Vec<String>,
}

/// Create or refresh the "concert" dsh profile.
pub fn setup_profile(mcp: &str, from: &str, skills: Option<&str>) -> Result<ProfileSetup> {
    let dir = profile_dir(PROFILE);
    let mut notes = vec![];
    fs::create_dir_all(home())?;
    let timeout = Duration::from_secs(180);
    if !dir.join("package.json").exists() {
        // Let dsh scaffold the profile from its shipped headless template.
        let c = dsh_command(&["--profile", PROFILE, "--from-default-profile", "headless", "--dump-config"])?;
        let res = run_capture(&c.cmd, &c.args, &c.env, &home(), timeout);
        if !res.ok || !dir.join("package.json").exists() {
            return Err(DshgoError::new(format!("dsh could not create the profile: {}", tail(&res.stderr)), 5));
        }
    }
    let mut skills_dir = None;
    let mut linked = vec![];
    if skills != Some("all") {
        // Default: no skills at all, which keeps the worker prompt as small as possible.
        let names: Vec<String> = skills.map(|l| l.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
        let (d, l, missing) = link_skills(&names)?;
        skills_dir = Some(d.display().to_string());
        linked = l;
        if names.is_empty() {
            notes.push("no skills selected: workers run without skills (choose some with --skills a,b or keep the dsh defaults with --skills all)".into());
        } else if !missing.is_empty() {
            notes.push(format!("not found in {}: {}", agents_skills_dir().display(), missing.join(", ")));
        }
    }

    let source_patch = profile_dir(from).join("cordis.patch.yml");
    let mut rows = vec![];
    if source_patch.exists() {
        rows = extract_rows(&fs::read_to_string(&source_patch)?, mcp)?;
        if rows.is_empty() && mcp != "none" {
            notes.push(format!("no matching MCP servers in the \"{from}\" profile; workers use the built-in web_search and web_fetch"));
        }
    } else if mcp != "none" {
        notes.push(format!("profile \"{from}\" has no cordis.patch.yml; no MCP servers copied"));
    }
    let mcp_servers: Vec<String> =
        rows.iter().map(|r| str_at(r, &["config", "serverName"]).or_else(|| str_at(r, &["id"])).unwrap_or("?").to_string()).collect();
    // Copied MCP rows can carry API keys, so the file stays private.
    write_private(&dir.join("cordis.patch.yml"), &render_patch(rows, skills_dir.as_deref(), from)?)?;

    // Validate the composed tree without booting it and without printing it.
    let c = dsh_command(&["--profile", PROFILE, "--dump-config"])?;
    let check = run_capture(&c.cmd, &c.args, &c.env, &home(), timeout);
    if !check.ok {
        return Err(DshgoError::new(format!("profile written, but dsh rejects it: {}", tail(&check.stderr)), 5));
    }
    Ok(ProfileSetup { profile: PROFILE.into(), profile_dir: dir.display().to_string(), mcp_servers, skills_dir, skills: linked, notes })
}

#[derive(Clone)]
pub struct PythonInfo {
    pub python: String,
    pub playwright: Option<String>,
}

static PYTHON: OnceLock<PythonInfo> = OnceLock::new();

/// A python that can import playwright, for `go-dsh capture`.
pub fn find_python() -> PythonInfo {
    PYTHON
        .get_or_init(|| {
            let mut candidates: Vec<String> = cfg("python").into_iter().collect();
            if cfg!(windows) {
                candidates.extend(["python", "python3", "py"].map(String::from));
            } else {
                candidates.extend(
                    ["python3", "python3.14", "python3.13", "python3.12", "python3.11", "python", "/opt/homebrew/bin/python3", "/usr/local/bin/python3"]
                        .map(String::from),
                );
            }
            for python in candidates {
                let Some(path) = which(&python).or_else(|| PathBuf::from(&python).exists().then(|| PathBuf::from(&python))) else { continue };
                let args = vec!["-c".to_string(), "import importlib.metadata as m; print(m.version(\"playwright\"))".to_string()];
                let res = run_capture(&path, &args, &[], &home(), Duration::from_secs(15));
                if res.ok {
                    return PythonInfo { python, playwright: Some(res.stdout.trim().to_string()) };
                }
            }
            PythonInfo { python: cfg("python").unwrap_or_else(|| if cfg!(windows) { "python".into() } else { "python3".into() }), playwright: None }
        })
        .clone()
}

#[derive(Debug, Serialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
    pub optional: bool,
}

pub fn doctor() -> Vec<Check> {
    let _ = fs::create_dir_all(home());
    let mut checks: Vec<Check> = vec![];
    let mut add = |name: &str, ok: bool, detail: String, optional: bool| checks.push(Check { name: name.into(), ok, detail, optional });

    let dsh = resolve_dsh();
    add(
        "dsh",
        dsh.is_some(),
        dsh.as_ref()
            .map(|d| format!("{}: {}", d.kind, d.where_))
            .unwrap_or_else(|| "not found: npm install -g @deepseek-ai/dsh (or go-dsh config set dshBin|dshRepo <path>)".into()),
        false,
    );

    let profile = active_profile();
    add(
        "profile",
        profile == PROFILE,
        if profile == PROFILE {
            profile_dir(PROFILE).display().to_string()
        } else {
            "profile concert not set up: jobs use the built-in dsh profile headless (run go-dsh setup)".into()
        },
        true,
    );

    let settings = match read_dsh_settings() {
        Ok(s) => s,
        Err(e) => {
            add("settings", false, e.message, false);
            serde_json::Value::Null
        }
    };
    let preset = settings["permission"]["defaultPreset"].as_str().unwrap_or("workspace-write (default)").to_string();
    let full = preset == "danger-full-access";
    add("permissions", full, format!("permission preset {preset}{}", if full { "" } else { ": unattended workers may stall on approval prompts" }), !full);
    let model = &settings["agent-default-model"];
    add(
        "model",
        model.is_object(),
        if model.is_object() {
            let eff = model["reasoningEffort"].as_str().map(|e| format!(" ({e})")).unwrap_or_default();
            format!("{}/{}{eff}", model["provider"].as_str().unwrap_or("?"), model["model"].as_str().unwrap_or("?"))
        } else {
            "no default model in ~/.dsh/settings.yaml (dsh default applies)".into()
        },
        true,
    );
    // A credentials file alone proves little: it may hold only a login token of the web UI. Headless
    // runs use the route "deepseek-official", which needs an API key (DEEPSEEK_API_KEY or one stored through
    // the web UI's Models page). Jobs in the web UI do not need this, so it is a hint, not a failure.
    let env_key = std::env::var_os("DEEPSEEK_API_KEY").is_some_and(|v| !v.is_empty());
    let file = dsh_home().join(".credentials.yaml");
    let stored_key = fs::read_to_string(&file).map(|t| t.contains("deepseek-official")).unwrap_or(false);
    let has_key = env_key || stored_key;
    add(
        "api-key",
        has_key,
        if env_key {
            "DEEPSEEK_API_KEY is set in the environment".into()
        } else if stored_key {
            "API key stored in the dsh credentials".into()
        } else if file.exists() {
            "logged in, but no API key for headless jobs: export DEEPSEEK_API_KEY or store it on the Models page of the DSH web UI (web UI jobs do not need it)"
                .into()
        } else {
            "no dsh credentials: log in through dsh or set DEEPSEEK_API_KEY".into()
        },
        true,
    );

    if let Some(d) = dsh.as_ref().filter(|d| d.kind != "custom") {
        if let Ok(c) = dsh_command(&["--profile", profile, "--dump-config"]) {
            let _ = d;
            let t0 = Instant::now();
            let res = run_capture(&c.cmd, &c.args, &c.env, &home(), Duration::from_secs(120));
            let ms = t0.elapsed().as_millis();
            add("dsh-config", res.ok, if res.ok { format!("profile {profile} composes in {ms} ms") } else { tail(&res.stderr) }, false);
        }
    }

    let py = find_python();
    add(
        "playwright",
        py.playwright.is_some(),
        match &py.playwright {
            Some(v) => format!("Python Playwright {v} ({})", py.python),
            None => "not importable: pip install playwright && python -m playwright install chromium (needed for go-dsh capture)".into(),
        },
        true,
    );
    for bin in ["ffmpeg", "ffprobe", "yt-dlp"] {
        let path = which(bin);
        add(bin, path.is_some(), path.map(|p| p.display().to_string()).unwrap_or_else(|| "not found".into()), true);
    }
    let writable = fs::create_dir_all(home()).is_ok() && {
        let probe = home().join(".write-test");
        let ok = fs::write(&probe, b"").is_ok();
        let _ = fs::remove_file(&probe);
        ok
    };
    add("state", writable, if writable { home().display().to_string() } else { format!("{} is not writable", home().display()) }, false);
    checks
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATCH: &str = r#"- id: base
- insert:
    - name: "@deepseek-ai/dsh-mcp-client"
      id: mcp-firecrawl
      config:
        serverName: firecrawl
        env:
          API_KEY: !!js process.env.FIRECRAWL_API_KEY
    - name: "@deepseek-ai/dsh-mcp-client"
      id: mcp-example
      config:
        serverName: example
    - name: other
      id: firecrawl-but-not-mcp
"#;

    #[test]
    fn research_filter_matches_names_not_substrings_of_exa() {
        let rows = extract_rows(PATCH, "research").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(str_at(&rows[0], &["id"]), Some("mcp-firecrawl"));
        assert_eq!(extract_rows(PATCH, "all").unwrap().len(), 2);
        assert_eq!(extract_rows(PATCH, "none").unwrap().len(), 0);
        assert_eq!(extract_rows(PATCH, "mcp-example").unwrap().len(), 1);
    }

    #[test]
    fn skills_are_linked_only_when_named_and_present() {
        let dsh = tempfile::tempdir().unwrap();
        let agents = tempfile::tempdir().unwrap();
        std::env::set_var("DSH_HOME", dsh.path());
        std::env::set_var("DSH_AGENTS_HOME", agents.path());
        let skill = agents.path().join("skills").join("alpha");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "x").unwrap();

        let (dir, linked, missing) = link_skills(&["alpha".to_string(), "ghost".to_string()]).unwrap();
        assert_eq!(linked, vec!["alpha"]);
        assert_eq!(missing, vec!["ghost"]);
        assert!(dir.join("alpha").join("SKILL.md").exists());

        // An empty selection links nothing.
        let (_, linked, missing) = link_skills(&[]).unwrap();
        assert!(linked.is_empty() && missing.is_empty());
    }

    #[test]
    fn js_expression_tags_survive_the_round_trip() {
        let rows = extract_rows(PATCH, "research").unwrap();
        let out = render_patch(rows, Some("/skills"), "web").unwrap();
        println!("{out}");
        assert!(out.contains("process.env.FIRECRAWL_API_KEY"), "{out}");
        assert!(out.contains("API_KEY: !!js process.env.FIRECRAWL_API_KEY"), "{out}");
        assert!(!out.contains("dshtag"), "{out}");
        assert!(out.contains(".dshgo-workspace"));
        // The rendered patch must parse again.
        let _: Yaml = serde_yaml::from_str(&out).unwrap();
    }
}
