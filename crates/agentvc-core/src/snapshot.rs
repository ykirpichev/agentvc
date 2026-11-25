//! Capturing a directory as a git tree, writing a tree back out, and diffing
//! two trees.
//!
//! Capture follows git's view of the world: `.gitignore` rules apply, `.git`
//! and `.agentvc` are skipped, and empty directories are not recorded.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{IoContext, Result};
use crate::objects::{Mode, ObjectStore, Oid, TreeEntry};

/// Directory names never captured in a snapshot, at any depth.
pub const ALWAYS_SKIPPED: &[&str] = &[".git", ".agentvc"];

/// Capture `dir` into `store` and return the root tree id.
pub fn capture(store: &ObjectStore, dir: &Path) -> Result<Oid> {
    let mut files: BTreeMap<PathBuf, (Mode, Oid)> = BTreeMap::new();
    let walker = ignore::WalkBuilder::new(dir)
        .hidden(false)
        .git_ignore(true)
        .git_exclude(false)
        .git_global(false)
        .require_git(false)
        .parents(false)
        .follow_links(false)
        .filter_entry(|e| {
            !(e.file_type().is_some_and(|t| t.is_dir())
                && ALWAYS_SKIPPED.iter().any(|s| e.file_name() == *s))
        })
        .build();
    for entry in walker {
        let entry = entry?;
        let Some(ft) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if ft.is_dir() {
            continue;
        }
        let rel = path
            .strip_prefix(dir)
            .expect("walk stays under dir")
            .to_path_buf();
        if ft.is_symlink() {
            let target = fs::read_link(path).at(path)?;
            let oid = store.write_blob(target.to_string_lossy().as_bytes())?;
            files.insert(rel, (Mode::Symlink, oid));
        } else if ft.is_file() {
            let data = fs::read(path).at(path)?;
            let oid = store.write_blob(&data)?;
            files.insert(rel, (file_mode(path)?, oid));
        }
    }
    build_tree(store, &files, Path::new(""))
}

#[cfg(unix)]
fn file_mode(path: &Path) -> Result<Mode> {
    use std::os::unix::fs::PermissionsExt;
    let meta = fs::metadata(path).at(path)?;
    Ok(if meta.permissions().mode() & 0o111 != 0 {
        Mode::Executable
    } else {
        Mode::File
    })
}

#[cfg(not(unix))]
fn file_mode(_path: &Path) -> Result<Mode> {
    Ok(Mode::File)
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
