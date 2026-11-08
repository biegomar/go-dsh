//! `go-dsh skill install`: puts the bundled go-dsh-delegation skill where agent
//! harnesses look for skills. The canonical copy goes to `~/.agents/skills` (the
//! open Agent Skills location, also read by OpenCode); Claude Code, Codex and Pi
//! get a link (or, where links are not permitted, a copy) when their config folder exists.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::paths::agents_skills_dir;
use crate::util::{home_dir, link_or_copy, remove_any, DshgoError, Result};

pub const SKILL_NAME: &str = "go-dsh-delegation";
const SKILL_MD: &str = include_str!("../assets/skills/go-dsh-delegation/SKILL.md");
const MARKER: &str = ".installed-by-go-dsh";
const VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct AgentTarget {
    pub agent: &'static str,
    pub parent: PathBuf,
    pub dir: PathBuf,
}

pub fn agent_targets() -> Vec<AgentTarget> {
    let h = home_dir().unwrap_or_else(|| PathBuf::from("."));
    let env_dir = |name: &str, default: PathBuf| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or(default);
    let claude = env_dir("CLAUDE_CONFIG_DIR", h.join(".claude"));
    let codex = env_dir("CODEX_HOME", h.join(".codex"));
    let pi = h.join(".pi").join("agent");
    vec![
        AgentTarget { agent: "claude", dir: claude.join("skills"), parent: claude },
        AgentTarget { agent: "codex", dir: codex.join("skills"), parent: codex },
        AgentTarget { agent: "pi", dir: pi.join("skills"), parent: pi },
    ]
}

#[derive(PartialEq)]
enum EntryKind {
    Link,
    Ours,
    Foreign,
    Missing,
}

fn entry_kind(path: &Path) -> EntryKind {
    match fs::symlink_metadata(path) {
        Err(_) => EntryKind::Missing,
        Ok(m) if m.file_type().is_symlink() => EntryKind::Link,
        Ok(_) => {
            if path.join(MARKER).exists() {
                EntryKind::Ours
            } else {
                EntryKind::Foreign
            }
        }
    }
}

#[derive(Debug, Serialize)]
pub struct SkillRow {
    pub agent: String,
    pub path: String,
    pub action: String,
}

#[derive(Debug, Serialize)]
pub struct SkillInstall {
    pub canonical: String,
    pub rows: Vec<SkillRow>,
}

/// Install or refresh the skill.
pub fn install_skill(agents: Option<&[String]>, copy: bool) -> Result<SkillInstall> {
    let canonical = agents_skills_dir().join(SKILL_NAME);
    match entry_kind(&canonical) {
        EntryKind::Foreign => {
            return Err(DshgoError::new(format!("{} exists and was not installed by go-dsh", canonical.display()), 1)
                .hint("move it away, then run go-dsh skill install again"))
        }
        EntryKind::Link | EntryKind::Ours => remove_any(&canonical),
        EntryKind::Missing => {}
    }
    fs::create_dir_all(&canonical)?;
    fs::write(canonical.join("SKILL.md"), SKILL_MD)?;
    fs::write(canonical.join(MARKER), format!("go-dsh {VERSION}\n"))?;

    let mut rows = vec![SkillRow { agent: "agents (OpenCode, shared)".into(), path: canonical.display().to_string(), action: "installed".into() }];
    for t in agent_targets() {
        if let Some(wanted) = agents {
            if !wanted.iter().any(|a| a == t.agent) {
                continue;
            }
        } else if !t.parent.exists() {
            rows.push(SkillRow { agent: t.agent.into(), path: t.dir.display().to_string(), action: "skipped (not installed)".into() });
            continue;
        }
        let target = t.dir.join(SKILL_NAME);
        match entry_kind(&target) {
            EntryKind::Foreign => {
                rows.push(SkillRow {
                    agent: t.agent.into(),
                    path: target.display().to_string(),
                    action: "skipped (a different skill with this name exists)".into(),
                });
                continue;
            }
            EntryKind::Link | EntryKind::Ours => remove_any(&target),
            EntryKind::Missing => {}
        }
        fs::create_dir_all(&t.dir)?;
        let action = link_or_copy(&canonical, &target, copy)?;
        if action == "copied" {
            // A copy is "ours" so the next install may replace it.
            fs::write(target.join(MARKER), format!("go-dsh {VERSION}\n"))?;
        }
        rows.push(SkillRow { agent: t.agent.into(), path: target.display().to_string(), action: action.into() });
    }
    Ok(SkillInstall { canonical: canonical.display().to_string(), rows })
}
