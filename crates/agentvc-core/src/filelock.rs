//! Exclusive advisory locks on a file (#204), for the stack, agent and
//! comments locks.
//!
//! The lock is the OS lock on an open file (`flock` on Unix, `LockFileEx` on
//! Windows, through [`std::fs::File::try_lock`]), not the file's existence.
//! The kernel releases it when the holder's process exits, even on SIGKILL,
//! so a lock is never taken over by age: a holder that runs for an hour keeps
//! it, and a lock file left by a dead process is free at once.
//!
//! The file itself stays. Deleting it would let two processes hold "the"
//! lock at once (one on the unlinked file, one on a new one), so nobody ever
//! removes it. While held, it says who holds it ([`Holder`]); the holder
//! empties it before letting go.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::{IoContext, Result};

/// Who holds a lock, as written into the lock file.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Holder {
    pub pid: u32,
    /// The holder's command line, shortened.
    pub command: String,
    /// When it took the lock (Unix seconds).
    pub since: i64,
}

impl Holder {
    fn current() -> Holder {
        let mut command = std::env::args().collect::<Vec<_>>().join(" ");
        if command.len() > 200 {
            let mut end = 200;
            while !command.is_char_boundary(end) {
                end -= 1;
            }
            command.truncate(end);
            command.push('…');
        }
        Holder {
            pid: std::process::id(),
            command,
            since: crate::space::now(),
        }
    }

    /// `pid 123 (agentvc stack sync), for 45s`.
    pub fn describe(&self) -> String {
        let age = crate::space::now().saturating_sub(self.since).max(0);
        format!("pid {} ({}), for {age}s", self.pid, self.command)
    }
}

/// An exclusive lock on a file; released on drop (or when the process dies).
#[derive(Debug)]
pub struct FileLock {
    file: File,
    path: PathBuf,
}

impl FileLock {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // Still holding the lock: forget the holder, then let go. Never
        // remove the file (see the module docs).
        let _ = self.file.set_len(0);
        let _ = self.file.unlock();
    }
}

/// The holder written in the lock file at `path`, if it can be read. On
/// Windows a held lock cannot be read by others, so this is `None` there.
pub fn holder(path: &Path) -> Option<Holder> {
    let mut text = String::new();
    File::open(path).ok()?.read_to_string(&mut text).ok()?;
    serde_json::from_str(text.trim()).ok()
}

/// Take the lock on `path` (created if missing), trying every `poll` for up
/// to `wait`. `Ok(Err(holder))` when someone else still holds it then.
pub fn acquire(
    path: &Path,
    wait: Duration,
    poll: Duration,
) -> Result<std::result::Result<FileLock, Option<Holder>>> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).at(dir)?;
    }
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .at(path)?;
    let start = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => {
                let text = serde_json::to_string(&Holder::current()).expect("serializes") + "\n";
                // Best effort: the holder is only for messages.
                let _ = file
                    .set_len(0)
                    .and_then(|()| file.seek(SeekFrom::Start(0)))
                    .and_then(|_| file.write_all(text.as_bytes()));
                return Ok(Ok(FileLock {
                    file,
                    path: path.to_path_buf(),
                }));
            }
            Err(fs::TryLockError::WouldBlock) => {
                if start.elapsed() >= wait {
                    return Ok(Err(holder(path)));
                }
                std::thread::sleep(poll);
            }
            Err(fs::TryLockError::Error(e)) => return Err(e).at(path),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLL: Duration = Duration::from_millis(5);

    #[test]
    fn held_lock_is_not_taken_however_old() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        let held = acquire(&path, Duration::ZERO, POLL).unwrap().unwrap();
        // Make the file look hours old: age must not matter.
        let old = std::time::SystemTime::now() - Duration::from_secs(4 * 3600);
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let again = acquire(&path, Duration::from_millis(50), POLL).unwrap();
        let who = again.expect_err("a held lock is never taken over");
        // Windows does not let others read a locked range.
        #[cfg(unix)]
        assert_eq!(who.unwrap().pid, std::process::id());
        #[cfg(not(unix))]
        let _ = who;
        assert!(path.exists());
        drop(held);
        // Released: free again, and the file is still there.
        assert!(path.exists());
        acquire(&path, Duration::ZERO, POLL).unwrap().unwrap();
    }

    #[test]
    fn file_left_by_a_dead_process_is_free_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        fs::write(
            &path,
            r#"{"pid":999999,"command":"agentvc stack sync","since":1}"#,
        )
        .unwrap();
        let start = Instant::now();
        let l = acquire(&path, Duration::from_secs(10), POLL)
            .unwrap()
            .unwrap();
        assert!(start.elapsed() < Duration::from_secs(1));
        drop(l);
        assert_eq!(fs::read_to_string(&path).unwrap(), "", "holder cleared");
    }

    #[test]
    fn waiter_gets_the_lock_when_the_holder_lets_go() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        let held = acquire(&path, Duration::ZERO, POLL).unwrap().unwrap();
        let p = path.clone();
        let t =
            std::thread::spawn(move || acquire(&p, Duration::from_secs(10), POLL).unwrap().is_ok());
        std::thread::sleep(Duration::from_millis(100));
        drop(held);
        assert!(t.join().unwrap());
    }

    /// A child process holding the lock is SIGKILLed: the lock is free at
    /// once, without any timeout.
    #[cfg(unix)]
    #[test]
    fn lock_of_a_killed_process_is_released() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        // Opened before the fork so the child only makes async-signal-safe
        // calls; the parent closes its copy at once.
        let f = File::options()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        // SAFETY: the child only calls flock and pause.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            use std::os::fd::AsRawFd;
            unsafe {
                libc::flock(f.as_raw_fd(), libc::LOCK_EX);
                loop {
                    libc::pause();
                }
            }
        }
        drop(f);
        // Wait until the child holds it.
        let start = Instant::now();
        loop {
            match acquire(&path, Duration::ZERO, POLL).unwrap() {
                Err(_) => break,
                Ok(l) => {
                    drop(l);
                    assert!(start.elapsed() < Duration::from_secs(10));
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            libc::waitpid(pid, std::ptr::null_mut(), 0);
        }
        let start = Instant::now();
        acquire(&path, Duration::from_secs(5), POLL)
            .unwrap()
            .expect("the dead holder's lock is released");
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
