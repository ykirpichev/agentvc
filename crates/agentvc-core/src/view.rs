//! Forking views: a new attempt directory that starts as a copy of its parent,
//! including ignored build and dependency directories, so the child starts
//! with a warm build.
//!
//! Files are cloned with reflinks (`FICLONE` on Linux btrfs/XFS, `clonefile`
//! on APFS) when the filesystem supports them, which makes a fork cost
//! metadata only. Without reflinks, Linux forks use overlayfs (see
//! [`crate::overlay`]), and files are copied as the last resort.

use std::fs;
use std::path::Path;
use std::time::Instant;

use serde::Serialize;

use crate::error::{IoContext, Result};

/// Directory names never copied into a view: the space itself, and git
/// metadata (views are tracked by agentvc, not by git).
pub const NOT_COPIED: &[&str] = &[".agentvc", ".git"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    /// Every file was cloned with a reflink.
    Reflink,
    /// Every file was copied byte for byte.
    Copy,
    /// Some files were reflinked and some copied.
    Mixed,
    /// The view was written out from a stored tree (cold: no ignored files).
    Materialize,
    /// An overlayfs mount over frozen layers of the parent.
    Overlay,
}

impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Backend::Reflink => "reflink",
            Backend::Copy => "copy",
            Backend::Mixed => "mixed",
            Backend::Materialize => "materialize",
            Backend::Overlay => "overlay",
        }
    }
}

/// Which backend a fork should use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendChoice {
    /// reflink, else overlayfs, else copy.
    #[default]
    Auto,
    Reflink,
    Overlay,
    Copy,
}

impl BackendChoice {
    pub fn as_str(self) -> &'static str {
        match self {
            BackendChoice::Auto => "auto",
            BackendChoice::Reflink => "reflink",
            BackendChoice::Overlay => "overlay",
            BackendChoice::Copy => "copy",
        }
    }
}

impl std::str::FromStr for BackendChoice {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, String> {
        Ok(match s {
            "auto" => BackendChoice::Auto,
            "reflink" => BackendChoice::Reflink,
            "overlay" | "overlayfs" => BackendChoice::Overlay,
            "copy" => BackendChoice::Copy,
            _ => {
                return Err(format!(
                    "unknown backend {s:?}: expected auto, reflink, overlay or copy"
                ))
            }
        })
    }
}

/// Whether files in `dir` can be cloned with reflinks. Tries a real clone of
/// a scratch file.
pub fn reflinks_supported(dir: &Path) -> std::result::Result<(), String> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let a = dir.join(format!(".reflink-probe-{}-{seq}", std::process::id()));
    let b = a.with_extension("clone");
    fs::write(&a, b"probe").map_err(|e| format!("{}: {e}", a.display()))?;
    let r = reflink_copy::reflink(&a, &b);
    let _ = fs::remove_file(&a);
    let _ = fs::remove_file(&b);
    r.map_err(|e| format!("no reflinks on this filesystem ({e})"))
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ForkStats {
    pub files: u64,
    pub reflinked: u64,
    pub copied: u64,
    pub bytes_copied: u64,
    pub dirs: u64,
    pub symlinks: u64,
    pub millis: u128,
}

impl ForkStats {
    pub fn backend(&self) -> Backend {
        match (self.reflinked, self.copied) {
            (_, 0) if self.reflinked > 0 => Backend::Reflink,
            (0, _) => Backend::Copy,
            _ => Backend::Mixed,
        }
    }
}

/// Fork directory `src` into a new directory `dst`, reflinking where
/// possible, or copying every file when `copy_only` is set.
pub fn fork_dir(src: &Path, dst: &Path, copy_only: bool) -> Result<ForkStats> {
    let start = Instant::now();
    let mut stats = ForkStats::default();
    fs::create_dir_all(dst).at(dst)?;
    copy_tree(src, dst, &mut stats, true, copy_only)?;
    stats.millis = start.elapsed().as_millis();
    Ok(stats)
}

fn copy_tree(
    src: &Path,
    dst: &Path,
    stats: &mut ForkStats,
    top: bool,
    copy_only: bool,
) -> Result<()> {
    for entry in fs::read_dir(src).at(src)? {
        let entry = entry.at(src)?;
        let name = entry.file_name();
        if top && NOT_COPIED.iter().any(|n| name == *n) {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        let ft = entry.file_type().at(&from)?;
        if ft.is_symlink() {
            let target = fs::read_link(&from).at(&from)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target, &to).at(&to)?;
            #[cfg(not(unix))]
            fs::copy(&from, &to).at(&to)?;
            stats.symlinks += 1;
        } else if ft.is_dir() {
            fs::create_dir(&to).at(&to)?;
            stats.dirs += 1;
            copy_tree(&from, &to, stats, false, copy_only)?;
            let perms = fs::metadata(&from).at(&from)?.permissions();
            fs::set_permissions(&to, perms).at(&to)?;
        } else if ft.is_file() {
            let cloned = if copy_only {
                Some(fs::copy(&from, &to).at(&to)?)
            } else {
                reflink_copy::reflink_or_copy(&from, &to).at(&to)?
            };
            match cloned {
                None => {
                    stats.reflinked += 1;
                    let perms = fs::metadata(&from).at(&from)?.permissions();
                    fs::set_permissions(&to, perms).at(&to)?;
                }
                Some(bytes) => {
                    stats.copied += 1;
                    stats.bytes_copied += bytes;
                }
            }
            stats.files += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fork_copies_ignored_dirs_but_not_git_or_space() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(src.join("target/debug")).unwrap();
        fs::create_dir_all(src.join(".git")).unwrap();
        fs::create_dir_all(src.join(".agentvc/views")).unwrap();
        fs::write(src.join("main.rs"), "fn main() {}").unwrap();
        fs::write(src.join("target/debug/app"), "binary").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("main.rs", src.join("link.rs")).unwrap();

        let dst = tmp.path().join("dst");
        let stats = fork_dir(&src, &dst, false).unwrap();
        assert_eq!(
            fs::read_to_string(dst.join("target/debug/app")).unwrap(),
            "binary"
        );
        assert!(!dst.join(".git").exists());
        assert!(!dst.join(".agentvc").exists());
        assert_eq!(stats.files, 2);
        #[cfg(unix)]
        assert_eq!(
            fs::read_link(dst.join("link.rs")).unwrap(),
            Path::new("main.rs")
        );

        // The fork is independent of its parent.
        fs::write(dst.join("main.rs"), "changed").unwrap();
        assert_eq!(
            fs::read_to_string(src.join("main.rs")).unwrap(),
            "fn main() {}"
        );
    }
}
