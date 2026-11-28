//! `agentvcd start`, `status` and `stop` (#221), through the binary.

use std::fs;
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_agentvcd");

/// A git repo with one commit, and a private `$AGENTVC_HOME`.
fn fixture() -> (tempfile::TempDir, tempfile::TempDir) {
    let repo = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let ok = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .current_dir(repo.path())
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q"]);
    fs::write(repo.path().join("a.txt"), "a\n").unwrap();
    git(&["add", "a.txt"]);
    git(&["commit", "-qm", "init"]);
    (repo, home)
}

/// `agentvcd ARGS --json` in `dir`: exit success and its JSON.
fn run(dir: &Path, home: &Path, args: &[&str]) -> (bool, Value) {
    let out = Command::new(BIN)
        .args(args)
        .arg("--json")
        .current_dir(dir)
        .env("AGENTVC_HOME", home)
        .env_remove("AGENTVC_CLOUD_URL")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let v = serde_json::from_str(text.trim())
        .unwrap_or_else(|e| panic!("{args:?}: {e}: {text:?} / {:?}", out.stderr));
    (out.status.success(), v)
}

/// Stops the daemon whatever the test does.
struct Stop<'a>(&'a Path, &'a Path);
impl Drop for Stop<'_> {
    fn drop(&mut self) {
        let _ = Command::new(BIN)
            .args(["stop", "--timeout-secs", "2"])
            .current_dir(self.0)
            .env("AGENTVC_HOME", self.1)
            .output();
    }
}

fn code(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or("")
}

#[test]
fn start_status_stop() {
    let (repo, home) = fixture();
    let (r, h) = (repo.path(), home.path());
    let sub = r.join("sub");
    fs::create_dir_all(&sub).unwrap();

    let (ok, v) = run(r, h, &["status"]);
    assert!(!ok);
    assert_eq!(code(&v), "not_running");
    assert_eq!(v["error"]["running"], false);
    let (ok, v) = run(r, h, &["stop"]);
    assert!(!ok);
    assert_eq!(code(&v), "not_running");

    let (ok, started) = run(r, h, &["start", "--port", "0", "--pr-poll-secs", "0"]);
    let _guard = Stop(r, h);
    assert!(ok, "{started}");
    assert_eq!(started["running"], true);
    let pid = started["pid"].as_u64().unwrap();
    let port = started["port"].as_u64().unwrap();
    assert_ne!(port, 0);
    assert_eq!(started["url"], format!("http://localhost:{port}"));
    let log = started["log"].as_str().unwrap();
    let pidfile = started["pidfile"].as_str().unwrap();
    // Outside the repo, under $AGENTVC_HOME.
    assert!(Path::new(log).starts_with(h), "{log}");
    assert!(Path::new(pidfile).is_file());
    assert!(fs::read_to_string(log).unwrap().contains("\"ok\":true"));
    // It made the space.
    assert!(r.join(".agentvc").is_dir());

    // From a subfolder: the same daemon.
    let (ok, st) = run(&sub, h, &["status"]);
    assert!(ok, "{st}");
    assert_eq!(st["pid"].as_u64(), Some(pid));
    assert_eq!(st["url"], started["url"]);
    assert_eq!(st["log"], started["log"]);

    let (ok, again) = run(r, h, &["start", "--port", "0"]);
    assert!(!ok);
    assert_eq!(code(&again), "already_running");
    assert_eq!(again["error"]["pid"].as_u64(), Some(pid));

    let (ok, stopped) = run(&sub, h, &["stop"]);
    assert!(ok, "{stopped}");
    assert_eq!(stopped["stopped"], true);
    assert_eq!(stopped["forced"], false);
    assert!(!Path::new(pidfile).exists());
    assert!(!agentvcd::daemon::alive(pid as u32));

    let (ok, v) = run(r, h, &["status"]);
    assert!(!ok);
    assert_eq!(code(&v), "not_running");
}

#[test]
fn start_reports_a_port_in_use() {
    let (repo, home) = fixture();
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port().to_string();
    let (ok, v) = run(repo.path(), home.path(), &["start", "--port", &port]);
    let _guard = Stop(repo.path(), home.path());
    assert!(!ok, "{v}");
    assert_eq!(code(&v), "port_in_use");
    assert!(v["error"]["message"].as_str().unwrap().contains(&port));
}

#[test]
fn start_reports_why_the_daemon_failed() {
    let (repo, home) = fixture();
    // No space and --no-init: the daemon exits at once.
    let (ok, v) = run(
        repo.path(),
        home.path(),
        &["start", "--port", "0", "--no-init"],
    );
    let _guard = Stop(repo.path(), home.path());
    assert!(!ok, "{v}");
    assert_eq!(code(&v), "start_failed");
    assert_eq!(v["error"]["cause"], "no_space");
    assert!(v["error"]["log"].as_str().is_some());
}

#[test]
fn a_stale_pidfile_counts_as_not_running() {
    let (repo, home) = fixture();
    let paths = agentvcd::daemon::Paths::for_dir(repo.path());
    // A process that is gone: a child we started and waited for.
    let mut gone = Command::new(BIN)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let pid = gone.id();
    gone.wait().unwrap();
    // `Paths` looks at the real $AGENTVC_HOME in this process; point it at
    // the one the binary will use instead.
    let state = home
        .path()
        .join("daemons")
        .join(paths.dir.file_name().unwrap());
    let pidfile = state.join(agentvcd::daemon::PIDFILE);
    agentvcd::daemon::write_pidfile(
        &pidfile,
        &agentvcd::daemon::PidInfo {
            pid,
            port: 1,
            url: "http://localhost:1".into(),
            workdir: paths.root.clone(),
            started_at: 0,
        },
    )
    .unwrap();
    let (ok, v) = run(repo.path(), home.path(), &["status"]);
    assert!(!ok);
    assert_eq!(code(&v), "not_running");
    assert_eq!(v["error"]["pidfile"], pidfile.to_string_lossy().as_ref());
    assert!(!pidfile.exists(), "the stale pidfile is removed");

    // And `start` goes ahead.
    let (ok, started) = run(repo.path(), home.path(), &["start", "--port", "0"]);
    let _guard = Stop(repo.path(), home.path());
    assert!(ok, "{started}");
    let (ok, _) = run(repo.path(), home.path(), &["stop"]);
    assert!(ok);
}

#[test]
fn plain_agentvcd_still_serves_in_the_foreground() {
    let (repo, home) = fixture();
    let mut child = Command::new(BIN)
        .args(["--port", "0", "--json", "--pr-poll-secs", "0"])
        .current_dir(repo.path())
        .env("AGENTVC_HOME", home.path())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufRead::read_line(
        &mut std::io::BufReader::new(child.stdout.take().unwrap()),
        &mut line,
    )
    .unwrap();
    let v: Value = serde_json::from_str(line.trim()).unwrap();
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(v["ok"], true, "{line}");
    assert!(v["url"].as_str().unwrap().starts_with("http://localhost:"));
    // Foreground daemons keep no pidfile.
    assert!(!home.path().join("daemons").exists());
}
