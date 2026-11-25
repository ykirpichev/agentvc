//! Stat index: remembers `(size, mtime, inode, mode) -> blob id` per path for
//! one view, so a snapshot only reads and hashes files that changed.
//!
//! Like git's index, an entry is only trusted when the file's mtime is safely
//! older than the moment the index was written ("racy" entries modified
//! within [`RACY_WINDOW_NS`] of that moment are rehashed), so coarse mtime
//! granularity can't hide an edit.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{IoContext, Result};
use crate::objects::{Mode, Oid};

const HEADER: &str = "agentvc-index v1";

/// Entries modified this close to (or after) the index write are not trusted.
pub const RACY_WINDOW_NS: i128 = 2_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    pub size: u64,
    pub mtime_ns: i128,
    pub ino: u64,
    pub mode: Mode,
}

impl Stat {
    pub fn of(meta: &fs::Metadata, mode: Mode) -> Stat {
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i128)
            .unwrap_or(0);
        #[cfg(unix)]
        let ino = std::os::unix::fs::MetadataExt::ino(meta);
        #[cfg(not(unix))]
        let ino = 0;
        Stat {
            size: meta.len(),
            mtime_ns,
            ino,
            mode,
        }
    }
}

fn mode_char(m: Mode) -> char {
    match m {
        Mode::File => 'f',
        Mode::Executable => 'x',
        Mode::Symlink => 'l',
        Mode::Tree => 'd',
    }
}

fn char_mode(c: &str) -> Option<Mode> {
    Some(match c {
        "f" => Mode::File,
        "x" => Mode::Executable,
        "l" => Mode::Symlink,
        _ => return None,
    })
}

fn now_ns() -> i128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0)
}

#[derive(Default)]
pub struct StatIndex {
    written_at_ns: i128,
    entries: HashMap<PathBuf, (Stat, Oid)>,
    next: HashMap<PathBuf, (Stat, Oid)>,
    pub hits: u64,
    pub misses: u64,
}

impl StatIndex {
    /// Load an index; a missing or unreadable file gives an empty index.
    pub fn load(path: &Path) -> StatIndex {
        let mut idx = StatIndex::default();
        let Ok(text) = fs::read_to_string(path) else {
            return idx;
        };
        let mut lines = text.lines();
        let Some(written) = lines
            .next()
            .and_then(|h| h.strip_prefix(HEADER))
            .and_then(|t| t.trim().parse::<i128>().ok())
        else {
            return idx;
        };
        idx.written_at_ns = written;
        for line in lines {
            let mut parts = line.splitn(6, ' ');
            let (Some(size), Some(mtime), Some(ino), Some(mode), Some(oid), Some(path)) = (
                parts.next(),
                parts.next(),
                parts.next(),
                parts.next(),
                parts.next(),
                parts.next(),
            ) else {
                continue;
            };
            let (Ok(size), Ok(mtime_ns), Ok(ino), Some(mode), Ok(oid)) = (
                size.parse(),
                mtime.parse(),
                ino.parse(),
                char_mode(mode),
                Oid::from_hex(oid),
            ) else {
                continue;
            };
            idx.entries.insert(
                PathBuf::from(path),
                (
                    Stat {
                        size,
                        mtime_ns,
                        ino,
                        mode,
                    },
                    oid,
                ),
            );
        }
        idx
    }

    /// The blob id recorded for `rel` if its stat is unchanged and not racy.
    pub fn lookup(&mut self, rel: &Path, stat: &Stat) -> Option<Oid> {
        let (old, oid) = self.entries.get(rel)?;
        if old == stat && stat.mtime_ns + RACY_WINDOW_NS < self.written_at_ns {
            self.hits += 1;
            Some(*oid)
        } else {
            None
        }
    }

    /// Record the entry for the index that [`StatIndex::save`] writes.
    pub fn record(&mut self, rel: PathBuf, stat: Stat, oid: Oid, was_hit: bool) {
        if !was_hit {
            self.misses += 1;
        }
        self.next.insert(rel, (stat, oid));
    }

    /// Whether the recorded entries differ from the loaded ones, so the file
    /// needs rewriting. An all-hit capture of the same paths changes nothing.
    pub fn is_dirty(&self) -> bool {
        self.misses > 0 || self.next.len() != self.entries.len() || self.written_at_ns == 0
    }

    /// Write the entries recorded during this capture, replacing the file.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).at(dir)?;
        }
        let mut out = Vec::with_capacity(self.next.len() * 96);
        writeln!(out, "{HEADER} {}", now_ns()).at(path)?;
        for (rel, (s, oid)) in &self.next {
            let p = rel.to_string_lossy();
            if p.contains('\n') {
                continue; // never trusted; rehashed every time
            }
            writeln!(
                out,
                "{} {} {} {} {} {}",
                s.size,
                s.mtime_ns,
                s.ino,
                mode_char(s.mode),
                oid,
                p
            )
            .at(path)?;
        }
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, out).at(&tmp)?;
        fs::rename(&tmp, path).at(path)
    }
}
