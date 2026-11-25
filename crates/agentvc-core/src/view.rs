//! Forking views: a new attempt directory that starts as a copy of its parent,
//! including ignored build and dependency directories, so the child starts
//! with a warm build.
//!
//! Files are cloned with reflinks (`FICLONE` on Linux btrfs/XFS, `clonefile`
//! on APFS) when the filesystem supports them, which makes a fork cost
//! metadata only. Elsewhere files are copied. An overlayfs backend is planned.

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
}

impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Backend::Reflink => "reflink",
            Backend::Copy => "copy",
            Backend::Mixed => "mixed",
            Backend::Materialize => "materialize",
        }
    }
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

/// Fork directory `src` into a new directory `dst`.
pub fn fork_dir(src: &Path, dst: &Path) -> Result<ForkStats> {
    let start = Instant::now();
    let mut stats = ForkStats::default();
    fs::create_dir_all(dst).at(dst)?;
    copy_tree(src, dst, &mut stats, true)?;
    stats.millis = start.elapsed().as_millis();
    Ok(stats)
}

fn copy_tree(src: &Path, dst: &Path, stats: &mut ForkStats, top: bool) -> Result<()> {
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
            copy_tree(&from, &to, stats, false)?;
            let perms = fs::metadata(&from).at(&from)?.permissions();
            fs::set_permissions(&to, perms).at(&to)?;
        } else if ft.is_file() {
            match reflink_copy::reflink_or_copy(&from, &to).at(&to)? {
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
        let stats = fork_dir(&src, &dst).unwrap();
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
