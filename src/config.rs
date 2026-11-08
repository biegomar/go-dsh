//! Persistent settings in `~/.dshgo/config.json`. Environment variables win over
//! the file, so CI and one-off calls can still override everything.

use std::path::PathBuf;

use serde_json::{Map, Value};

use crate::paths::home;
use crate::util::{expand_home, read_json, write_json_atomic, DshgoError, Result};

pub struct ConfigKey {
    pub key: &'static str,
    pub env: &'static str,
    pub help: &'static str,
}

pub const CONFIG_KEYS: &[ConfigKey] = &[
    ConfigKey { key: "dshRepo", env: "DSHGO_DSH_REPO", help: "path to a dsh source checkout (wins over dshBin)" },
    ConfigKey { key: "dshBin", env: "DSHGO_DSH_BIN", help: "path to the dsh executable" },
    ConfigKey { key: "webUrl", env: "DSHGO_WEB_URL", help: "origin of the DSH web UI, e.g. http://127.0.0.1:3080" },
    ConfigKey { key: "backend", env: "DSHGO_BACKEND", help: "auto | web | headless" },
    ConfigKey { key: "workspaces", env: "DSHGO_WORKSPACES", help: "root folder for new workspaces (default ~/dsh-workspaces)" },
    ConfigKey { key: "python", env: "DSHGO_PYTHON", help: "python with playwright for `go-dsh capture`" },
];

fn file() -> PathBuf {
    home().join("config.json")
}

pub fn read_config() -> Map<String, Value> {
    match read_json::<Value>(&file()) {
        Ok(Some(Value::Object(m))) => m,
        _ => Map::new(),
    }
}

pub fn key_def(key: &str) -> Option<&'static ConfigKey> {
    CONFIG_KEYS.iter().find(|k| k.key == key)
}

/// Effective value: environment first, then config.json.
pub fn cfg(key: &str) -> Option<String> {
    let def = key_def(key)?;
    let from_env = std::env::var(def.env).ok().filter(|v| !v.is_empty());
    let value = from_env.or_else(|| read_config().get(key).and_then(|v| v.as_str()).map(String::from))?;
    if value.is_empty() {
        None
    } else {
        Some(expand_home(&value))
    }
}

pub fn set_config(key: &str, value: Option<&str>) -> Result<()> {
    if key_def(key).is_none() {
        let keys: Vec<&str> = CONFIG_KEYS.iter().map(|k| k.key).collect();
        return Err(DshgoError::new(format!("Unknown config key \"{key}\". Keys: {}", keys.join(", ")), 2));
    }
    let mut next = read_config();
    match value {
        Some(v) => {
            next.insert(key.to_string(), Value::String(v.to_string()));
        }
        None => {
            next.remove(key);
        }
    }
    write_json_atomic(&file(), &Value::Object(next))
}
