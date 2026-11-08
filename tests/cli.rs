//! End-to-end tests: the real `go-dsh` binary against a fake dsh. Never touches the real setup.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;
use tempfile::TempDir;

struct Env {
    home: TempDir,
    ws: TempDir,
}

fn fake_dsh() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().parent().unwrap();
    let name = if cfg!(windows) { "fake-dsh.exe" } else { "fake-dsh" };
    let p = dir.join("examples").join(name);
    assert!(p.exists(), "fake-dsh example not built: {}", p.display());
    p
}

fn env() -> Env {
    Env { home: tempfile::tempdir().unwrap(), ws: tempfile::tempdir().unwrap() }
}

struct Out {
    code: i32,
    out: String,
    err: String,
    json: Option<Value>,
}

fn run(e: &Env, args: &[&str]) -> Out {
    run_in(e, args, "")
}

fn run_in(e: &Env, args: &[&str], input: &str) -> Out {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_go-dsh"))
        .args(args)
        .env("DSHGO_HOME", e.home.path())
        .env("DSHGO_DSH_CMD", fake_dsh())
        .env("DSHGO_WORKSPACES", e.ws.path().join("auto"))
        .env("DSHGO_BACKEND", "headless")
        .env("DSHGO_WEB_URL", "http://127.0.0.1:9")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    let res = child.wait_with_output().unwrap();
    let out = String::from_utf8_lossy(&res.stdout).to_string();
    let json = if args.contains(&"--json") { serde_json::from_str(&out).ok() } else { None };
    Out { code: res.status.code().unwrap_or(-1), out, err: String::from_utf8_lossy(&res.stderr).to_string(), json }
}

fn p(base: &Path, name: &str) -> String {
    base.join(name).display().to_string()
}

#[test]
fn run_wait_json_returns_the_final_answer() {
    let e = env();
    let cwd = p(e.ws.path(), "one");
    let r = run(&e, &["run", "-C", &cwd, "--wait", "--json", "Say hello"]);
    assert_eq!(r.code, 0, "{}", r.err);
    let j = r.json.expect("json");
    assert_eq!(j["ok"], true);
    assert_eq!(j["result"], "FAKE-RESULT: Say hello");
    assert_eq!(j["job"]["status"], "done");
    assert!(Path::new(&cwd).join("AGENTS.md").exists(), "new -C dir becomes a workspace");
}

#[test]
fn run_without_cwd_creates_an_auto_workspace() {
    let e = env();
    let r = run(&e, &["run", "-w", "--json", "Auto folder"]);
    assert_eq!(r.code, 0, "{}", r.err);
    let j = r.json.unwrap();
    let cwd = j["job"]["cwd"].as_str().unwrap();
    assert!(Path::new(cwd).starts_with(e.ws.path().join("auto")) || cwd.contains("auto"));
    assert!(Path::new(cwd).join(".dshgo-workspace").exists());
}

#[test]
fn async_run_wait_result_files_and_continue() {
    let e = env();
    let cwd = p(e.ws.path(), "two");
    let start = run(&e, &["run", "-C", &cwd, "--json", "Write a file WRITE note.txt"]);
    assert_eq!(start.code, 0, "{}", start.err);
    let id = start.json.unwrap()["job"]["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("j-"));
    let wait = run(&e, &["wait", &id, "--timeout", "30s", "--json"]);
    assert_eq!(wait.code, 0, "{}", wait.out);
    assert_eq!(wait.json.unwrap()["jobs"][0]["status"], "done");
    let result = run(&e, &["result", &id]);
    assert_eq!(result.out.trim(), "FAKE-RESULT: Write a file WRITE note.txt");
    let files = run(&e, &["files", &id]);
    assert!(files.out.contains("note.txt"), "{}", files.out);
    let logs = run(&e, &["logs", &id]);
    assert!(logs.out.contains("→ bash: echo hallo"), "{}", logs.out);
    let cont = run(&e, &["continue", &id, "More please", "-w", "--max-wait", "30s", "--json"]);
    assert_eq!(cont.code, 0, "{} {}", cont.out, cont.err);
    let j = cont.json.unwrap();
    assert!(j["result"].as_str().unwrap().contains("(resumed session-fake-"), "{}", j["result"]);
    assert_eq!(j["job"]["parent"], id.as_str());
}

#[test]
fn failing_job_exits_one_with_error() {
    let e = env();
    let cwd = p(e.ws.path(), "fail");
    let r = run(&e, &["run", "-C", &cwd, "-w", "--json", "Please FAIL now"]);
    assert_eq!(r.code, 1);
    let j = r.json.unwrap();
    assert_eq!(j["job"]["status"], "failed");
    assert!(j["job"]["error"].as_str().unwrap().contains("Fake failure"));
}

#[test]
fn wait_timeout_exits_four_and_cancel_stops_the_job() {
    let e = env();
    let cwd = p(e.ws.path(), "slow");
    let start = run(&e, &["run", "-C", &cwd, "--json", "SLEEP 30"]);
    let id = start.json.unwrap()["job"]["id"].as_str().unwrap().to_string();
    let wait = run(&e, &["wait", &id, "--timeout", "2s"]);
    assert_eq!(wait.code, 4, "{}", wait.out);
    let cancel = run(&e, &["cancel", &id, "--json"]);
    assert_eq!(cancel.code, 0, "{}", cancel.err);
    assert_eq!(cancel.json.unwrap()["jobs"][0]["status"], "cancelled");
}

#[test]
fn research_batch_collect_writes_index() {
    let e = env();
    let root = p(e.ws.path(), "batch");
    let r = run(&e, &["research", "Question one", "Question two", "-C", &root, "--json"]);
    assert_eq!(r.code, 0, "{}", r.err);
    let batch = r.json.unwrap()["batch"]["id"].as_str().unwrap().to_string();
    let wait = run(&e, &["wait", &batch, "--timeout", "60s", "--json"]);
    assert_eq!(wait.code, 0, "{}", wait.out);
    let col = run(&e, &["collect", &batch, "--json"]);
    assert_eq!(col.code, 0, "{}", col.err);
    let index = std::fs::read_to_string(Path::new(&root).join("INDEX.md")).unwrap();
    assert!(index.contains("Question one"), "{index}");
    assert!(index.contains("FAKE-RESULT"), "{index}");
    let status = run(&e, &["status", "last-batch", "--json"]);
    assert_eq!(status.json.unwrap()["summary"]["status"], "done");
}

#[test]
fn batch_file_with_blocks_and_stdin() {
    let e = env();
    let root = p(e.ws.path(), "blocks");
    let r = run_in(&e, &["batch", "-C", &root, "-p", "2", "--json"], "first task\nsecond line\n---\nsecond task\n");
    assert_eq!(r.code, 0, "{}", r.err);
    let j = r.json.unwrap();
    assert_eq!(j["jobs"].as_array().unwrap().len(), 2);
}

#[test]
fn usage_errors_exit_two_and_missing_refs_three() {
    let e = env();
    assert_eq!(run(&e, &["run"]).code, 2);
    assert_eq!(run(&e, &["run", "--effort", "bogus", "x"]).code, 2);
    let r = run(&e, &["status", "nope-nothing", "--json"]);
    assert_eq!(r.code, 3);
    assert_eq!(r.json.unwrap()["ok"], false);
}

#[test]
fn config_set_and_show_and_guide() {
    let e = env();
    let set = run(&e, &["config", "set", "workspaces", "/x/y", "--json"]);
    assert_eq!(set.code, 0, "{}", set.err);
    assert_eq!(set.json.unwrap()["stored"]["workspaces"], "/x/y");
    assert!(run(&e, &["guide"]).out.contains("drive DeepSeek Harness"));
    assert_eq!(run(&e, &["config", "set", "nope", "1"]).code, 2);
}

#[test]
fn help_and_version() {
    let e = env();
    let h = run(&e, &["--help"]);
    assert_eq!(h.code, 0);
    assert!(h.out.contains("research"));
    assert!(run(&e, &["--version"]).out.contains("go-dsh"));
}

#[test]
fn continue_a_batch_creates_a_follow_up_batch() {
    let e = env();
    let root = p(e.ws.path(), "cont");
    let r = run(&e, &["research", "Question one", "Question two", "-C", &root, "--json"]);
    assert_eq!(r.code, 0, "{}", r.err);
    let old = r.json.unwrap()["batch"]["id"].as_str().unwrap().to_string();
    assert_eq!(run(&e, &["wait", &old, "--timeout", "60s"]).code, 0);

    let c = run(&e, &["continue", &old, "Add a table", "-w", "--max-wait", "60s", "--json"]);
    assert_eq!(c.code, 0, "{} {}", c.out, c.err);
    let j = c.json.unwrap();
    let new_id = j["batch"]["id"].as_str().unwrap();
    assert_ne!(new_id, old);
    let jobs = j["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 2);
    for job in jobs {
        assert_eq!(job["status"], "done");
        assert!(job["result"].as_str().unwrap().contains("(resumed session-fake-"), "{job}");
        assert!(job["parent"].as_str().unwrap().starts_with("j-"));
    }
    // The follow-up batch collects like any other.
    assert_eq!(run(&e, &["collect", new_id]).code, 0);
}

#[test]
fn continue_only_failed_skips_finished_jobs() {
    let e = env();
    let root = p(e.ws.path(), "mixed");
    let r = run_in(&e, &["batch", "-C", &root, "--json"], "good task\n---\nbad task FAIL\n");
    let old = r.json.unwrap()["batch"]["id"].as_str().unwrap().to_string();
    assert_eq!(run(&e, &["wait", &old, "--timeout", "60s"]).code, 1);

    let c = run(&e, &["continue", &old, "Try again", "--only-failed", "-w", "--max-wait", "60s", "--json"]);
    assert_eq!(c.code, 0, "{} {}", c.out, c.err);
    assert_eq!(c.json.unwrap()["jobs"].as_array().unwrap().len(), 1);

    // Everything in the original batch is now either done or already continued; with all done nothing is left.
    let all_done = run(&e, &["research", "Only question", "-C", &p(e.ws.path(), "done"), "--json"]);
    let done_batch = all_done.json.unwrap()["batch"]["id"].as_str().unwrap().to_string();
    assert_eq!(run(&e, &["wait", &done_batch, "--timeout", "60s"]).code, 0);
    let none = run(&e, &["continue", &done_batch, "x", "--only-failed", "--json"]);
    assert_eq!(none.code, 1);
    assert_eq!(none.json.unwrap()["ok"], false);
}
