//! Small shared helpers: errors, ids, durations, slugs, JSON files, processes.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use serde::de::DeserializeOwned;
use serde::Serialize;

/// Error carrying a process exit code (2 usage, 3 not found, 4 timeout, 5 environment).
#[derive(Debug, Clone)]
pub struct DshgoError {
    pub message: String,
    pub code: i32,
    pub hint: Option<String>,
}

impl DshgoError {
    pub fn new(message: impl Into<String>, code: i32) -> Self {
        Self { message: message.into(), code, hint: None }
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl fmt::Display for DshgoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DshgoError {}

impl From<io::Error> for DshgoError {
    fn from(e: io::Error) -> Self {
        DshgoError::new(e.to_string(), 1)
    }
}

impl From<serde_json::Error> for DshgoError {
    fn from(e: serde_json::Error) -> Self {
        DshgoError::new(e.to_string(), 1)
    }
}

impl From<serde_yaml::Error> for DshgoError {
    fn from(e: serde_yaml::Error) -> Self {
        DshgoError::new(e.to_string(), 1)
    }
}

pub type Result<T> = std::result::Result<T, DshgoError>;

const B36: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";

fn to_base36(mut n: u128) -> String {
    if n == 0 {
        return "0".into();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(B36[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// Time-ordered short id, e.g. `j-mgf3k2ab7x9`.
pub fn new_id(prefix: &str) -> String {
    let millis = Utc::now().timestamp_millis().max(0) as u128;
    let rand: String = (0..4).map(|_| B36[(rand::random::<u8>() % 36) as usize] as char).collect();
    format!("{prefix}-{}{rand}", to_base36(millis))
}

/// Parse `90s`, `20m`, `1.5h` or bare seconds into seconds.
pub fn parse_duration(text: &str) -> Result<u64> {
    let bad = || DshgoError::new(format!("Invalid duration \"{text}\". Examples: 90s, 20m, 1h"), 2);
    let t = text.trim();
    let (num, factor) = match t.chars().last() {
        Some(c) if c.eq_ignore_ascii_case(&'s') => (&t[..t.len() - 1], 1.0),
        Some(c) if c.eq_ignore_ascii_case(&'m') => (&t[..t.len() - 1], 60.0),
        Some(c) if c.eq_ignore_ascii_case(&'h') => (&t[..t.len() - 1], 3600.0),
        Some(_) => (t, 1.0),
        None => return Err(bad()),
    };
    let num = num.trim();
    if num.is_empty() || !num.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return Err(bad());
    }
    let v: f64 = num.parse().map_err(|_| bad())?;
    Ok((v * factor).round() as u64)
}

/// Filesystem-friendly slug that keeps letters of every script, umlauts included.
pub fn slug(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut pending_dash = false;
    for c in text.chars().flat_map(|c| c.to_lowercase()) {
        if c.is_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(c);
        } else {
            pending_dash = true;
        }
    }
    let cut: String = out.chars().take(max).collect();
    let cut = cut.trim_end_matches('-').to_string();
    if cut.is_empty() {
        "task".into()
    } else {
        cut
    }
}

pub fn write_json_atomic<T: Serialize>(file: &Path, value: &T) -> Result<()> {
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir)?;
    }
    let tag: u32 = rand::random();
    let mut tmp_name = file.as_os_str().to_os_string();
    tmp_name.push(format!(".{}.{tag:08x}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp_name);
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    fs::write(&tmp, text)?;
    let mut last = None;
    for _ in 0..8 {
        match fs::rename(&tmp, file) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = Some(e);
                sleep(Duration::from_millis(25));
            }
        }
    }
    let _ = fs::remove_file(&tmp);
    Err(last.map(DshgoError::from).unwrap_or_else(|| DshgoError::new("rename failed", 1)))
}

/// Read JSON; a missing file yields `None`. Retries briefly for concurrent writers.
pub fn read_json<T: DeserializeOwned>(file: &Path) -> Result<Option<T>> {
    for attempt in 0..3 {
        match fs::read_to_string(file) {
            Ok(text) => match serde_json::from_str::<T>(&text) {
                Ok(v) => return Ok(Some(v)),
                Err(e) if attempt == 2 => return Err(e.into()),
                Err(_) => {}
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) if attempt == 2 => return Err(e.into()),
            Err(_) => {}
        }
        sleep(Duration::from_millis(15));
    }
    Ok(None)
}

pub fn fmt_duration(ms: Option<i64>) -> String {
    let Some(ms) = ms else { return "-".into() };
    let total = ((ms as f64) / 1000.0).round().max(0.0) as i64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

pub fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

pub fn parse_iso_ms(iso: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(iso).ok().map(|d| d.timestamp_millis())
}

/// Single-line excerpt for compact listings.
pub fn excerpt(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        let mut s: String = flat.chars().take(max.saturating_sub(1)).collect();
        s.push('…');
        s
    } else {
        flat
    }
}

pub fn sleep_ms(ms: u64) {
    sleep(Duration::from_millis(ms));
}

/// Expand a leading `~` (also `~/` and `~\`) to the home directory.
pub fn expand_home(p: &str) -> String {
    if p == "~" || p.starts_with("~/") || p.starts_with("~\\") {
        if let Some(h) = home_dir() {
            return format!("{}{}", h.display(), &p[1..]);
        }
    }
    p.to_string()
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::home_dir()
}

// ---------------------------------------------------------------- processes --

#[cfg(unix)]
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: signal 0 only checks for existence.
    let r = unsafe { libc::kill(pid as i32, 0) };
    r == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
pub fn pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    if pid == 0 {
        return false;
    }
    // SAFETY: plain Win32 calls; the handle is closed before returning.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h as usize == 0 {
            return false;
        }
        let mut code: u32 = 0;
        let ok = GetExitCodeProcess(h, &mut code);
        CloseHandle(h);
        ok != 0 && code == 259 // STILL_ACTIVE
    }
}

/// Ask a process (and its group / tree) to stop.
#[cfg(unix)]
pub fn terminate(pid: u32) {
    kill_unix(pid, libc::SIGTERM);
}

/// Stop a process (and its group / tree) for good.
#[cfg(unix)]
pub fn force_kill(pid: u32) {
    kill_unix(pid, libc::SIGKILL);
}

#[cfg(unix)]
fn kill_unix(pid: u32, sig: i32) {
    if pid == 0 {
        return;
    }
    // SAFETY: plain kill(2); first the process group (we spawn children as group leaders), then the pid.
    unsafe {
        if libc::kill(-(pid as i32), sig) != 0 {
            libc::kill(pid as i32, sig);
        }
    }
}

#[cfg(windows)]
pub fn terminate(pid: u32) {
    taskkill(pid);
}

#[cfg(windows)]
pub fn force_kill(pid: u32) {
    taskkill(pid);
}

#[cfg(windows)]
fn taskkill(pid: u32) {
    use std::os::windows::process::CommandExt;
    if pid == 0 {
        return;
    }
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .status();
}

/// Platform flags so a spawned process runs in its own group without a console window.
pub fn detach_command(cmd: &mut std::process::Command, no_window: bool) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let _ = no_window;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        let flags = CREATE_NEW_PROCESS_GROUP | if no_window { CREATE_NO_WINDOW } else { DETACHED_PROCESS };
        cmd.creation_flags(flags);
        make_std_handles_private();
    }
}

/// Recursively copy a directory.
pub fn copy_dir(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &to)?;
        } else {
            fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

/// Link `target` to `source` (symlink); falls back to a copy where links are not permitted (Windows without developer mode).
pub fn link_or_copy(source: &Path, target: &Path, force_copy: bool) -> io::Result<&'static str> {
    if !force_copy {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(source, target)?;
            return Ok("linked");
        }
        #[cfg(windows)]
        {
            let r = if source.is_dir() { std::os::windows::fs::symlink_dir(source, target) } else { std::os::windows::fs::symlink_file(source, target) };
            if r.is_ok() {
                return Ok("linked");
            }
        }
    }
    if source.is_dir() {
        copy_dir(source, target)?;
    } else {
        fs::copy(source, target)?;
    }
    Ok("copied")
}

/// Remove a file, symlink or directory tree if present.
pub fn remove_any(path: &Path) {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => {
            let _ = fs::remove_dir_all(path);
        }
        Ok(_) => {
            if fs::remove_file(path).is_err() {
                let _ = fs::remove_dir(path);
            }
        }
        Err(_) => {}
    }
}

/// Write a file readable only by the current user where the platform supports it.
pub fn write_private(path: &Path, content: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Windows lets a spawned child inherit every inheritable handle of its parent, so a detached
/// runner would keep the caller's stdout/stderr pipes open and a capturing agent harness would
/// wait until the whole job is over. Mark our own standard handles non-inheritable first; the
/// child still gets the log files we hand it explicitly.
#[cfg(windows)]
fn make_std_handles_private() {
    use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT};
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: plain Win32 calls on our own standard handles; failures are ignored.
        unsafe {
            let h = GetStdHandle(which);
            if h as usize != 0 && h as isize != -1 {
                SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90s").unwrap(), 90);
        assert_eq!(parse_duration("20m").unwrap(), 1200);
        assert_eq!(parse_duration("1.5h").unwrap(), 5400);
        assert_eq!(parse_duration("45").unwrap(), 45);
        assert_eq!(parse_duration("abc").unwrap_err().code, 2);
        assert_eq!(parse_duration("").unwrap_err().code, 2);
    }

    #[test]
    fn slugs_keep_umlauts() {
        assert_eq!(slug("Größe der Äpfel & Birnen!", 40), "größe-der-äpfel-birnen");
        assert_eq!(slug("!!!", 40), "task");
        assert_eq!(slug("abc def ghi", 5), "abc-d");
        assert_eq!(slug("abc def", 4), "abc");
    }

    #[test]
    fn excerpt_and_duration_format() {
        assert_eq!(excerpt("a  b\nc", 10), "a b c");
        assert_eq!(excerpt("abcdefghij", 5), "abcd…");
        assert_eq!(fmt_duration(Some(3_725_000)), "1h 02m");
        assert_eq!(fmt_duration(Some(65_000)), "1m 05s");
        assert_eq!(fmt_duration(Some(4_000)), "4s");
        assert_eq!(fmt_duration(None), "-");
    }

    #[test]
    fn ids_are_time_ordered_and_prefixed() {
        let a = new_id("j");
        assert!(a.starts_with("j-"));
        assert!(a.len() >= 12);
    }
}
