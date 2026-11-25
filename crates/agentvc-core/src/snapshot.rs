//! Capturing a directory as a git tree, writing a tree back out, and diffing
//! two trees.
//!
//! Capture follows git's view of the world: `.gitignore` rules apply, `.git`
//! and `.agentvc` are skipped, and empty directories are not recorded.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ignore::WalkState;

use serde::Serialize;

use crate::error::{Error, IoContext, Result};
use crate::index::{Stat, StatIndex};
use crate::objects::{Mode, ObjectStore, Oid, TreeEntry};

/// Directory names never captured in a snapshot, at any depth.
pub const ALWAYS_SKIPPED: &[&str] = &[".git", ".agentvc"];

/// Files (relative to the view root) never captured: machine-local settings
/// such as the Claude Code hook config written by `agentvc hooks`.
pub const ALWAYS_SKIPPED_FILES: &[&str] = &[".claude/settings.local.json"];

/// Capture `dir` into `store` and return the root tree id.
pub fn capture(store: &ObjectStore, dir: &Path) -> Result<Oid> {
    capture_indexed(store, dir, None)
}

/// Capture `dir`, reusing blob ids from the stat index at `index_path` for
/// files whose size, mtime, inode and mode are unchanged, and rewriting the
/// index afterwards.
pub fn capture_indexed(store: &ObjectStore, dir: &Path, index_path: Option<&Path>) -> Result<Oid> {
    let mut index = index_path.map(StatIndex::load).unwrap_or_default();
    let mut files: BTreeMap<PathBuf, (Mode, Oid)> = BTreeMap::new();
    let walker = ignore::WalkBuilder::new(dir)
        .hidden(false)
        .git_ignore(true)
        .git_exclude(false)
        .git_global(false)
        .require_git(false)
        .parents(false)
        .follow_links(false)
        .filter_entry({
            let root = dir.to_path_buf();
            move |e| {
                let is_dir = e.file_type().is_some_and(|t| t.is_dir());
                if is_dir && ALWAYS_SKIPPED.iter().any(|s| e.file_name() == *s) {
                    return false;
                }
                let rel = e.path().strip_prefix(&root).unwrap_or(e.path());
                !ALWAYS_SKIPPED_FILES.iter().any(|f| rel == Path::new(f))
            }
        })
        .build_parallel();

    // Walk and stat in parallel; stats dominate the cost of an unchanged view.
    let found: Mutex<Vec<(PathBuf, bool, fs::Metadata)>> = Mutex::new(Vec::new());
    let failure: Mutex<Option<Error>> = Mutex::new(None);
    walker.run(|| {
        Box::new(|entry| {
            let fail = |e: Error| {
                failure.lock().expect("lock").get_or_insert(e);
                WalkState::Quit
            };
            let entry = match entry {
                Ok(e) => e,
                Err(e) => return fail(e.into()),
            };
            let Some(ft) = entry.file_type() else {
                return WalkState::Continue;
            };
            if !(ft.is_symlink() || ft.is_file()) {
                return WalkState::Continue;
            }
            let path = entry.path();
            let meta = match fs::symlink_metadata(path) {
                Ok(m) => m,
                Err(source) => {
                    return fail(Error::Io {
                        path: path.to_path_buf(),
                        source,
                    })
                }
            };
            let rel = path
                .strip_prefix(dir)
                .expect("walk stays under dir")
                .to_path_buf();
            found
                .lock()
                .expect("lock")
                .push((rel, ft.is_symlink(), meta));
            WalkState::Continue
        })
    });
    if let Some(e) = failure.into_inner().expect("lock") {
        return Err(e);
    }

    for (rel, is_symlink, meta) in found.into_inner().expect("lock") {
        let path = dir.join(&rel);
        let mode = if is_symlink {
            Mode::Symlink
        } else {
            mode_from_meta(&meta)
        };
        let stat = Stat::of(&meta, mode);
        let cached = index.lookup(&rel, &stat);
        let oid = match cached {
            Some(oid) => oid,
            None if is_symlink => {
                let target = fs::read_link(&path).at(&path)?;
                store.write_blob(target.to_string_lossy().as_bytes())?
            }
            None => store.write_blob(&fs::read(&path).at(&path)?)?,
        };
        index.record(rel.clone(), stat, oid, cached.is_some());
        files.insert(rel, (mode, oid));
    }
    let tree = build_tree(store, &files, Path::new(""))?;
    if let Some(p) = index_path.filter(|_| index.is_dirty()) {
        index.save(p)?;
    }
    Ok(tree)
}

#[cfg(unix)]
fn mode_from_meta(meta: &fs::Metadata) -> Mode {
    use std::os::unix::fs::PermissionsExt;
    if meta.permissions().mode() & 0o111 != 0 {
        Mode::Executable
    } else {
        Mode::File
    }
}

#[cfg(not(unix))]
fn mode_from_meta(_meta: &fs::Metadata) -> Mode {
    Mode::File
}

/// Build the tree for directory `prefix` from a flat, sorted map of files.
fn build_tree(
    store: &ObjectStore,
    files: &BTreeMap<PathBuf, (Mode, Oid)>,
    prefix: &Path,
) -> Result<Oid> {
    let mut entries = Vec::new();
    let mut subdirs: Vec<String> = Vec::new();
    for (path, (mode, oid)) in files.range(prefix.to_path_buf()..) {
        let Ok(rest) = path.strip_prefix(prefix) else {
            break;
        };
        let mut comps = rest.components();
        let first = comps
            .next()
            .expect("file path is non-empty")
            .as_os_str()
            .to_string_lossy()
            .into_owned();
        if comps.next().is_none() {
            entries.push(TreeEntry {
                name: first,
                mode: *mode,
                oid: *oid,
            });
        } else if subdirs.last() != Some(&first) {
            subdirs.push(first);
        }
    }
    for name in subdirs {
        let oid = build_tree(store, files, &prefix.join(&name))?;
        entries.push(TreeEntry {
            name,
            mode: Mode::Tree,
            oid,
        });
    }
    store.write_tree(&mut entries)
}

/// Write the tree `oid` out into `dest`, which must not exist yet.
pub fn materialize(store: &ObjectStore, oid: &Oid, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest).at(dest)?;
    for e in store.read_tree(oid)? {
        let path = dest.join(&e.name);
        match e.mode {
            Mode::Tree => materialize(store, &e.oid, &path)?,
            Mode::File | Mode::Executable => {
                fs::write(&path, store.read_blob(&e.oid)?).at(&path)?;
                #[cfg(unix)]
                if e.mode == Mode::Executable {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).at(&path)?;
                }
            }
            Mode::Symlink => {
                let target = String::from_utf8_lossy(&store.read_blob(&e.oid)?).into_owned();
                #[cfg(unix)]
                std::os::unix::fs::symlink(&target, &path).at(&path)?;
                #[cfg(not(unix))]
                fs::write(&path, target).at(&path)?;
            }
        }
    }
    Ok(())
}

/// Flatten a tree into `path -> (mode, blob id)`.
pub fn flatten(store: &ObjectStore, oid: &Oid) -> Result<BTreeMap<String, (Mode, Oid)>> {
    let mut out = BTreeMap::new();
    flatten_into(store, oid, "", &mut out)?;
    Ok(out)
}

fn flatten_into(
    store: &ObjectStore,
    oid: &Oid,
    prefix: &str,
    out: &mut BTreeMap<String, (Mode, Oid)>,
) -> Result<()> {
    for e in store.read_tree(oid)? {
        let path = format!("{prefix}{}", e.name);
        if e.mode == Mode::Tree {
            flatten_into(store, &e.oid, &format!("{path}/"), out)?;
        } else {
            out.insert(path, (e.mode, e.oid));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    Added,
    Modified,
    Removed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FileChange {
    pub path: String,
    pub kind: ChangeKind,
}

/// Files that differ between trees `a` and `b`, sorted by path.
pub fn diff(store: &ObjectStore, a: &Oid, b: &Oid) -> Result<Vec<FileChange>> {
    if a == b {
        return Ok(Vec::new());
    }
    let fa = flatten(store, a)?;
    let fb = flatten(store, b)?;
    let mut out = Vec::new();
    for (path, va) in &fa {
        match fb.get(path) {
            None => out.push(FileChange {
                path: path.clone(),
                kind: ChangeKind::Removed,
            }),
            Some(vb) if vb != va => out.push(FileChange {
                path: path.clone(),
                kind: ChangeKind::Modified,
            }),
            _ => {}
        }
    }
    for path in fb.keys() {
        if !fa.contains_key(path) {
            out.push(FileChange {
                path: path.clone(),
                kind: ChangeKind::Added,
            });
        }
    }
    out.sort_by(|x, y| x.path.cmp(&y.path));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, ObjectStore, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(tmp.path().join("objects"));
        let work = tmp.path().join("work");
        fs::create_dir_all(work.join("src/nested")).unwrap();
        fs::write(work.join("README.md"), "hi\n").unwrap();
        fs::write(work.join("src/lib.rs"), "fn a() {}\n").unwrap();
        fs::write(work.join("src/nested/x.txt"), "x\n").unwrap();
        fs::write(work.join(".gitignore"), "target/\n").unwrap();
        fs::create_dir_all(work.join(".claude")).unwrap();
        fs::write(work.join(".claude/settings.local.json"), "{}").unwrap();
        fs::create_dir_all(work.join("target")).unwrap();
        fs::write(work.join("target/big.bin"), "ignored").unwrap();
        fs::create_dir_all(work.join(".git")).unwrap();
        fs::write(work.join(".git/HEAD"), "ref").unwrap();
        (tmp, store, work)
    }

    #[test]
    fn capture_skips_ignored_and_round_trips() {
        let (tmp, store, work) = setup();
        let tree = capture(&store, &work).unwrap();
        let files = flatten(&store, &tree).unwrap();
        let names: Vec<_> = files.keys().cloned().collect();
        assert_eq!(
            names,
            vec![".gitignore", "README.md", "src/lib.rs", "src/nested/x.txt"]
        );

        let out = tmp.path().join("out");
        materialize(&store, &tree, &out).unwrap();
        assert_eq!(
            fs::read_to_string(out.join("src/lib.rs")).unwrap(),
            "fn a() {}\n"
        );
        assert_eq!(capture(&store, &out).unwrap(), tree);
    }

    #[test]
    fn index_skips_unchanged_files_and_sees_edits() {
        let (tmp, store, work) = setup();
        let idx = tmp.path().join("index");
        let plain = capture(&store, &work).unwrap();
        // First indexed capture: everything is a miss, same tree as unindexed.
        assert_eq!(capture_indexed(&store, &work, Some(&idx)).unwrap(), plain);

        // Age the index so entries are not racy, then capture again.
        let mut text = fs::read_to_string(&idx).unwrap();
        let header_end = text.find('\n').unwrap();
        text.replace_range(..header_end, "agentvc-index v1 99999999999999999999");
        fs::write(&idx, &text).unwrap();
        let mut loaded = StatIndex::load(&idx);
        let rel = Path::new("src/lib.rs");
        let meta = fs::symlink_metadata(work.join(rel)).unwrap();
        assert!(loaded.lookup(rel, &Stat::of(&meta, Mode::File)).is_some());

        // An edit that keeps the size is still seen (mtime/inode change).
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(work.join("src/lib.rs"), "fn z() {}\n").unwrap();
        fs::write(&idx, &text).unwrap();
        let t2 = capture_indexed(&store, &work, Some(&idx)).unwrap();
        assert_ne!(t2, plain);
        assert_eq!(t2, capture(&store, &work).unwrap());
    }

    #[test]
    fn racy_entries_are_rehashed() {
        let (tmp, store, work) = setup();
        let idx = tmp.path().join("index");
        capture_indexed(&store, &work, Some(&idx)).unwrap();
        // Files were written just now, inside the racy window: no hits.
        let mut loaded = StatIndex::load(&idx);
        let rel = Path::new("README.md");
        let meta = fs::symlink_metadata(work.join(rel)).unwrap();
        assert!(loaded.lookup(rel, &Stat::of(&meta, Mode::File)).is_none());
    }

    #[test]
    fn capture_is_deterministic_and_diff_reports_changes() {
        let (_tmp, store, work) = setup();
        let t1 = capture(&store, &work).unwrap();
        assert_eq!(capture(&store, &work).unwrap(), t1);

        fs::write(work.join("src/lib.rs"), "fn b() {}\n").unwrap();
        fs::remove_file(work.join("README.md")).unwrap();
        fs::write(work.join("NEW.md"), "new\n").unwrap();
        let t2 = capture(&store, &work).unwrap();
        let d = diff(&store, &t1, &t2).unwrap();
        assert_eq!(
            d,
            vec![
                FileChange {
                    path: "NEW.md".into(),
                    kind: ChangeKind::Added
                },
                FileChange {
                    path: "README.md".into(),
                    kind: ChangeKind::Removed
                },
                FileChange {
                    path: "src/lib.rs".into(),
                    kind: ChangeKind::Modified
                },
            ]
        );
    }
}
