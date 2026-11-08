//! Filesystem locations. Every getter reads the environment lazily so tests
//! and callers can redirect state with `DSHGO_HOME`, `DSH_HOME` and `DSHGO_DSH_REPO`.

use std::env;
use std::path::PathBuf;

use crate::util::home_dir;

/// Name of the dsh profile that go-dsh manages and boots.
pub const PROFILE: &str = "concert";

fn user_home() -> PathBuf {
    home_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from)
}

pub fn home() -> PathBuf {
    env_path("DSHGO_HOME").unwrap_or_else(|| user_home().join(".dshgo"))
}
pub fn jobs_dir() -> PathBuf {
    home().join("jobs")
}
pub fn job_dir(id: &str) -> PathBuf {
    jobs_dir().join(id)
}
pub fn batches_dir() -> PathBuf {
    home().join("batches")
}
pub fn batch_dir(id: &str) -> PathBuf {
    batches_dir().join(id)
}
pub fn logs_dir() -> PathBuf {
    home().join("logs")
}

pub fn dsh_home() -> PathBuf {
    env_path("DSH_HOME").unwrap_or_else(|| user_home().join(".dsh"))
}
pub fn profile_dir(name: &str) -> PathBuf {
    dsh_home().join("profiles").join(name)
}
pub fn curated_skills_dir() -> PathBuf {
    dsh_home().join("skills-concert")
}
pub fn agents_skills_dir() -> PathBuf {
    env_path("DSH_AGENTS_HOME").unwrap_or_else(|| user_home().join(".agents")).join("skills")
}

/// Default root for workspaces created without an explicit `-C`.
pub fn default_workspace_root() -> PathBuf {
    env_path("DSHGO_WORKSPACES").unwrap_or_else(|| user_home().join("dsh-workspaces"))
}
