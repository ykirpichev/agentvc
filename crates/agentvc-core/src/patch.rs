//! Unified diffs between two trees, for `agentvc diff --patch`.
//!
//! The file list comes from [`crate::snapshot::diff`]; this module adds the
//! line-level hunks, `--stat` counts and the git-style text rendering. Binary
//! files (a NUL byte in the first 8000 bytes, invalid UTF-8, or over
//! [`MAX_TEXT_BYTES`]) are reported, never dumped.

use serde::Serialize;
use similar::{ChangeTag, TextDiff};

use crate::error::Result;
use crate::objects::{Mode, ObjectStore, Oid};
use crate::snapshot::{self, ChangeKind};

/// Files larger than this are treated as binary.
pub const MAX_TEXT_BYTES: usize = 8 * 1024 * 1024;

/// Lines of context around each change, as in `git diff`.
pub const DEFAULT_CONTEXT: usize = 3;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Hunk {
    pub old_start: usize,
    pub old_lines: usize,
    pub new_start: usize,
    pub new_lines: usize,
    /// Lines prefixed with `' '`, `'-'` or `'+'`, without their newline. A
    /// line missing its final newline is followed by
    /// `\ No newline at end of file`.
    pub lines: Vec<String>,
}

impl Hunk {
    pub fn header(&self) -> String {
        format!(
            "@@ -{} +{} @@",
            range(self.old_start, self.old_lines),
            range(self.new_start, self.new_lines)
        )
    }
}

fn range(start: usize, lines: usize) -> String {
    if lines == 1 {
        start.to_string()
    } else {
        format!("{start},{lines}")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FilePatch {
    pub path: String,
    pub kind: ChangeKind,
    /// `100644`, `100755` or `120000`; `None` on the side where the file
    /// does not exist.
    pub old_mode: Option<&'static str>,
    pub new_mode: Option<&'static str>,
    /// True when either side is not text; then `hunks` is empty.
    pub binary: bool,
    pub added: usize,
    pub removed: usize,
    pub hunks: Vec<Hunk>,
}

/// Text of a blob, or `None` when it is binary.
fn text(store: &ObjectStore, oid: &Oid) -> Result<Option<String>> {
    let data = store.read_blob(oid)?;
    if data.len() > MAX_TEXT_BYTES || data[..data.len().min(8000)].contains(&0) {
        return Ok(None);
    }
    Ok(String::from_utf8(data).ok())
}

/// Hunks and line counts between two texts.
pub fn text_hunks(old: &str, new: &str, context: usize) -> (Vec<Hunk>, usize, usize) {
    let diff = TextDiff::configure()
        .algorithm(similar::Algorithm::Patience)
        .diff_lines(old, new);
    let (mut added, mut removed) = (0, 0);
    let mut hunks = Vec::new();
    for group in diff.grouped_ops(context) {
        let (Some(first), Some(last)) = (group.first(), group.last()) else {
            continue;
        };
        let (o, n) = (first.old_range().start, first.new_range().start);
        let old_lines = last.old_range().end - o;
        let new_lines = last.new_range().end - n;
        let mut lines = Vec::new();
        for op in &group {
            for change in diff.iter_changes(op) {
                let sign = match change.tag() {
                    ChangeTag::Equal => ' ',
                    ChangeTag::Delete => {
                        removed += 1;
                        '-'
                    }
                    ChangeTag::Insert => {
                        added += 1;
                        '+'
                    }
                };
                let value = change.value();
                // A CRLF line keeps its '\r', as in git's output.
                let line = value.strip_suffix('\n').unwrap_or(value);
                lines.push(format!("{sign}{line}"));
                if !value.ends_with('\n') {
                    lines.push("\\ No newline at end of file".into());
                }
            }
        }
        hunks.push(Hunk {
            // Like git: a range of zero lines starts at the line before it.
            old_start: if old_lines == 0 { o } else { o + 1 },
            old_lines,
            new_start: if new_lines == 0 { n } else { n + 1 },
            new_lines,
            lines,
        });
    }
    (hunks, added, removed)
}

/// Per-file patches between trees `a` and `b`, sorted by path.
pub fn diff_trees(store: &ObjectStore, a: &Oid, b: &Oid, context: usize) -> Result<Vec<FilePatch>> {
    let changes = snapshot::diff(store, a, b)?;
    if changes.is_empty() {
        return Ok(Vec::new());
    }
    let fa = snapshot::flatten(store, a)?;
    let fb = snapshot::flatten(store, b)?;
    let mut out = Vec::with_capacity(changes.len());
    for c in changes {
        let old = fa.get(&c.path).copied();
        let new = fb.get(&c.path).copied();
        let read = |side: Option<(Mode, Oid)>| -> Result<Option<Option<String>>> {
            side.map(|(_, oid)| text(store, &oid)).transpose()
        };
        let (ot, nt) = (read(old)?, read(new)?);
        let binary = matches!(ot, Some(None)) || matches!(nt, Some(None));
        let (hunks, added, removed) = if binary {
            (Vec::new(), 0, 0)
        } else {
            text_hunks(
                ot.flatten().as_deref().unwrap_or(""),
                nt.flatten().as_deref().unwrap_or(""),
                context,
            )
        };
        out.push(FilePatch {
            path: c.path,
            kind: c.kind,
            old_mode: old.map(|(m, _)| m.as_str()),
            new_mode: new.map(|(m, _)| m.as_str()),
            binary,
            added,
            removed,
            hunks,
        });
    }
    Ok(out)
}

/// Git-style unified diff text for `patches`.
pub fn render(patches: &[FilePatch]) -> String {
    let mut s = String::new();
    for p in patches {
        s += &format!("diff --git a/{0} b/{0}\n", p.path);
        match (p.old_mode, p.new_mode) {
            (None, Some(m)) => s += &format!("new file mode {m}\n"),
            (Some(m), None) => s += &format!("deleted file mode {m}\n"),
            (Some(o), Some(n)) if o != n => s += &format!("old mode {o}\nnew mode {n}\n"),
            _ => {}
        }
        let old = if p.old_mode.is_some() {
            format!("a/{}", p.path)
        } else {
            "/dev/null".into()
        };
        let new = if p.new_mode.is_some() {
            format!("b/{}", p.path)
        } else {
            "/dev/null".into()
        };
        if p.binary {
            s += &format!("Binary files {old} and {new} differ\n");
            continue;
        }
        if p.hunks.is_empty() {
            continue;
        }
        s += &format!("--- {old}\n+++ {new}\n");
        for h in &p.hunks {
            s += &h.header();
            s.push('\n');
            for l in &h.lines {
                s += l;
                s.push('\n');
            }
        }
    }
    s
}

const BAR_WIDTH: usize = 40;

/// `git diff --stat`-style summary of `patches`.
pub fn render_stat(patches: &[FilePatch]) -> String {
    let width = patches.iter().map(|p| p.path.len()).max().unwrap_or(0);
    let most = patches
        .iter()
        .map(|p| p.added + p.removed)
        .max()
        .unwrap_or(0);
    let mut s = String::new();
    let (mut added, mut removed) = (0, 0);
    for p in patches {
        added += p.added;
        removed += p.removed;
        if p.binary {
            s += &format!(" {:<width$} | Bin\n", p.path);
        } else {
            let n = p.added + p.removed;
            // Like git, scale every bar by the largest change so the widest
            // is BAR_WIDTH, keeping at least one mark for each side.
            let scale = |x: usize| {
                if most <= BAR_WIDTH || x == 0 {
                    x
                } else {
                    (x * BAR_WIDTH / most).max(1)
                }
            };
            let bar = format!(
                "{}{}",
                "+".repeat(scale(p.added)),
                "-".repeat(scale(p.removed))
            );
            s += &format!(" {:<width$} | {n:>4} {bar}\n", p.path);
        }
    }
    let files = patches.len();
    s += &format!(
        " {files} file{} changed, {added} insertion{}(+), {removed} deletion{}(-)\n",
        if files == 1 { "" } else { "s" },
        if added == 1 { "" } else { "s" },
        if removed == 1 { "" } else { "s" },
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::TreeEntry;

    fn tree(store: &ObjectStore, files: &[(&str, Mode, &[u8])]) -> Oid {
        let mut entries: Vec<TreeEntry> = files
            .iter()
            .map(|(name, mode, data)| TreeEntry {
                name: name.to_string(),
                mode: *mode,
                oid: store.write_blob(data).unwrap(),
            })
            .collect();
        store.write_tree(&mut entries).unwrap()
    }

    #[test]
    fn patches_modified_added_deleted_and_binary_files() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(tmp.path().join("objects"));
        let lines: String = (1..=10).map(|i| format!("line {i}\n")).collect();
        let changed = lines.replace("line 5\n", "line five\n");
        let a = tree(
            &store,
            &[
                ("bin.dat", Mode::File, b"\x00\x01\x02"),
                ("gone.txt", Mode::File, b"bye\n"),
                ("lib.rs", Mode::File, lines.as_bytes()),
                ("run.sh", Mode::File, b"echo hi\n"),
            ],
        );
        let b = tree(
            &store,
            &[
                ("bin.dat", Mode::File, b"\x00\x01\x03"),
                ("lib.rs", Mode::File, changed.as_bytes()),
                ("new.txt", Mode::File, b"hello"),
                ("run.sh", Mode::Executable, b"echo hi\n"),
            ],
        );
        let p = diff_trees(&store, &a, &b, DEFAULT_CONTEXT).unwrap();
        let by = |path: &str| p.iter().find(|f| f.path == path).unwrap();

        let bin = by("bin.dat");
        assert!(bin.binary && bin.hunks.is_empty());

        let gone = by("gone.txt");
        assert_eq!(
            (gone.kind, gone.added, gone.removed),
            (ChangeKind::Removed, 0, 1)
        );
        assert_eq!(gone.hunks[0].header(), "@@ -1 +0,0 @@");

        let lib = by("lib.rs");
        assert_eq!((lib.added, lib.removed), (1, 1));
        assert_eq!(lib.hunks.len(), 1);
        assert_eq!(lib.hunks[0].header(), "@@ -2,7 +2,7 @@");
        assert_eq!(
            lib.hunks[0].lines,
            [
                " line 2",
                " line 3",
                " line 4",
                "-line 5",
                "+line five",
                " line 6",
                " line 7",
                " line 8"
            ]
        );

        let new = by("new.txt");
        assert_eq!(new.kind, ChangeKind::Added);
        assert_eq!(new.hunks[0].header(), "@@ -0,0 +1 @@");
        assert_eq!(
            new.hunks[0].lines,
            ["+hello", "\\ No newline at end of file"]
        );

        let sh = by("run.sh");
        assert_eq!((sh.old_mode, sh.new_mode), (Some("100644"), Some("100755")));
        assert!(sh.hunks.is_empty());

        let text = render(&p);
        assert!(text.contains("Binary files a/bin.dat and b/bin.dat differ\n"));
        assert!(text.contains("deleted file mode 100644\n--- a/gone.txt\n+++ /dev/null\n"));
        assert!(text.contains("new file mode 100644\n--- /dev/null\n+++ b/new.txt\n"));
        assert!(text.contains("old mode 100644\nnew mode 100755\n"));
        assert!(!text.contains('\u{0}'));

        let stat = render_stat(&p);
        assert!(stat.contains("bin.dat  | Bin"), "{stat}");
        assert!(
            stat.ends_with("5 files changed, 2 insertions(+), 2 deletions(-)\n"),
            "{stat}"
        );
    }

    #[test]
    fn rendered_patch_applies_with_git() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(tmp.path().join("objects"));
        let old: String = (1..=30).map(|i| format!("{i}\n")).collect();
        let new = old
            .replace("3\n", "three\n")
            .replace("20\n", "")
            .replace("29\n", "29\nextra\n");
        let a = tree(&store, &[("f.txt", Mode::File, old.as_bytes())]);
        let b = tree(&store, &[("f.txt", Mode::File, new.as_bytes())]);
        let text = render(&diff_trees(&store, &a, &b, DEFAULT_CONTEXT).unwrap());
        let dir = tmp.path().join("w");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), &old).unwrap();
        std::fs::write(dir.join("p.diff"), &text).unwrap();
        let st = std::process::Command::new("git")
            .args(["apply", "p.diff"])
            .current_dir(&dir)
            .status()
            .unwrap();
        assert!(st.success(), "{text}");
        assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), new);
    }
}
