//! `agentvcd start`, `stop` and `status` (#221): the daemon in the
//! background, one per checkout.
//!
//! `start` runs `agentvcd` again, detached, with its output in a log file.
//! Once the child listens it writes a pidfile saying which process serves
//! which port; `stop` and `status` go by that file. Both live in a folder of
//! their own per checkout under `$AGENTVC_HOME/daemons/` (default
//! `~/.agentvc`), so they exist before the repo has a `.agentvc` and do not
//! show up in `git status`. The folder is keyed by the folder the daemon
//! serves, so two clones of one repo can each run one.
//!
//! A pidfile whose process is gone, or whose process no longer listens on
//! its port (the pid was reused), is stale: it is removed and the daemon
//! counts as not running.

use std::fs;
use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// The pidfile's name in a checkout's daemon folder.
pub const PIDFILE: &str = "agentvcd.pid";
/// The log's name in a checkout's daemon folder.
pub const LOGFILE: &str = "agentvcd.log";

/// What the pidfile says about a running daemon.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PidInfo {
    pub pid: u32,
    pub port: u16,
    pub url: String,
    /// The folder it serves.
    pub workdir: PathBuf,
    /// Unix seconds.
    pub started_at: i64,
}

/// The folder a daemon started in `dir` serves: the enclosing agentvc space,
/// else the git repository's top (where `agentvcd` makes the space), else
/// `dir` itself. Canonical, so every way of naming it finds one daemon.
pub fn served_root(dir: &Path) -> PathBuf {
    let dir = canon(dir);
    if let Some(top) = dir
        .ancestors()
        .find(|d| d.join(".agentvc").join("meta.db").is_file())
    {
        return top.to_path_buf();
    }
    let top = std::process::Command::new("git")
        .arg("-C")
        .arg(&dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()));
    match top {
        Some(t) if !t.as_os_str().is_empty() => canon(&t),
        _ => dir,
    }
}

fn canon(p: &Path) -> PathBuf {
    let p = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    // Windows' canonical form has a `\\?\` prefix git does not print.
    #[cfg(windows)]
    {
        let s = p.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(rest.to_string());
        }
    }
    p
}

/// The folder holding the pidfile and log of the daemon serving `root`:
/// `$AGENTVC_HOME/daemons/<folder name>-<hash of its path>`, or under the
/// system temp folder when there is no home.
pub fn state_dir(root: &Path) -> PathBuf {
    let base = agentvc_core::checkouts::home()
        .unwrap_or_else(|| std::env::temp_dir().join("agentvc"))
        .join("daemons");
    let name: String = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    let id = agentvc_core::checkouts::space_id(&root.to_string_lossy());
    let id = id.trim_start_matches('s');
    base.join(if name.is_empty() {
        id.to_string()
    } else {
        format!("{name}-{id}")
    })
}

/// Where a checkout's daemon keeps its pidfile and log.
#[derive(Clone, Debug)]
pub struct Paths {
    pub root: PathBuf,
    pub dir: PathBuf,
    pub pidfile: PathBuf,
    pub log: PathBuf,
}

impl Paths {
    /// The paths for the daemon that would serve `dir`.
    pub fn for_dir(dir: &Path) -> Paths {
        let root = served_root(dir);
        let state = state_dir(&root);
        Paths {
            pidfile: state.join(PIDFILE),
            log: state.join(LOGFILE),
            dir: state,
            root,
        }
    }
}

/// The pidfile at `path`, if it is there and readable.
pub fn read_pidfile(path: &Path) -> Option<PidInfo> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(text.trim()).ok()
}

/// Write the pidfile atomically (a temp file renamed into place).
pub fn write_pidfile(path: &Path, info: &PidInfo) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(serde_json::to_string(info).expect("serializes").as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

/// Remove the pidfile if it still names `pid` (a newer daemon may own it).
pub fn remove_pidfile_if(path: &Path, pid: u32) {
    if read_pidfile(path).is_some_and(|i| i.pid == pid) {
        let _ = fs::remove_file(path);
    }
}

/// Whether something listens on 127.0.0.1:`port`.
pub fn port_answers(port: u16) -> bool {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
}

/// The daemon serving `paths`, from its pidfile; a stale pidfile is removed
/// and gives `None`.
pub fn running(paths: &Paths) -> Option<PidInfo> {
    let info = read_pidfile(&paths.pidfile)?;
    if alive(info.pid) && port_answers(info.port) {
        return Some(info);
    }
    // Gone, or the pid now belongs to something that does not hold our port.
    let _ = fs::remove_file(&paths.pidfile);
    None
}

/// Whether process `pid` exists (and, on Linux, is not a zombie).
#[cfg(unix)]
pub fn alive(pid: u32) -> bool {
    let Ok(p) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // Signal 0 checks for existence without sending anything.
    let rc = unsafe { libc::kill(p, 0) };
    if rc != 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EPERM) {
        return false;
    }
    // A zombie still has a pid but is done.
    if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
        if let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r.trim_start()) {
            return !rest.starts_with('Z') && !rest.starts_with('X');
        }
    }
    true
}

/// Whether process `pid` exists.
#[cfg(windows)]
pub fn alive(pid: u32) -> bool {
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\"")))
        .unwrap_or(false)
}

#[cfg(not(any(unix, windows)))]
pub fn alive(_pid: u32) -> bool {
    false
}

/// Stop process `pid`: SIGTERM, then SIGKILL if it is still there after
/// `grace` (on Windows, `taskkill /F`). `Ok(true)` when it had to be killed.
pub fn terminate(pid: u32, grace: Duration) -> io::Result<bool> {
    #[cfg(unix)]
    {
        let p = libc::pid_t::try_from(pid)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad pid"))?;
        if unsafe { libc::kill(p, libc::SIGTERM) } != 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ESRCH) {
                return Ok(false);
            }
            return Err(e);
        }
        if wait_gone(pid, grace) {
            return Ok(false);
        }
        unsafe { libc::kill(p, libc::SIGKILL) };
        wait_gone(pid, Duration::from_secs(5));
        Ok(true)
    }
    #[cfg(windows)]
    {
        // A detached process has no console to send Ctrl+C to.
        let _ = grace;
        let out = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output()?;
        if !out.status.success() && alive(pid) {
            return Err(io::Error::other(
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            ));
        }
        wait_gone(pid, Duration::from_secs(5));
        Ok(true)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (pid, grace);
        Err(io::Error::other("stopping a process is not supported here"))
    }
}

/// Wait up to `timeout` for `pid` to exit; whether it did.
pub fn wait_gone(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !alive(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The last `{"ok": false, "error": {...}}` line the daemon printed into its
/// log since byte `from`: its code and message.
pub fn error_in_log(log: &Path, from: u64) -> Option<(String, String)> {
    let bytes = fs::read(log).ok()?;
    let start = usize::try_from(from).unwrap_or(0).min(bytes.len());
    let text = String::from_utf8_lossy(&bytes[start..]);
    text.lines().rev().find_map(|l| {
        let v: serde_json::Value = serde_json::from_str(l.trim()).ok()?;
        let e = v.get("error")?;
        Some((
            e.get("code")?.as_str()?.to_string(),
            e.get("message")?.as_str()?.to_string(),
        ))
    })
}

/// The log's text since byte `from`, the last `max` bytes of it.
pub fn log_tail(log: &Path, from: u64, max: usize) -> String {
    let Ok(bytes) = fs::read(log) else {
        return String::new();
    };
    let start = usize::try_from(from).unwrap_or(0).min(bytes.len());
    let start = start.max(bytes.len().saturating_sub(max));
    String::from_utf8_lossy(&bytes[start..]).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_dir_is_named_after_the_folder_and_stable() {
        let a = state_dir(Path::new("/src/my repo"));
        let b = state_dir(Path::new("/src/my repo"));
        let c = state_dir(Path::new("/other/my repo"));
        assert_eq!(a, b);
        assert_ne!(a, c);
        let name = a.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("my_repo-"), "{name}");
    }

    #[test]
    fn pidfile_round_trips_and_is_only_removed_by_its_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("d").join(PIDFILE);
        let info = PidInfo {
            pid: 42,
            port: 1,
            url: "http://localhost:1".into(),
            workdir: tmp.path().into(),
            started_at: 7,
        };
        write_pidfile(&path, &info).unwrap();
        assert_eq!(read_pidfile(&path), Some(info));
        remove_pidfile_if(&path, 43);
        assert!(path.exists());
        remove_pidfile_if(&path, 42);
        assert!(!path.exists());
    }

    #[test]
    fn this_process_is_alive() {
        assert!(alive(std::process::id()));
    }

    #[test]
    fn finds_the_last_error_in_the_log_after_an_offset() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join(LOGFILE);
        let old = "{\"ok\":false,\"error\":{\"code\":\"old\",\"message\":\"x\"}}\n";
        let new = "noise\n{\"ok\":false,\"error\":{\"code\":\"bind\",\"message\":\"in use\"}}\n";
        fs::write(&log, format!("{old}{new}")).unwrap();
        assert_eq!(
            error_in_log(&log, old.len() as u64),
            Some(("bind".into(), "in use".into()))
        );
        assert_eq!(error_in_log(&log, (old.len() + new.len()) as u64), None);
    }
}
