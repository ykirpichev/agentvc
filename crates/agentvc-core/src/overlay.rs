//! overlayfs views for filesystems without reflinks (ext4, Docker's overlay).
//!
//! A view is an overlay mount: its private writable layer (`upper`) sits on a
//! chain of frozen, read-only *layers* stored under `.agentvc/layers/`.
//!
//! A parent view keeps changing after a fork, and overlayfs needs its lower
//! layers to stay put, so a fork first *freezes* the parent into a layer:
//!
//! - The first fork from a parent copies it whole into a base layer (paid
//!   once, like the copy backend).
//! - Later forks walk the parent with `lstat` only, compare against the
//!   manifest saved with the parent's last layer, and write a thin delta
//!   layer holding just the changed files plus whiteouts for deleted ones.
//!   If nothing changed, the last layer is reused as is.
//! - A parent that is itself an overlay view starts from its own lower
//!   chain, so forking a fork costs only what the parent changed.
//!
//! Mounting, in order of preference:
//!
//! - `kernel`: `mount(2)` with `CAP_SYS_ADMIN` (root, or a privileged
//!   container). The mount is visible to every process.
//! - `fuse`: `fuse-overlayfs`, unprivileged and visible to every process.
//! - `userns`: an unprivileged user and mount namespace. The mount exists only
//!   inside agentvc processes and what they start (`agentvc run`), so each
//!   agentvc process that needs the view enters a namespace and mounts it.
//!
//! Views are remounted on demand: anything that reads a view (snapshot, fork,
//! run, `agentvc path`) mounts it first if it is not mounted, for example
//! after a reboot.

use std::collections::BTreeMap;
use std::ffi::{CString, OsString};
use std::fs;
use std::io::{BufReader, BufWriter, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, OptionalExtension};
use serde::Serialize;

use crate::error::{Error, IoContext, Result};
use crate::space::{Attempt, Space, SPACE_DIR};
use crate::view::NOT_COPIED;

/// Longest lower chain before a fork compacts it into a fresh base layer.
/// Keeps the mount options well under the kernel's one-page limit.
pub const MAX_CHAIN: usize = 32;

/// How an overlay view is mounted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mounter {
    Kernel,
    Fuse,
    Userns,
}

impl Mounter {
    pub fn as_str(self) -> &'static str {
        match self {
            Mounter::Kernel => "kernel",
            Mounter::Fuse => "fuse",
            Mounter::Userns => "userns",
        }
    }

    pub fn parse(s: &str) -> Option<Mounter> {
        Some(match s {
            "kernel" => Mounter::Kernel,
            "fuse" => Mounter::Fuse,
            "userns" => Mounter::Userns,
            _ => return None,
        })
    }

    /// Whether processes outside agentvc see the mount.
    pub fn is_global(self) -> bool {
        !matches!(self, Mounter::Userns)
    }
}

/// The directories of one overlay mount.
#[derive(Clone, Debug)]
pub struct MountSpec {
    /// Lower layers, topmost first.
    pub lowers: Vec<PathBuf>,
    pub upper: PathBuf,
    pub work: PathBuf,
    pub target: PathBuf,
    /// Skip syncing the upper filesystem on unmount (scratch mounts only:
    /// an unmount otherwise flushes every dirty page on that filesystem).
    pub volatile: bool,
}

impl MountSpec {
    fn options(&self, mounter: Mounter) -> std::result::Result<String, String> {
        let all = self
            .lowers
            .iter()
            .chain([&self.upper, &self.work])
            .map(|p| p.to_str().ok_or("non-UTF-8 path"))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if let Some(p) = all.iter().find(|p| p.contains([',', ':', '\\'])) {
            return Err(format!(
                "path {p:?} contains ',', ':' or '\\', which overlayfs options cannot carry"
            ));
        }
        let mut o = format!(
            "lowerdir={},upperdir={},workdir={}",
            all[..self.lowers.len()].join(":"),
            self.upper.display(),
            self.work.display()
        );
        if mounter == Mounter::Userns {
            // trusted.* xattrs are off limits in a user namespace.
            o.push_str(",userxattr");
        }
        if self.volatile && mounter != Mounter::Fuse {
            o.push_str(",volatile");
        }
        Ok(o)
    }
}

// ---- mounting --------------------------------------------------------------

/// Whether `path` is a mount point (its device differs from its parent's).
pub fn is_mounted(path: &Path) -> bool {
    let (Ok(m), Some(parent)) = (fs::symlink_metadata(path), path.parent()) else {
        return false;
    };
    fs::symlink_metadata(parent).is_ok_and(|p| p.dev() != m.dev())
}

/// Mount an overlay. For `userns` the calling process enters a private user
/// and mount namespace first (once), so the mount is only visible there.
pub fn mount(mounter: Mounter, spec: &MountSpec) -> std::result::Result<(), String> {
    let opts = spec.options(mounter)?;
    match mounter {
        Mounter::Kernel => kernel_mount(&opts, &spec.target),
        Mounter::Userns => {
            enter_userns()?;
            kernel_mount(&opts, &spec.target)
        }
        Mounter::Fuse => {
            let out = Command::new("fuse-overlayfs")
                .arg("-o")
                .arg(&opts)
                .arg(&spec.target)
                .stdin(Stdio::null())
                .output()
                .map_err(|e| format!("fuse-overlayfs: {e}"))?;
            if out.status.success() {
                Ok(())
            } else {
                Err(format!(
                    "fuse-overlayfs failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ))
            }
        }
    }
}

fn kernel_mount(opts: &str, target: &Path) -> std::result::Result<(), String> {
    let src = CString::new("overlay").expect("no NUL");
    let target_c = cpath(target)?;
    let data = CString::new(opts).map_err(|_| "NUL in mount options".to_string())?;
    // SAFETY: all pointers are valid NUL-terminated strings for the call.
    let rc = unsafe {
        libc::mount(
            src.as_ptr(),
            target_c.as_ptr(),
            src.as_ptr(),
            0,
            data.as_ptr().cast(),
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(format!(
            "mount overlay on {}: {}",
            target.display(),
            std::io::Error::last_os_error()
        ))
    }
}

/// Unmount a view. A `userns` mount ends with the process, so this is a no-op
/// for it outside the namespace.
pub fn unmount(mounter: Mounter, target: &Path) -> std::result::Result<(), String> {
    if !is_mounted(target) {
        return Ok(());
    }
    match mounter {
        Mounter::Fuse => {
            for tool in ["fusermount3", "fusermount"] {
                if let Ok(s) = Command::new(tool)
                    .arg("-u")
                    .arg(target)
                    .stderr(Stdio::null())
                    .status()
                {
                    if s.success() {
                        return Ok(());
                    }
                }
            }
            Err(format!("could not unmount {}", target.display()))
        }
        Mounter::Kernel | Mounter::Userns => {
            let t = cpath(target)?;
            // SAFETY: t is a valid C string.
            if unsafe { libc::umount2(t.as_ptr(), 0) } == 0 {
                Ok(())
            } else {
                Err(format!(
                    "umount {}: {}",
                    target.display(),
                    std::io::Error::last_os_error()
                ))
            }
        }
    }
}

/// Unmount every mount at or below `dir` that this process can see, deepest
/// first: overlay views of a space that is being deleted, or test leftovers.
pub fn unmount_under(dir: &Path) {
    let Ok(info) = fs::read_to_string("/proc/self/mountinfo") else {
        return;
    };
    let mut mounts: Vec<(PathBuf, bool)> = info
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(' ').collect();
            let point = PathBuf::from(unescape_mountinfo(f.get(4)?));
            let sep = f.iter().position(|x| *x == "-")?;
            let fuse = f.get(sep + 1)?.starts_with("fuse");
            point.starts_with(dir).then_some((point, fuse))
        })
        .collect();
    mounts.sort_by_key(|(p, _)| std::cmp::Reverse(p.components().count()));
    for (point, fuse) in mounts {
        if fuse {
            let _ = unmount(Mounter::Fuse, &point);
        } else if let Ok(c) = cpath(&point) {
            // SAFETY: c is a valid C string.
            unsafe { libc::umount2(c.as_ptr(), libc::MNT_DETACH) };
        }
    }
}

/// Mount points in mountinfo escape space, tab, newline and backslash as
/// octal.
fn unescape_mountinfo(s: &str) -> OsString {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            out.push((b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    OsString::from_vec(out)
}

fn cpath(p: &Path) -> std::result::Result<CString, String> {
    CString::new(p.as_os_str().as_bytes()).map_err(|_| format!("NUL in path {}", p.display()))
}

static IN_USERNS: AtomicBool = AtomicBool::new(false);

/// Whether this process can enter a user namespace now: `unshare` refuses a
/// process with more than one thread.
pub fn can_enter_userns() -> bool {
    IN_USERNS.load(Ordering::Relaxed) || thread_count() == Some(1)
}

fn thread_count() -> Option<u32> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("Threads:"))
        .and_then(|n| n.trim().parse().ok())
}

/// Move this process into a new user and mount namespace in which it keeps
/// its own uid and gid but may mount. Idempotent.
fn enter_userns() -> std::result::Result<(), String> {
    if IN_USERNS.load(Ordering::Relaxed) {
        return Ok(());
    }
    if !can_enter_userns() {
        return Err("cannot enter a user namespace from a multi-threaded process".into());
    }
    // SAFETY: plain syscalls on the current process.
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) } != 0 {
        return Err(format!(
            "unshare(user, mount): {}",
            std::io::Error::last_os_error()
        ));
    }
    IN_USERNS.store(true, Ordering::Relaxed);
    let w = |file: &str, s: String| fs::write(file, s).map_err(|e| format!("writing {file}: {e}"));
    w("/proc/self/setgroups", "deny".into())?;
    w("/proc/self/uid_map", format!("{uid} {uid} 1"))?;
    w("/proc/self/gid_map", format!("{gid} {gid} 1"))?;
    // Keep our mounts from propagating anywhere.
    let root = CString::new("/").expect("no NUL");
    // SAFETY: valid C string, null source/type/data are allowed here.
    unsafe {
        libc::mount(
            std::ptr::null(),
            root.as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        )
    };
    Ok(())
}

/// Find a way to mount overlays from this process, trying a real mount of an
/// empty overlay under `scratch`. Returns the reasons each mounter failed.
pub fn probe(scratch: &Path) -> std::result::Result<Mounter, String> {
    let mut why = Vec::new();
    // Unique per call, not just per process: two threads of one process can
    // probe at once (prepare and fork racing), and a shared directory would
    // be removed under the other's mount.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = scratch.join(format!("probe-{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let spec = MountSpec {
        lowers: vec![dir.join("l")],
        upper: dir.join("u"),
        work: dir.join("w"),
        target: dir.join("m"),
        volatile: true,
    };
    for d in [&spec.lowers[0], &spec.upper, &spec.work, &spec.target] {
        fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let result = (|| {
        if IN_USERNS.load(Ordering::Relaxed) {
            // Already in our namespace: mounting there is the only option.
            return match mount(Mounter::Userns, &spec) {
                Ok(()) => Ok(Mounter::Userns),
                Err(e) => Err(format!("userns: {e}")),
            };
        }
        // `volatile` needs Linux 5.10; older kernels get a plain mount.
        let plain = MountSpec {
            volatile: false,
            ..spec.clone()
        };
        match mount(Mounter::Kernel, &spec).or_else(|_| mount(Mounter::Kernel, &plain)) {
            Ok(()) => return Ok(Mounter::Kernel),
            Err(e) => why.push(format!("kernel: {e}")),
        }
        if which("fuse-overlayfs") {
            match mount(Mounter::Fuse, &spec) {
                Ok(()) => return Ok(Mounter::Fuse),
                Err(e) => why.push(format!("fuse: {e}")),
            }
        } else {
            why.push("fuse: fuse-overlayfs not found in PATH".into());
        }
        if !can_enter_userns() {
            why.push("userns: this process is multi-threaded".into());
        } else {
            match probe_userns(&spec).or_else(|_| probe_userns(&plain)) {
                Ok(()) => return Ok(Mounter::Userns),
                Err(e) => why.push(format!("userns: {e}")),
            }
        }
        Err(why.join("; "))
    })();
    if let Ok(m) = result {
        let _ = unmount(m, &spec.target);
    }
    let _ = fs::remove_dir_all(&dir);
    result
}

fn which(prog: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(prog).is_file()))
}

/// Try the userns mount in a forked child, so a failure leaves this process
/// outside any namespace.
fn probe_userns(spec: &MountSpec) -> std::result::Result<(), String> {
    let opts = CString::new(spec.options(Mounter::Userns)?).map_err(|e| e.to_string())?;
    let target = cpath(&spec.target)?;
    // SAFETY: geteuid/getegid have no preconditions.
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    let uid_map = format!("{uid} {uid} 1");
    let gid_map = format!("{gid} {gid} 1");
    let files: [(&[u8], &[u8]); 3] = [
        (b"/proc/self/setgroups\0", b"deny"),
        (b"/proc/self/uid_map\0", uid_map.as_bytes()),
        (b"/proc/self/gid_map\0", gid_map.as_bytes()),
    ];
    let overlay = b"overlay\0";
    // SAFETY: the child only makes raw syscalls on memory prepared before
    // the fork, then exits with _exit.
    unsafe {
        let pid = libc::fork();
        if pid < 0 {
            return Err(format!("fork: {}", std::io::Error::last_os_error()));
        }
        if pid == 0 {
            if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) != 0 {
                libc::_exit(2);
            }
            for (path, data) in files {
                let fd = libc::open(path.as_ptr().cast(), libc::O_WRONLY);
                if fd < 0 || libc::write(fd, data.as_ptr().cast(), data.len()) < 0 {
                    libc::_exit(3);
                }
                libc::close(fd);
            }
            let rc = libc::mount(
                overlay.as_ptr().cast(),
                target.as_ptr(),
                overlay.as_ptr().cast(),
                0,
                opts.as_ptr().cast(),
            );
            libc::_exit(if rc == 0 { 0 } else { 4 });
        }
        let mut status = 0;
        libc::waitpid(pid, &mut status, 0);
        match libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status)) {
            Some(0) => Ok(()),
            Some(2) => Err("user namespaces are not allowed here".into()),
            Some(3) => Err("could not set up the namespace's uid map".into()),
            Some(4) => Err("the kernel refused an overlay mount in a user namespace".into()),
            _ => Err("probe process failed".into()),
        }
    }
}

// ---- manifests -------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Dir,
    File,
    Symlink,
}

/// What a fork records about one path, enough to see that it changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stat {
    kind: Kind,
    mode: u32,
    size: u64,
    mtime_ns: i128,
}

impl Stat {
    fn of(m: &fs::Metadata) -> Option<Stat> {
        let ft = m.file_type();
        let kind = if ft.is_dir() {
            Kind::Dir
        } else if ft.is_file() {
            Kind::File
        } else if ft.is_symlink() {
            Kind::Symlink
        } else {
            return None; // sockets, fifos, devices: not carried into views
        };
        Some(Stat {
            kind,
            mode: m.mode() & 0o7777,
            // A directory's size and mtime change with its entries, which are
            // compared one by one anyway.
            size: if kind == Kind::Dir { 0 } else { m.size() },
            mtime_ns: if kind == Kind::Dir {
                0
            } else {
                m.mtime() as i128 * 1_000_000_000 + m.mtime_nsec() as i128
            },
        })
    }
}

/// The state of a directory tree at the moment it was frozen into a layer.
#[derive(Clone, Debug, Default)]
pub struct Manifest {
    taken_ns: i128,
    entries: BTreeMap<PathBuf, Stat>,
}

const MANIFEST_MAGIC: &[u8; 8] = b"avcman1\n";

impl Manifest {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `lstat` every entry under `root` (skipping `.agentvc` and `.git` at the
    /// top), in parallel.
    pub fn scan(root: &Path) -> Result<Manifest> {
        let taken_ns = now_ns();
        let found = std::sync::Mutex::new(Vec::new());
        let failure = std::sync::Mutex::new(None);
        ignore::WalkBuilder::new(root)
            .standard_filters(false)
            .follow_links(false)
            .filter_entry(|e| !(e.depth() == 1 && NOT_COPIED.iter().any(|n| e.file_name() == *n)))
            .build_parallel()
            .run(|| {
                let found = &found;
                let failure = &failure;
                Box::new(move |entry| {
                    let entry = match entry {
                        Ok(e) => e,
                        Err(e) => {
                            failure.lock().expect("lock").get_or_insert(e.into());
                            return ignore::WalkState::Quit;
                        }
                    };
                    if entry.depth() == 0 {
                        return ignore::WalkState::Continue;
                    }
                    match fs::symlink_metadata(entry.path()) {
                        Ok(m) => {
                            if let Some(s) = Stat::of(&m) {
                                found
                                    .lock()
                                    .expect("lock")
                                    .push((entry.path().to_path_buf(), s));
                            }
                            ignore::WalkState::Continue
                        }
                        Err(e) => {
                            failure.lock().expect("lock").get_or_insert(Error::Io {
                                path: entry.path().to_path_buf(),
                                source: e,
                            });
                            ignore::WalkState::Quit
                        }
                    }
                })
            });
        if let Some(e) = failure.into_inner().expect("lock") {
            return Err(e);
        }
        let entries = found
            .into_inner()
            .expect("lock")
            .into_iter()
            .map(|(p, s)| (p.strip_prefix(root).expect("under root").to_path_buf(), s))
            .collect();
        Ok(Manifest { taken_ns, entries })
    }

    pub fn load(path: &Path) -> Result<Manifest> {
        let mut r = BufReader::new(fs::File::open(path).at(path)?);
        let bad = || Error::Io {
            path: path.to_path_buf(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, "corrupt manifest"),
        };
        let mut magic = [0u8; 8];
        r.read_exact(&mut magic).at(path)?;
        if &magic != MANIFEST_MAGIC {
            return Err(bad());
        }
        let mut m = Manifest {
            taken_ns: read_i128(&mut r).at(path)?,
            ..Default::default()
        };
        let mut kind = [0u8; 1];
        loop {
            match r.read_exact(&mut kind) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e).at(path),
            }
            let kind = match kind[0] {
                b'd' => Kind::Dir,
                b'f' => Kind::File,
                b'l' => Kind::Symlink,
                _ => return Err(bad()),
            };
            let mode = read_u64(&mut r).at(path)? as u32;
            let size = read_u64(&mut r).at(path)?;
            let mtime_ns = read_i128(&mut r).at(path)?;
            let len = read_u64(&mut r).at(path)? as usize;
            let mut name = vec![0u8; len];
            r.read_exact(&mut name).at(path)?;
            m.entries.insert(
                PathBuf::from(OsString::from_vec(name)),
                Stat {
                    kind,
                    mode,
                    size,
                    mtime_ns,
                },
            );
        }
        Ok(m)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let tmp = path.with_extension("tmp");
        let mut w = BufWriter::new(fs::File::create(&tmp).at(&tmp)?);
        let mut put = || -> std::io::Result<()> {
            w.write_all(MANIFEST_MAGIC)?;
            w.write_all(&self.taken_ns.to_le_bytes())?;
            for (p, s) in &self.entries {
                w.write_all(&[match s.kind {
                    Kind::Dir => b'd',
                    Kind::File => b'f',
                    Kind::Symlink => b'l',
                }])?;
                w.write_all(&(s.mode as u64).to_le_bytes())?;
                w.write_all(&s.size.to_le_bytes())?;
                w.write_all(&s.mtime_ns.to_le_bytes())?;
                let b = p.as_os_str().as_bytes();
                w.write_all(&(b.len() as u64).to_le_bytes())?;
                w.write_all(b)?;
            }
            w.flush()
        };
        put().at(&tmp)?;
        fs::rename(&tmp, path).at(path)
    }
}

fn read_u64(r: &mut impl Read) -> std::io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

fn read_i128(r: &mut impl Read) -> std::io::Result<i128> {
    let mut b = [0u8; 16];
    r.read_exact(&mut b)?;
    Ok(i128::from_le_bytes(b))
}

fn now_ns() -> i128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0)
}

// ---- layers ----------------------------------------------------------------

/// What freezing a parent into a layer cost.
#[derive(Clone, Debug, Default, Serialize)]
pub struct LayerStats {
    /// Entries the parent walk looked at.
    pub scanned: u64,
    pub files: u64,
    pub bytes: u64,
    pub dirs: u64,
    pub symlinks: u64,
    pub whiteouts: u64,
}

impl LayerStats {
    pub fn is_empty(&self) -> bool {
        self.files + self.dirs + self.symlinks + self.whiteouts == 0
    }
}

/// Write into `layer` everything in `src` (described by `now`) that differs
/// from `base`: the whole tree when there is no base, otherwise changed and
/// new entries plus whiteouts for deleted ones.
pub fn write_layer(
    src: &Path,
    now: &Manifest,
    base: Option<&Manifest>,
    layer: &Path,
) -> Result<LayerStats> {
    fs::create_dir_all(layer).at(layer)?;
    let mut st = LayerStats {
        scanned: now.entries.len() as u64,
        ..Default::default()
    };
    // Entries modified this close to the base scan may have changed again
    // within the timestamp's resolution, so they are always copied.
    let racy_ns = base.map(|b| b.taken_ns - 2_000_000_000);
    let changed = |rel: &Path, s: &Stat| match base {
        None => true,
        Some(b) => match b.entries.get(rel) {
            None => true,
            Some(old) if old.kind != s.kind || old.mode != s.mode => true,
            Some(_) if s.kind == Kind::Dir => false,
            Some(old) => {
                old.size != s.size
                    || old.mtime_ns != s.mtime_ns
                    || racy_ns.is_some_and(|r| s.mtime_ns >= r)
            }
        },
    };
    // Directories are created writable and get their real modes at the end,
    // so read-only directories can still be filled.
    let mut modes: Vec<(PathBuf, u32)> = Vec::new();
    // Sorted order puts every directory before its contents.
    for (rel, s) in &now.entries {
        if !changed(rel, s) {
            continue;
        }
        let from = src.join(rel);
        let to = layer.join(rel);
        if let Some(parent) = to.parent() {
            if !parent.exists() {
                // A changed file deep in an unchanged directory: create the
                // directory chain with the parent's modes so the merged view
                // shows the same permissions.
                ensure_dirs(
                    src,
                    layer,
                    rel.parent().unwrap_or(Path::new("")),
                    &mut modes,
                )?;
            }
        }
        match s.kind {
            Kind::Dir => {
                fs::create_dir_all(&to).at(&to)?;
                modes.push((to, s.mode));
                st.dirs += 1;
            }
            Kind::Symlink => {
                let target = fs::read_link(&from).at(&from)?;
                std::os::unix::fs::symlink(&target, &to).at(&to)?;
                set_mtime(&to, s.mtime_ns, true)?;
                st.symlinks += 1;
            }
            Kind::File => {
                let bytes = reflink_copy::reflink_or_copy(&from, &to)
                    .at(&to)?
                    .unwrap_or(s.size);
                fs::set_permissions(&to, fs::Permissions::from_mode(s.mode)).at(&to)?;
                set_mtime(&to, s.mtime_ns, false)?;
                st.files += 1;
                st.bytes += bytes;
            }
        }
    }
    if let Some(base) = base {
        for rel in base.entries.keys() {
            if now.entries.contains_key(rel) {
                continue;
            }
            // Only the topmost deleted path needs a whiteout; and a path
            // whose parent became a file is hidden by that file already.
            let parent = rel.parent().unwrap_or(Path::new(""));
            let parent_is_dir = parent.as_os_str().is_empty()
                || now.entries.get(parent).is_some_and(|p| p.kind == Kind::Dir);
            if !parent_is_dir {
                continue;
            }
            ensure_dirs(src, layer, parent, &mut modes)?;
            whiteout(&layer.join(rel))?;
            st.whiteouts += 1;
        }
    }
    for (dir, mode) in modes.iter().rev() {
        fs::set_permissions(dir, fs::Permissions::from_mode(*mode)).at(dir)?;
    }
    Ok(st)
}

/// Create `layer/rel` and its ancestors, queueing the modes they have in
/// `src` (the merged view shows the topmost layer's directory modes).
fn ensure_dirs(
    src: &Path,
    layer: &Path,
    rel: &Path,
    modes: &mut Vec<(PathBuf, u32)>,
) -> Result<()> {
    let mut cur = PathBuf::new();
    for c in rel.components() {
        cur.push(c);
        let to = layer.join(&cur);
        if to.exists() {
            continue;
        }
        fs::create_dir(&to).at(&to)?;
        if let Ok(m) = fs::symlink_metadata(src.join(&cur)) {
            modes.push((to, m.mode() & 0o7777));
        }
    }
    Ok(())
}

fn whiteout(path: &Path) -> Result<()> {
    let p = cpath(path).map_err(|e| Error::Io {
        path: path.to_path_buf(),
        source: std::io::Error::other(e),
    })?;
    // A whiteout is a 0/0 character device; unprivileged users may create
    // one since Linux 5.8.
    // SAFETY: p is a valid C string.
    if unsafe { libc::mknod(p.as_ptr(), libc::S_IFCHR, 0) } != 0 {
        return Err(std::io::Error::last_os_error()).at(path);
    }
    Ok(())
}

fn set_mtime(path: &Path, mtime_ns: i128, nofollow: bool) -> Result<()> {
    let p = cpath(path).map_err(|e| Error::Io {
        path: path.to_path_buf(),
        source: std::io::Error::other(e),
    })?;
    let t = libc::timespec {
        tv_sec: mtime_ns.div_euclid(1_000_000_000) as libc::time_t,
        tv_nsec: mtime_ns.rem_euclid(1_000_000_000) as libc::c_long,
    };
    let omit = libc::timespec {
        tv_sec: 0,
        tv_nsec: libc::UTIME_OMIT,
    };
    let flags = if nofollow {
        libc::AT_SYMLINK_NOFOLLOW
    } else {
        0
    };
    // SAFETY: p is valid, times points at two timespecs.
    if unsafe { libc::utimensat(libc::AT_FDCWD, p.as_ptr(), [omit, t].as_ptr(), flags) } != 0 {
        return Err(std::io::Error::last_os_error()).at(path);
    }
    Ok(())
}

/// Remove a directory tree that may contain read-only directories (layers
/// keep the parent's modes).
pub fn remove_tree(path: &Path) {
    fn writable(p: &Path) {
        if let Ok(rd) = fs::read_dir(p) {
            let _ = fs::set_permissions(p, fs::Permissions::from_mode(0o700));
            for e in rd.flatten() {
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    writable(&e.path());
                }
            }
        }
    }
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    writable(path);
    let _ = fs::remove_dir_all(path);
}

// ---- views -----------------------------------------------------------------

/// How an overlay fork was set up, reported with the fork.
#[derive(Clone, Debug, Serialize)]
pub struct OverlayInfo {
    pub mounter: Mounter,
    /// Whether processes outside agentvc see the mount (false for userns).
    pub global: bool,
    /// Top lower layer (`.agentvc/layers/L<n>`).
    pub layer: i64,
    /// Length of the lower chain.
    pub layers: usize,
    /// True when the parent had not changed since its last layer, so that
    /// layer was shared instead of writing a new one.
    pub reused: bool,
    /// What freezing the parent cost.
    pub freeze: LayerStats,
}

/// What [`Space::prepare`] did.
#[derive(Clone, Debug, Serialize)]
pub struct PrepareResult {
    pub attempt: i64,
    /// Why nothing was done, when nothing was.
    pub skipped: Option<String>,
    /// The attempt's latest layer.
    pub layer: Option<i64>,
    pub layers: usize,
    /// True when the view had not changed since its last layer.
    pub reused: bool,
    pub freeze: LayerStats,
    pub millis: u128,
}

impl Space {
    fn space_dir(&self) -> PathBuf {
        self.workdir().join(SPACE_DIR)
    }

    fn layer_dir(&self, layer: i64) -> PathBuf {
        self.space_dir().join("layers").join(format!("L{layer}"))
    }

    fn layer_manifest(&self, layer: i64) -> PathBuf {
        self.space_dir()
            .join("layers")
            .join(format!("L{layer}.manifest"))
    }

    fn overlay_dirs(&self, attempt: i64) -> (PathBuf, PathBuf) {
        let d = self.space_dir().join("overlay").join(format!("a{attempt}"));
        (d.join("upper"), d.join("work"))
    }

    /// Layer ids from `top` down to the base.
    fn layer_chain(&self, top: i64) -> Result<Vec<i64>> {
        let mut chain = vec![top];
        let mut cur = top;
        while let Some(p) = self
            .db
            .query_row(
                "SELECT parent FROM layers WHERE id = ?1",
                params![cur],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten()
        {
            chain.push(p);
            cur = p;
        }
        Ok(chain)
    }

    /// The overlay record of an attempt: (top layer, mounter).
    fn overlay_of(&self, attempt: i64) -> Result<Option<(i64, Mounter)>> {
        Ok(self
            .db
            .query_row(
                "SELECT top_layer, mounter FROM overlays WHERE attempt_id = ?1",
                params![attempt],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?
            .and_then(|(l, m)| Mounter::parse(&m).map(|m| (l, m))))
    }

    /// Freeze the parent's view into a layer: the whole view the first time,
    /// a delta against its last layer afterwards, or that layer itself when
    /// nothing changed. Returns (layer, cost, reused).
    fn freeze(&self, parent: &Attempt) -> Result<(i64, LayerStats, bool)> {
        // One freeze per parent at a time: a fork that races a `prepare`
        // waits for it and then reuses its layer instead of copying again.
        let _lock = self.freeze_lock(parent.id)?;
        // Holding the lock, any unfinished layer of this parent is a leftover
        // of a crashed or killed freeze.
        let stale: Vec<i64> = self
            .db
            .prepare("SELECT id FROM layers WHERE source_attempt = ?1 AND complete = 0")?
            .query_map(params![parent.id], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        for l in stale {
            remove_tree(&self.layer_dir(l));
            let _ = fs::remove_file(self.layer_manifest(l));
            self.db
                .execute("DELETE FROM layers WHERE id = ?1", params![l])?;
        }
        let last_own: Option<i64> = self
            .db
            .query_row(
                "SELECT id FROM layers WHERE source_attempt = ?1 AND complete = 1
                 ORDER BY id DESC LIMIT 1",
                params![parent.id],
                |r| r.get(0),
            )
            .optional()?;
        let mut base = match last_own {
            Some(l) => Some(l),
            None => self.overlay_of(parent.id)?.map(|(l, _)| l),
        };
        if let Some(b) = base {
            if self.layer_chain(b)?.len() >= MAX_CHAIN {
                base = None; // compact: start a fresh base layer
            }
        }
        let base_manifest = match base {
            Some(b) => Manifest::load(&self.layer_manifest(b)).ok(),
            None => None,
        };
        if base_manifest.is_none() {
            base = None;
        }
        let now = Manifest::scan(&parent.view_path)?;

        self.db.execute(
            "INSERT INTO layers (parent, source_attempt, created_at) VALUES (?1, ?2, ?3)",
            params![base, parent.id, now_secs()],
        )?;
        let id = self.db.last_insert_rowid();
        let dir = self.layer_dir(id);
        let mut written = write_layer(&parent.view_path, &now, base_manifest.as_ref(), &dir);
        if written.is_err() && base.is_some() {
            // For example whiteouts refused on an old kernel: write a full
            // base layer instead of a delta.
            remove_tree(&dir);
            base = None;
            self.db
                .execute("UPDATE layers SET parent = NULL WHERE id = ?1", params![id])?;
            written = write_layer(&parent.view_path, &now, None, &dir);
        }
        let stats = match written {
            Ok(s) => s,
            Err(e) => {
                remove_tree(&dir);
                self.db
                    .execute("DELETE FROM layers WHERE id = ?1", params![id])?;
                return Err(e);
            }
        };
        if let (Some(b), true) = (base, stats.is_empty()) {
            remove_tree(&dir);
            self.db
                .execute("DELETE FROM layers WHERE id = ?1", params![id])?;
            return Ok((b, stats, true));
        }
        now.save(&self.layer_manifest(id))?;
        self.db.execute(
            "UPDATE layers SET files = ?1, bytes = ?2, complete = 1 WHERE id = ?3",
            params![stats.files as i64, stats.bytes as i64, id],
        )?;
        Ok((id, stats, false))
    }

    /// An exclusive `flock` on `.agentvc/layers/aN.lock`, released on drop.
    fn freeze_lock(&self, attempt: i64) -> Result<fs::File> {
        use std::os::fd::AsRawFd;
        let dir = self.space_dir().join("layers");
        fs::create_dir_all(&dir).at(&dir)?;
        let path = dir.join(format!("a{attempt}.lock"));
        let f = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .at(&path)?;
        loop {
            // SAFETY: the fd is open for the duration of the call.
            if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } == 0 {
                return Ok(f);
            }
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::Interrupted {
                return Err(e).at(&path);
            }
        }
    }

    /// The mounter a fork in this space would use for an overlay view, or
    /// why it would not use overlayfs (reflinks work, or nothing can mount).
    pub fn overlay_mounter(&self) -> std::result::Result<Mounter, String> {
        let views = self.space_dir().join("views");
        if crate::view::reflinks_supported(&views).is_ok() {
            return Err("reflinks work here, so forks do not need overlay layers".into());
        }
        let scratch = self.space_dir().join("tmp");
        fs::create_dir_all(&scratch).map_err(|e| format!("{}: {e}", scratch.display()))?;
        probe(&scratch).map_err(|why| format!("overlayfs unavailable ({why})"))
    }

    /// Freeze an attempt's view into a layer now, off the fork path, so the
    /// next fork from it only has to write what changed since. The first
    /// prepare copies the whole view; later ones write deltas or nothing.
    /// Skipped, with the reason, when forks here would not use overlayfs.
    pub fn prepare(&self, attempt_id: i64) -> Result<PrepareResult> {
        let start = std::time::Instant::now();
        let attempt = self.attempt(attempt_id)?;
        let skip = |reason: String| PrepareResult {
            attempt: attempt_id,
            skipped: Some(reason),
            layer: None,
            layers: 0,
            reused: false,
            freeze: LayerStats::default(),
            millis: start.elapsed().as_millis(),
        };
        if !attempt.status.is_open() {
            return Ok(skip(format!("attempt a{attempt_id} is {}", attempt.status)));
        }
        if let Err(why) = self.overlay_mounter() {
            return Ok(skip(why));
        }
        if !self.ensure_view(&attempt)? {
            return Ok(skip(format!("attempt a{attempt_id} has no view")));
        }
        let (layer, freeze, reused) = self.freeze(&attempt)?;
        self.record(
            "agentvc",
            "attempt.prepare",
            serde_json::json!({ "attempt": attempt_id, "layer": layer, "reused": reused }),
        )?;
        Ok(PrepareResult {
            attempt: attempt_id,
            skipped: None,
            layer: Some(layer),
            layers: self.layer_chain(layer)?.len(),
            reused,
            freeze,
            millis: start.elapsed().as_millis(),
        })
    }

    fn mount_spec(&self, attempt: &Attempt, top: i64) -> Result<MountSpec> {
        let (upper, work) = self.overlay_dirs(attempt.id);
        Ok(MountSpec {
            lowers: self
                .layer_chain(top)?
                .into_iter()
                .map(|l| self.layer_dir(l))
                .collect(),
            upper,
            work,
            target: attempt.view_path.clone(),
            volatile: false,
        })
    }

    /// Fork `parent` into a new overlay view at `view_path` for attempt
    /// `id`. The inner error is the reason overlayfs is unavailable, for
    /// callers that fall back to copying.
    pub(crate) fn overlay_fork(
        &self,
        parent: &Attempt,
        id: i64,
        view_path: &Path,
    ) -> Result<std::result::Result<OverlayInfo, String>> {
        let scratch = self.space_dir().join("tmp");
        fs::create_dir_all(&scratch).at(&scratch)?;
        let mounter = match probe(&scratch) {
            Ok(m) => m,
            Err(why) => return Ok(Err(why)),
        };
        let (layer, freeze, reused) = self.freeze(parent)?;
        let (upper, work) = self.overlay_dirs(id);
        for d in [&upper, &work, &view_path.to_path_buf()] {
            fs::create_dir_all(d).at(d)?;
        }
        let spec = MountSpec {
            lowers: self
                .layer_chain(layer)?
                .into_iter()
                .map(|l| self.layer_dir(l))
                .collect(),
            upper,
            work,
            target: view_path.to_path_buf(),
            volatile: false,
        };
        if let Err(why) = mount(mounter, &spec) {
            let _ = fs::remove_dir_all(spec.upper.parent().expect("has parent"));
            let _ = fs::remove_dir(view_path);
            return Ok(Err(why));
        }
        self.db.execute(
            "INSERT INTO overlays (attempt_id, top_layer, mounter) VALUES (?1, ?2, ?3)",
            params![id, layer, mounter.as_str()],
        )?;
        Ok(Ok(OverlayInfo {
            mounter,
            global: mounter.is_global(),
            layer,
            layers: spec.lowers.len(),
            reused,
            freeze,
        }))
    }

    /// Mount an overlay view unless it is mounted already.
    pub(crate) fn mount_overlay_view(&self, attempt: &Attempt) -> Result<bool> {
        if is_mounted(&attempt.view_path) {
            return Ok(true);
        }
        let unmounted = |reason: String| Error::ViewUnmounted {
            id: attempt.id,
            reason,
        };
        let Some((top, mounter)) = self.overlay_of(attempt.id)? else {
            return Err(unmounted("no overlay record".into()));
        };
        let spec = self.mount_spec(attempt, top)?;
        if !spec.upper.is_dir() {
            return Err(unmounted(format!(
                "its upper layer {} is missing",
                spec.upper.display()
            )));
        }
        fs::create_dir_all(&spec.target).at(&spec.target)?;
        mount(mounter, &spec).map_err(unmounted)?;
        Ok(true)
    }

    /// Unmount the overlay views of closed attempts, so mounts do not pile
    /// up. Their contents stay and come back on demand. A view that is busy
    /// (a shell still inside it) stays mounted.
    pub(crate) fn release_views(&self, ids: &[i64]) {
        for &id in ids {
            let (Ok(a), Ok(Some((_, m)))) = (self.attempt(id), self.overlay_of(id)) else {
                continue;
            };
            if !a.status.is_open() {
                let _ = unmount(m, &a.view_path);
            }
        }
    }

    /// Unmount every overlay view this process can see. Their upper layers
    /// stay, so the views come back on demand.
    pub fn unmount_views(&self) -> Result<()> {
        let mut stmt = self
            .db
            .prepare("SELECT attempt_id, mounter FROM overlays ORDER BY attempt_id DESC")?;
        let rows: Vec<(i64, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        for (id, m) in rows {
            let a = self.attempt(id)?;
            if let Some(m) = Mounter::parse(&m) {
                unmount(m, &a.view_path).map_err(|e| Error::Io {
                    path: a.view_path.clone(),
                    source: std::io::Error::other(e),
                })?;
            }
        }
        Ok(())
    }
}

fn now_secs() -> i64 {
    (now_ns() / 1_000_000_000) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::space::{ForkRequest, Ref, ROOT_ATTEMPT};
    use crate::view::{Backend, BackendChoice};

    const HOUR_AGO: i128 = 3600 * 1_000_000_000;

    /// Backdate everything under `dir`, so the racy-entry rule does not force
    /// copies of files the test just wrote.
    fn age(dir: &Path) {
        let t = now_ns() - HOUR_AGO;
        for (rel, _) in Manifest::scan(dir).unwrap().entries {
            set_mtime(&dir.join(rel), t, true).unwrap();
        }
    }

    #[test]
    fn layers_hold_only_changes_and_whiteouts() {
        let tmp = crate::testutil::tempdir();
        let src = tmp.path().join("src");
        fs::create_dir_all(src.join("target/debug")).unwrap();
        fs::create_dir_all(src.join("gone/deep")).unwrap();
        fs::create_dir_all(src.join(".git")).unwrap();
        fs::write(src.join("main.rs"), "v1").unwrap();
        fs::write(src.join("keep.rs"), "same").unwrap();
        fs::write(src.join("old.rs"), "old").unwrap();
        fs::write(src.join("gone/deep/x"), "x").unwrap();
        fs::write(src.join("target/debug/app"), "bin").unwrap();
        std::os::unix::fs::symlink("main.rs", src.join("link")).unwrap();
        age(&src);

        let m1 = Manifest::scan(&src).unwrap();
        assert!(!m1.entries.contains_key(Path::new(".git")));
        let base = tmp.path().join("L1");
        let full = write_layer(&src, &m1, None, &base).unwrap();
        assert_eq!(full.files, 5);
        assert_eq!(
            fs::read_to_string(base.join("target/debug/app")).unwrap(),
            "bin"
        );
        // mtimes are kept, so the layer scans the same as its source.
        let again = Manifest::scan(&base).unwrap();
        assert_eq!(again.entries, m1.entries);

        let manifest = tmp.path().join("L1.manifest");
        m1.save(&manifest).unwrap();
        let m1 = Manifest::load(&manifest).unwrap();
        assert_eq!(m1.entries, again.entries);

        // Nothing changed: an empty delta.
        let m2 = Manifest::scan(&src).unwrap();
        let none = write_layer(&src, &m2, Some(&m1), &tmp.path().join("L2")).unwrap();
        assert!(none.is_empty(), "{none:?}");

        fs::write(src.join("main.rs"), "v2 longer").unwrap();
        fs::write(src.join("new.rs"), "new").unwrap();
        fs::remove_file(src.join("old.rs")).unwrap();
        fs::remove_dir_all(src.join("gone")).unwrap();
        let m3 = Manifest::scan(&src).unwrap();
        let delta = tmp.path().join("L3");
        let d = write_layer(&src, &m3, Some(&m1), &delta).unwrap();
        assert_eq!((d.files, d.whiteouts), (2, 2), "{d:?}");
        assert!(!delta.join("keep.rs").exists());
        assert!(!delta.join("target").exists());
        use std::os::unix::fs::FileTypeExt;
        for w in ["old.rs", "gone"] {
            let m = fs::symlink_metadata(delta.join(w)).unwrap();
            assert!(m.file_type().is_char_device() && m.rdev() == 0, "{w}");
        }
        assert!(!delta.join("gone/deep").exists());
    }

    /// Unmounts the space's overlay views when a test ends, even on panic.
    struct Unmount<'a>(&'a Space);
    impl Drop for Unmount<'_> {
        fn drop(&mut self) {
            let _ = self.0.unmount_views();
        }
    }

    fn fork(space: &Space, from: i64, backend: BackendChoice) -> crate::ForkResult {
        space
            .fork(&ForkRequest {
                from: Some(Ref::Attempt(from)),
                actor: "test".into(),
                backend,
                ..Default::default()
            })
            .unwrap()
    }

    #[test]
    fn overlay_views_fork_share_layers_and_remount() {
        let tmp = crate::testutil::tempdir();
        let w = tmp.path();
        fs::write(w.join("main.rs"), "fn main() {}\n").unwrap();
        fs::write(w.join("old.rs"), "old\n").unwrap();
        fs::write(w.join(".gitignore"), "target/\n").unwrap();
        fs::create_dir_all(w.join("target")).unwrap();
        fs::write(w.join("target/cache"), "warm").unwrap();
        age(w);
        let space = Space::init(w).unwrap();
        let _guard = Unmount(&space);
        let scratch = w.join(".agentvc/tmp");
        fs::create_dir_all(&scratch).unwrap();
        if let Err(why) = probe(&scratch) {
            eprintln!("skipping: overlayfs unavailable ({why})");
            // Auto must then fall back to copy and say why.
            let r = fork(&space, ROOT_ATTEMPT, BackendChoice::Auto);
            if r.backend == Backend::Copy {
                assert!(r.backend_reason.unwrap().contains("overlayfs unavailable"));
            }
            assert!(matches!(
                space.fork(&ForkRequest {
                    backend: BackendChoice::Overlay,
                    ..Default::default()
                }),
                Err(Error::BackendUnavailable { .. })
            ));
            return;
        }

        let a = fork(&space, ROOT_ATTEMPT, BackendChoice::Overlay);
        assert_eq!(a.backend, Backend::Overlay);
        let info = a.overlay.clone().unwrap();
        assert_eq!((info.layers, info.reused), (1, false));
        let v = a.attempt.view_path.clone();
        assert!(is_mounted(&v));
        assert_eq!(fs::read_to_string(v.join("target/cache")).unwrap(), "warm");
        // Writes stay in the view.
        fs::write(v.join("main.rs"), "fn main() { a() }\n").unwrap();
        assert_eq!(
            fs::read_to_string(w.join("main.rs")).unwrap(),
            "fn main() {}\n"
        );

        // The parent has not changed: its layer is shared.
        let b = fork(&space, ROOT_ATTEMPT, BackendChoice::Overlay);
        let bi = b.overlay.unwrap();
        assert!(bi.reused && bi.layer == info.layer, "{bi:?}");

        // The parent changes: the next fork adds a thin delta layer.
        fs::write(w.join("main.rs"), "fn main() { parent() }\n").unwrap();
        fs::remove_file(w.join("old.rs")).unwrap();
        let c = fork(&space, ROOT_ATTEMPT, BackendChoice::Overlay);
        let ci = c.overlay.unwrap();
        assert_eq!((ci.layers, ci.freeze.files, ci.freeze.whiteouts), (2, 1, 1));
        let cv = &c.attempt.view_path;
        assert_eq!(
            fs::read_to_string(cv.join("main.rs")).unwrap(),
            "fn main() { parent() }\n"
        );
        assert!(!cv.join("old.rs").exists());
        // Earlier views still see the parent as it was when they forked.
        assert!(b.attempt.view_path.join("old.rs").exists());

        // Forking a fork stacks on the fork's own chain.
        let d = fork(&space, a.attempt.id, BackendChoice::Auto);
        assert_eq!(d.backend, Backend::Overlay);
        let di = d.overlay.unwrap();
        assert_eq!(di.layers, 2);
        assert_eq!(
            fs::read_to_string(d.attempt.view_path.join("main.rs")).unwrap(),
            "fn main() { a() }\n"
        );

        // A restart: views come back on demand with their writes intact.
        let before = space.head_snapshot(a.attempt.id).unwrap().tree;
        space.unmount_views().unwrap();
        assert!(!is_mounted(&v));
        let reopened = Space::discover(w).unwrap();
        let s = reopened
            .snapshot(a.attempt.id, "after-restart", None)
            .unwrap();
        assert!(is_mounted(&v));
        assert_eq!(s.snapshot.tree, before);
        assert_eq!(
            fs::read_to_string(v.join("main.rs")).unwrap(),
            "fn main() { a() }\n"
        );
    }

    /// A space with some backdated files, or None when this host cannot
    /// mount overlays (the test then has nothing to check).
    fn overlay_space() -> Option<(crate::testutil::TestDir, Space)> {
        let tmp = crate::testutil::tempdir();
        let w = tmp.path();
        fs::create_dir_all(w.join("target/debug")).unwrap();
        for i in 0..20 {
            fs::write(w.join(format!("target/debug/f{i}")), format!("{i}")).unwrap();
        }
        fs::write(w.join("main.rs"), "fn main() {}\n").unwrap();
        age(w);
        let space = Space::init(w).unwrap();
        match space.overlay_mounter() {
            Ok(_) => Some((tmp, space)),
            Err(why) => {
                eprintln!("skipping: {why}");
                assert!(space.prepare(ROOT_ATTEMPT).unwrap().skipped.is_some());
                None
            }
        }
    }

    #[test]
    fn prepare_makes_the_first_fork_thin() {
        let Some((tmp, space)) = overlay_space() else {
            return;
        };
        let _guard = Unmount(&space);
        // A crashed freeze left an unfinished layer behind.
        space
            .db
            .execute(
                "INSERT INTO layers (source_attempt, created_at) VALUES (1, 0)",
                [],
            )
            .unwrap();
        let stale = space.db.last_insert_rowid();
        fs::create_dir_all(space.layer_dir(stale).join("junk")).unwrap();

        let p = space.prepare(ROOT_ATTEMPT).unwrap();
        assert!(p.skipped.is_none(), "{p:?}");
        assert_eq!((p.freeze.files, p.layers, p.reused), (21, 1, false));
        // Removed (its id may be reused by the new layer).
        assert!(!space.layer_dir(stale).join("junk").exists());

        let f = fork(&space, ROOT_ATTEMPT, BackendChoice::Auto);
        let o = f.overlay.unwrap();
        assert!(o.reused && Some(o.layer) == p.layer, "{o:?}");
        assert_eq!(o.freeze.files, 0);
        assert_eq!(
            fs::read_to_string(f.attempt.view_path.join("target/debug/f7")).unwrap(),
            "7"
        );

        // Preparing again with no changes writes nothing.
        let again = space.prepare(ROOT_ATTEMPT).unwrap();
        assert!(again.reused && again.layer == p.layer);
        drop(tmp);
    }

    #[test]
    fn prepare_and_fork_at_once_copy_the_parent_once() {
        let Some((tmp, space)) = overlay_space() else {
            return;
        };
        let _guard = Unmount(&space);
        let w = tmp.path().to_path_buf();
        let barrier = std::sync::Barrier::new(2);
        let (p, f) = std::thread::scope(|s| {
            let p = s.spawn(|| {
                let sp = Space::discover(&w).unwrap();
                barrier.wait();
                sp.prepare(ROOT_ATTEMPT).unwrap()
            });
            let f = s.spawn(|| {
                let sp = Space::discover(&w).unwrap();
                barrier.wait();
                let r = fork(&sp, ROOT_ATTEMPT, BackendChoice::Overlay);
                r.overlay.unwrap()
            });
            (p.join().unwrap(), f.join().unwrap())
        });
        // Whichever froze first copied the files; the other reused its layer.
        assert_eq!(p.freeze.files + f.freeze.files, 21, "{p:?} {f:?}");
        assert_eq!(Some(f.layer), p.layer);
        let complete: i64 = space
            .db
            .query_row("SELECT count(*) FROM layers WHERE complete = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(complete, 1);
    }
}
