//! Browse the whole code of any ref (#171): list a directory of a stored
//! tree, list every path for go-to-file, search file contents, and capture
//! a live working tree (an agent's worktree, a workspace checkout) into the
//! store so it reads like any snapshot.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{Error, Result};
use crate::objects::{Mode, ObjectStore, Oid};
use crate::snapshot::{self, ChangeKind, FileChange};
use crate::space::{Space, SPACE_DIR};

/// Files larger than this are not searched.
pub const MAX_SEARCH_BYTES: usize = 1024 * 1024;

/// One entry of a listed directory.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    /// Path from the root of the tree.
    pub path: String,
    /// `dir`, `file`, `executable` or `symlink`.
    pub kind: &'static str,
    /// Against the base, when one was given: `added`, `modified` or
    /// `removed` (a removed file is listed although the tree lacks it). A
    /// directory is `modified` when anything under it changed, `added`
    /// when it is new.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change: Option<ChangeKind>,
    /// Changed files under a directory.
    #[serde(skip_serializing_if = "is_zero")]
    pub changed: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

fn kind_name(m: Mode) -> &'static str {
    match m {
        Mode::Tree => "dir",
        Mode::File => "file",
        Mode::Executable => "executable",
        Mode::Symlink => "symlink",
    }
}

/// Normalize a directory path: no leading, trailing or doubled slashes.
pub fn clean_dir(path: &str) -> String {
    path.split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect::<Vec<_>>()
        .join("/")
}

/// The tree at `dir` inside `root`, or `None` when there is no such
/// directory.
pub fn subtree(store: &ObjectStore, root: &Oid, dir: &str) -> Result<Option<Oid>> {
    let mut cur = *root;
    for part in dir.split('/').filter(|p| !p.is_empty()) {
        match store
            .read_tree(&cur)?
            .into_iter()
            .find(|e| e.name == part && e.mode == Mode::Tree)
        {
            Some(e) => cur = e.oid,
            None => return Ok(None),
        }
    }
    Ok(Some(cur))
}

/// Directory `dir` of `tree`: directories first, then files, by name. With
/// `changes` (the diff against the tree `base`), entries are marked and files the
/// base had here but the tree lacks are listed as removed. `None` when the
/// directory exists on neither side.
pub fn list_dir(
    store: &ObjectStore,
    tree: &Oid,
    dir: &str,
    changes: Option<(&[FileChange], &Oid)>,
) -> Result<Option<Vec<DirEntry>>> {
    let dir = clean_dir(dir);
    let prefix = if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    };
    let join = |name: &str| format!("{prefix}{name}");
    let mut out: Vec<DirEntry> = match subtree(store, tree, &dir)? {
        Some(t) => store
            .read_tree(&t)?
            .into_iter()
            .map(|e| DirEntry {
                path: join(&e.name),
                kind: kind_name(e.mode),
                name: e.name,
                change: None,
                changed: 0,
            })
            .collect(),
        None => Vec::new(),
    };
    let mut seen_dir = !out.is_empty() || dir.is_empty();
    if let Some((changes, base)) = changes {
        let in_base =
            |child: &str| -> Result<bool> { Ok(subtree(store, base, &join(child))?.is_some()) };
        // Per direct child: what changed at or under it.
        let mut by_child: BTreeMap<&str, (usize, Option<ChangeKind>, bool, bool)> = BTreeMap::new();
        for c in changes {
            let Some(rest) = c.path.strip_prefix(&prefix) else {
                continue;
            };
            seen_dir = true;
            let (child, nested) = match rest.split_once('/') {
                Some((c, _)) => (c, true),
                None => (rest, false),
            };
            let e = by_child.entry(child).or_insert((0, None, true, false));
            e.0 += 1;
            if nested {
                e.3 = true;
                if c.kind != ChangeKind::Added {
                    e.2 = false;
                }
            } else {
                e.1 = Some(c.kind);
            }
        }
        for (child, (count, own, all_added, nested)) in by_child {
            match out.iter_mut().find(|e| e.name == child) {
                Some(e) if e.kind == "dir" => {
                    e.changed = count;
                    e.change = Some(if all_added && !in_base(child)? {
                        ChangeKind::Added
                    } else {
                        ChangeKind::Modified
                    });
                }
                Some(e) => e.change = own,
                None => out.push(DirEntry {
                    name: child.to_string(),
                    path: join(child),
                    kind: if nested { "dir" } else { "file" },
                    change: Some(ChangeKind::Removed),
                    changed: if nested { count } else { 0 },
                }),
            }
        }
    }
    if !seen_dir {
        return Ok(None);
    }
    out.sort_by(|a, b| {
        (a.kind != "dir")
            .cmp(&(b.kind != "dir"))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(Some(out))
}

/// Every file path of `tree`, sorted, at most `cap` (then `true`).
pub fn all_paths(store: &ObjectStore, tree: &Oid, cap: usize) -> Result<(Vec<String>, bool)> {
    let mut out = Vec::new();
    let truncated = walk(store, tree, "", &mut |p, _, _| {
        out.push(p.to_string());
        out.len() < cap
    })?;
    Ok((out, truncated))
}

/// Visit every file of `tree` in path order; stop (and return true) when
/// `f` returns false.
fn walk(
    store: &ObjectStore,
    tree: &Oid,
    prefix: &str,
    f: &mut dyn FnMut(&str, Mode, &Oid) -> bool,
) -> Result<bool> {
    for e in store.read_tree(tree)? {
        let path = if prefix.is_empty() {
            e.name.clone()
        } else {
            format!("{prefix}/{}", e.name)
        };
        if e.mode == Mode::Tree {
            if walk(store, &e.oid, &path, f)? {
                return Ok(true);
            }
        } else if !f(&path, e.mode, &e.oid) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// What to search for.
#[derive(Clone, Debug)]
pub struct SearchQuery {
    pub text: String,
    pub regex: bool,
    pub case_sensitive: bool,
    /// Only paths matching one of these globs (`src/**`, `*.rs`).
    pub paths: Vec<String>,
    /// Stop after this many matching lines.
    pub limit: usize,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct SearchMatch {
    pub path: String,
    /// 1-based.
    pub line: usize,
    /// The line, cut to 400 characters.
    pub text: String,
    /// Byte ranges of the matches within `text`.
    pub ranges: Vec<(usize, usize)>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SearchResult {
    pub matches: Vec<SearchMatch>,
    /// Files with at least one match.
    pub files: usize,
    /// Files searched.
    pub searched: usize,
    /// The limit cut the results short.
    pub truncated: bool,
}

const LINE_CAP: usize = 400;

/// Search the text files of `tree` line by line, in path order.
pub fn search(store: &ObjectStore, tree: &Oid, q: &SearchQuery) -> Result<SearchResult> {
    let bad = |e: String| Error::BadQuery(e);
    let pattern = if q.regex {
        q.text.clone()
    } else {
        regex::escape(&q.text)
    };
    if q.text.is_empty() {
        return Err(bad("search for what? (q is empty)".into()));
    }
    let re = regex::RegexBuilder::new(&pattern)
        .case_insensitive(!q.case_sensitive)
        .size_limit(1 << 22)
        .build()
        .map_err(|e| bad(format!("bad regex: {e}")))?;
    let globs = if q.paths.is_empty() {
        None
    } else {
        let mut b = globset::GlobSetBuilder::new();
        for g in &q.paths {
            // A bare directory name means everything under it.
            let g = g.trim_matches('/');
            let pat = if g.contains(['*', '?', '[']) {
                g.to_string()
            } else {
                format!("{{{g},{g}/**}}")
            };
            b.add(
                globset::GlobBuilder::new(&pat)
                    .literal_separator(false)
                    .build()
                    .map_err(|e| bad(format!("bad path glob {g:?}: {e}")))?,
            );
        }
        Some(b.build().map_err(|e| bad(e.to_string()))?)
    };
    let mut out = SearchResult {
        matches: Vec::new(),
        files: 0,
        searched: 0,
        truncated: false,
    };
    let limit = q.limit.max(1);
    let mut err = None;
    walk(store, tree, "", &mut |path, mode, oid| {
        if mode == Mode::Symlink || globs.as_ref().is_some_and(|g| !g.is_match(path)) {
            return true;
        }
        let data = match store.read_blob(oid) {
            Ok(d) => d,
            Err(e) => {
                err = Some(e);
                return false;
            }
        };
        if data.len() > MAX_SEARCH_BYTES || data.contains(&0) {
            return true;
        }
        let Ok(text) = std::str::from_utf8(&data) else {
            return true;
        };
        out.searched += 1;
        let mut hit = false;
        for (i, line) in text.lines().enumerate() {
            let ranges: Vec<(usize, usize)> = re
                .find_iter(line)
                .filter(|m| m.start() < LINE_CAP)
                .map(|m| (m.start(), m.end().min(LINE_CAP)))
                .collect();
            if ranges.is_empty() {
                continue;
            }
            if out.matches.len() >= limit {
                out.truncated = true;
                return false;
            }
            hit = true;
            let mut cut = line.len().min(LINE_CAP);
            while !line.is_char_boundary(cut) {
                cut -= 1;
            }
            let ranges = ranges
                .into_iter()
                .map(|(s, e)| (s.min(cut), e.min(cut)))
                .collect();
            out.matches.push(SearchMatch {
                path: path.to_string(),
                line: i + 1,
                text: line[..cut].to_string(),
                ranges,
            });
        }
        if hit {
            out.files += 1;
        }
        true
    })?;
    if let Some(e) = err {
        return Err(e);
    }
    Ok(out)
}

/// Changed files between `base` and `tree`, as a map for marking paths.
pub fn change_map(changes: &[FileChange]) -> HashMap<&str, ChangeKind> {
    changes.iter().map(|c| (c.path.as_str(), c.kind)).collect()
}

impl Space {
    /// Capture a live working tree (outside the space's views) into the
    /// store, honoring its `.gitignore`, and return the tree id. `key` names
    /// the stat index kept under `.agentvc/browse/`, so only files that
    /// changed since the last capture are hashed again.
    pub fn capture_worktree(&self, key: &str, dir: &Path) -> Result<Oid> {
        if !dir.is_dir() {
            return Err(Error::NotFound {
                kind: "folder",
                id: dir.to_string_lossy().into_owned(),
            });
        }
        let idx_dir: PathBuf = self.workdir().join(SPACE_DIR).join("browse");
        std::fs::create_dir_all(&idx_dir).map_err(|e| Error::Io {
            path: idx_dir.clone(),
            source: e,
        })?;
        let safe: String = key
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let idx = idx_dir.join(format!("{safe}.idx"));
        snapshot::capture_indexed(self.objects(), dir, Some(&idx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn store_with(files: &[(&str, &str)]) -> (tempfile::TempDir, ObjectStore, Oid) {
        let tmp = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(tmp.path().join("objects"));
        let work = tmp.path().join("w");
        for (p, c) in files {
            let p = work.join(p);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, c).unwrap();
        }
        let t = snapshot::capture(&store, &work).unwrap();
        (tmp, store, t)
    }

    #[test]
    fn lists_directories_first_and_marks_changes() {
        let (tmp, s, b) = store_with(&[("a.txt", "1\n"), ("src/x.rs", "x\n"), ("old/y", "y")]);
        let work = tmp.path().join("w2");
        fs::create_dir_all(work.join("src/new")).unwrap();
        fs::write(work.join("a.txt"), "2\n").unwrap();
        fs::write(work.join("src/x.rs"), "x\n").unwrap();
        fs::write(work.join("src/new/z.rs"), "z").unwrap();
        let h = snapshot::capture(&s, &work).unwrap();
        let changes = snapshot::diff(&s, &b, &h).unwrap();
        let ch = Some((changes.as_slice(), &b));
        let root = list_dir(&s, &h, "", ch).unwrap().unwrap();
        let view: Vec<(&str, &str, Option<ChangeKind>, usize)> = root
            .iter()
            .map(|e| (e.name.as_str(), e.kind, e.change, e.changed))
            .collect();
        assert_eq!(
            view,
            [
                ("old", "dir", Some(ChangeKind::Removed), 1),
                ("src", "dir", Some(ChangeKind::Modified), 1),
                ("a.txt", "file", Some(ChangeKind::Modified), 0),
            ]
        );
        let src = list_dir(&s, &h, "/src/", ch).unwrap().unwrap();
        assert_eq!(src[0].path, "src/new");
        assert_eq!(src[0].change, Some(ChangeKind::Added));
        assert_eq!(src[1].path, "src/x.rs");
        assert_eq!(src[1].change, None);
        // A removed directory still lists what it lost.
        let old = list_dir(&s, &h, "old", ch).unwrap().unwrap();
        assert_eq!(old[0].path, "old/y");
        assert_eq!(old[0].change, Some(ChangeKind::Removed));
        assert!(list_dir(&s, &h, "nope", ch).unwrap().is_none());
        assert!(list_dir(&s, &b, "src/x.rs", None).unwrap().is_none());
    }

    #[test]
    fn all_paths_caps() {
        let (_t, s, t) = store_with(&[("b", ""), ("a/1", ""), ("a/2", "")]);
        assert_eq!(
            all_paths(&s, &t, 10).unwrap(),
            (vec!["a/1".into(), "a/2".into(), "b".into()], false)
        );
        assert!(all_paths(&s, &t, 2).unwrap().1);
    }

    #[test]
    fn search_finds_lines_with_ranges_and_filters_paths() {
        let (_t, s, t) = store_with(&[
            ("src/a.rs", "fn main() {\n    let Foo = foo();\n}\n"),
            ("docs/b.md", "foo bar foo\n"),
            ("bin.dat", "foo\0"),
        ]);
        let q = |text: &str, regex: bool, case: bool, paths: &[&str]| SearchQuery {
            text: text.into(),
            regex,
            case_sensitive: case,
            paths: paths.iter().map(|p| p.to_string()).collect(),
            limit: 100,
        };
        let r = search(&s, &t, &q("foo", false, false, &[])).unwrap();
        let got: Vec<(&str, usize, usize)> = r
            .matches
            .iter()
            .map(|m| (m.path.as_str(), m.line, m.ranges.len()))
            .collect();
        assert_eq!(got, [("docs/b.md", 1, 2), ("src/a.rs", 2, 2)]);
        assert_eq!((r.files, r.searched), (2, 2));
        let r = search(&s, &t, &q("Foo", false, true, &[])).unwrap();
        assert_eq!(r.matches.len(), 1);
        assert_eq!(r.matches[0].ranges, [(8, 11)]);
        let r = search(&s, &t, &q(r"fo+\(", true, true, &["src"])).unwrap();
        assert_eq!(r.matches[0].path, "src/a.rs");
        let r = search(&s, &t, &q("foo", false, false, &["*.md"])).unwrap();
        assert_eq!(r.files, 1);
        let mut lim = q("foo", false, false, &[]);
        lim.limit = 1;
        assert!(search(&s, &t, &lim).unwrap().truncated);
        assert!(search(&s, &t, &q("(", true, false, &[])).is_err());
    }
}
