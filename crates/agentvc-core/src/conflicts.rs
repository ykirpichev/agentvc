//! Conflicts as data (#93) and resolving them hunk by hunk (#92).
//!
//! When `rebase`, `take` or `merge` leaves conflicts (`--keep-conflicts`, or
//! the review UI), each file is recorded in table `conflicts` with the blob
//! ids of its three sides, and the file in the view gets diff3 markers. The
//! attempt can then be finished later, by anyone:
//!
//! - [`Space::conflict_file`] parses the markers into hunks (ours, base,
//!   theirs), which the UI shows side by side;
//! - [`Space::resolve_conflict`] replaces chosen hunks with ours, theirs,
//!   both or custom text, and records a `resolve` snapshot once no file of
//!   the attempt has markers left;
//! - a file whose markers are gone (edited by hand or by an agent) counts as
//!   resolved the next time conflicts are listed.
//!
//! `export` and `pr` refuse with code `unresolved_conflicts` while any are
//! open, so markers never ship.

use std::fs;

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::error::{Error, IoContext, Result};
use crate::export::HostRepo;
use crate::objects::Oid;
use crate::rebase::{CatFile, Merge};
use crate::resolve::Sides;
use crate::space::{now, Snapshot, Space};

/// A file left conflicted by an operation.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Conflict {
    pub id: i64,
    pub attempt_id: i64,
    /// The snapshot the operation recorded (with the markers).
    pub snapshot_id: i64,
    /// `rebase`, `take` or `merge`.
    pub operation: String,
    pub path: String,
    /// Blob ids of the three sides; `base` is `None` for a file both added.
    pub base: Option<String>,
    pub ours: Option<String>,
    pub theirs: Option<String>,
    pub ours_label: String,
    pub theirs_label: String,
    pub created_at: i64,
    pub resolved_at: Option<i64>,
    /// How: `ours`, `theirs`, `both`, `custom`, `mixed` or `edited`.
    pub resolution: Option<String>,
}

const COLS: &str = "id, attempt_id, snapshot_id, operation, path, base, ours, theirs, \
                    ours_label, theirs_label, created_at, resolved_at, resolution";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Conflict> {
    Ok(Conflict {
        id: r.get(0)?,
        attempt_id: r.get(1)?,
        snapshot_id: r.get(2)?,
        operation: r.get(3)?,
        path: r.get(4)?,
        base: r.get(5)?,
        ours: r.get(6)?,
        theirs: r.get(7)?,
        ours_label: r.get(8)?,
        theirs_label: r.get(9)?,
        created_at: r.get(10)?,
        resolved_at: r.get(11)?,
        resolution: r.get(12)?,
    })
}

/// One conflict hunk of a file, as its markers show it.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Hunk {
    /// Numbered from 1 in file order.
    pub index: usize,
    /// 1-based line of the `<<<<<<<` marker.
    pub line: usize,
    pub ours: String,
    /// `None` for two-way markers without a `|||||||` section.
    pub base: Option<String>,
    pub theirs: String,
    pub ours_label: String,
    pub theirs_label: String,
}

/// A conflicted file of an attempt, split into hunks.
#[derive(Clone, Debug, Serialize)]
pub struct ConflictFile {
    pub path: String,
    pub conflict: Option<Conflict>,
    pub hunks: Vec<Hunk>,
    /// The whole file as it is in the view, markers included.
    pub text: String,
}

/// What to put in place of a hunk.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Take {
    Ours,
    Theirs,
    /// Ours, then theirs.
    Both,
    /// Theirs, then ours.
    BothTheirsFirst,
    Base,
    /// The text given with the choice.
    Custom,
}

impl Take {
    pub fn parse(s: &str) -> Option<Take> {
        match s {
            "ours" | "mine" => Some(Take::Ours),
            "theirs" | "main" => Some(Take::Theirs),
            "both" => Some(Take::Both),
            "both-theirs-first" => Some(Take::BothTheirsFirst),
            "base" => Some(Take::Base),
            "custom" => Some(Take::Custom),
            _ => None,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Take::Ours => "ours",
            Take::Theirs => "theirs",
            Take::Both | Take::BothTheirsFirst => "both",
            Take::Base => "base",
            Take::Custom => "custom",
        }
    }
}

/// A choice for one hunk (`index` from [`Hunk::index`]).
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HunkChoice {
    pub index: usize,
    pub take: Take,
    /// For `custom`: the text that replaces the hunk.
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ResolveResult {
    pub attempt: i64,
    pub path: String,
    /// Hunks replaced by this call.
    pub resolved_hunks: usize,
    /// Hunks of this file still with markers.
    pub remaining_hunks: usize,
    /// Conflicted files of the attempt still open, this one included.
    pub open: Vec<String>,
    /// Recorded when the last open file was resolved.
    pub snapshot: Option<Snapshot>,
}

/// Split `text` into hunks; `None` if a marker block is not closed.
pub fn parse_hunks(text: &str) -> Option<Vec<Hunk>> {
    let mut hunks = Vec::new();
    let mut lines = text.split_inclusive('\n').enumerate().peekable();
    while let Some((n, l)) = lines.next() {
        let Some(label) = open_label(l) else {
            continue;
        };
        let (mut ours, mut base, mut theirs) = (String::new(), None::<String>, String::new());
        let mut part = 0; // 0 ours, 1 base, 2 theirs
        let mut closed = None;
        for (_, l) in lines.by_ref() {
            let bare = l.trim_end_matches(['\n', '\r']);
            if part < 2 && (bare == "|||||||" || bare.starts_with("||||||| ")) && part == 0 {
                part = 1;
                base = Some(String::new());
            } else if part < 2 && bare == "=======" {
                part = 2;
            } else if part == 2 && (bare == ">>>>>>>" || bare.starts_with(">>>>>>> ")) {
                closed = Some(bare.trim_start_matches('>').trim().to_string());
                break;
            } else {
                match part {
                    0 => ours.push_str(l),
                    1 => base.get_or_insert_with(String::new).push_str(l),
                    _ => theirs.push_str(l),
                }
            }
        }
        let theirs_label = closed?;
        hunks.push(Hunk {
            index: hunks.len() + 1,
            line: n + 1,
            ours,
            base,
            theirs,
            ours_label: label,
            theirs_label,
        });
    }
    Some(hunks)
}

fn open_label(line: &str) -> Option<String> {
    let bare = line.trim_end_matches(['\n', '\r']);
    if bare == "<<<<<<<" {
        Some(String::new())
    } else {
        bare.strip_prefix("<<<<<<< ").map(|l| l.trim().to_string())
    }
}

fn join_blocks(blocks: &[&str]) -> String {
    let mut out = String::new();
    for b in blocks.iter().filter(|b| !b.is_empty()) {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(b);
    }
    out
}

/// Replace the chosen hunks of `text`; returns the new text and how many
/// hunks were replaced.
pub fn apply_choices(text: &str, choices: &[HunkChoice]) -> Result<(String, usize)> {
    let hunks = parse_hunks(text)
        .ok_or_else(|| Error::BadMergeRule("conflict markers are not closed".into()))?;
    for c in choices {
        if c.index == 0 || c.index > hunks.len() {
            return Err(Error::BadHunk {
                spec: c.index.to_string(),
                reason: format!("the file has {} conflict hunks", hunks.len()),
            });
        }
        if c.take == Take::Custom && c.text.is_none() {
            return Err(Error::BadHunk {
                spec: c.index.to_string(),
                reason: "custom needs text".into(),
            });
        }
    }
    let mut out = String::new();
    let mut index = 0;
    let mut replaced = 0;
    let mut lines = text.split_inclusive('\n');
    while let Some(l) = lines.next() {
        if open_label(l).is_none() {
            out.push_str(l);
            continue;
        }
        index += 1;
        let mut raw = l.to_string();
        // Copy the block, then decide whether to keep it.
        let mut in_theirs = false;
        for l in lines.by_ref() {
            raw.push_str(l);
            let bare = l.trim_end_matches(['\n', '\r']);
            if bare == "=======" {
                in_theirs = true;
            } else if in_theirs && (bare == ">>>>>>>" || bare.starts_with(">>>>>>> ")) {
                break;
            }
        }
        let h = &hunks[index - 1];
        match choices.iter().rev().find(|c| c.index == index) {
            None => out.push_str(&raw),
            Some(c) => {
                replaced += 1;
                let base = h.base.clone().unwrap_or_default();
                let text = match c.take {
                    Take::Ours => h.ours.clone(),
                    Take::Theirs => h.theirs.clone(),
                    Take::Both => join_blocks(&[&h.ours, &h.theirs]),
                    Take::BothTheirsFirst => join_blocks(&[&h.theirs, &h.ours]),
                    Take::Base => base,
                    Take::Custom => c.text.clone().unwrap_or_default(),
                };
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str(&text);
                // Keep the line break the marker block ended with.
                if !text.is_empty() && !text.ends_with('\n') && raw.ends_with('\n') {
                    out.push('\n');
                }
            }
        }
    }
    Ok((out, replaced))
}

impl Space {
    /// Record the files `merge` left conflicted by `operation` in snapshot
    /// `snapshot_id`, with their sides copied into the store.
    pub(crate) fn record_conflicts(
        &self,
        repo: &HostRepo,
        attempt_id: i64,
        snapshot_id: i64,
        operation: &str,
        merge: &Merge,
        sides: &Sides,
    ) -> Result<()> {
        if merge.conflicts.is_empty() {
            return Ok(());
        }
        let mut git = CatFile::start(repo)?;
        let mut import = |oid: &Option<String>| -> Result<Option<String>> {
            let Some(oid) = oid else { return Ok(None) };
            let (_, data) = git.read(&Oid::from_hex(oid)?)?;
            Ok(Some(self.objects().write_raw("blob", &data)?.to_hex()))
        };
        // A new operation supersedes what an earlier one left open.
        self.db.execute(
            "UPDATE conflicts SET resolved_at = ?1, resolution = 'superseded'
             WHERE attempt_id = ?2 AND resolved_at IS NULL",
            params![now(), attempt_id],
        )?;
        for path in &merge.conflicts {
            let st = merge.stages.get(path).cloned().unwrap_or_default();
            let (b, o, t) = (import(&st[0])?, import(&st[1])?, import(&st[2])?);
            self.db.execute(
                &format!(
                    "INSERT INTO conflicts ({}) VALUES (NULL, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL, NULL)",
                    COLS
                ),
                params![
                    attempt_id,
                    snapshot_id,
                    operation,
                    path,
                    b,
                    o,
                    t,
                    sides.ours,
                    sides.theirs,
                    now()
                ],
            )?;
        }
        Ok(())
    }

    fn view_text(&self, attempt_id: i64, path: &str) -> Result<Option<String>> {
        let attempt = self.attempt(attempt_id)?;
        if path.split('/').any(|p| p == ".." || p.is_empty()) {
            return Err(Error::NotFound {
                kind: "conflicted file",
                id: path.to_string(),
            });
        }
        let file = attempt.view_path.join(path);
        match fs::read(&file) {
            Ok(b) => Ok(Some(String::from_utf8_lossy(&b).into_owned())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).at(&file),
        }
    }

    /// Conflicts of an attempt, oldest first. Open ones whose file has no
    /// markers any more are marked resolved (`edited`) first.
    pub fn conflicts(&self, attempt_id: i64, include_resolved: bool) -> Result<Vec<Conflict>> {
        let open: Vec<Conflict> = self
            .db
            .prepare(&format!(
                "SELECT {COLS} FROM conflicts WHERE attempt_id = ?1 AND resolved_at IS NULL ORDER BY id"
            ))?
            .query_map([attempt_id], row)?
            .collect::<rusqlite::Result<_>>()?;
        let attempt_open = self
            .attempt(attempt_id)
            .map(|a| a.status.is_open())
            .unwrap_or(false);
        // What ships: the view of an open attempt, else its last snapshot.
        let head_files = if attempt_open || open.is_empty() {
            None
        } else {
            let head = self.head_snapshot(attempt_id)?;
            Some(crate::snapshot::flatten(self.objects(), &head.tree_oid()?)?)
        };
        for c in &open {
            let text = match &head_files {
                None => self.view_text(attempt_id, &c.path)?,
                Some(files) => match files.get(&c.path) {
                    Some((_, oid)) => {
                        Some(String::from_utf8_lossy(&self.objects().read_blob(oid)?).into_owned())
                    }
                    None => None,
                },
            };
            if !text.as_deref().is_some_and(crate::resolve::has_markers) {
                self.mark_resolved(c.id, "edited")?;
            }
        }
        let sql = if include_resolved {
            format!("SELECT {COLS} FROM conflicts WHERE attempt_id = ?1 ORDER BY id")
        } else {
            format!(
                "SELECT {COLS} FROM conflicts WHERE attempt_id = ?1 AND resolved_at IS NULL ORDER BY id"
            )
        };
        Ok(self
            .db
            .prepare(&sql)?
            .query_map([attempt_id], row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    fn mark_resolved(&self, id: i64, how: &str) -> Result<()> {
        self.db.execute(
            "UPDATE conflicts SET resolved_at = ?1, resolution = ?2 WHERE id = ?3",
            params![now(), how, id],
        )?;
        Ok(())
    }

    fn open_conflict(&self, attempt_id: i64, path: &str) -> Result<Option<Conflict>> {
        Ok(self
            .db
            .query_row(
                &format!(
                    "SELECT {COLS} FROM conflicts WHERE attempt_id = ?1 AND path = ?2
                     AND resolved_at IS NULL ORDER BY id DESC LIMIT 1"
                ),
                params![attempt_id, path],
                row,
            )
            .optional()?)
    }

    /// A conflicted file of an attempt, split into hunks.
    pub fn conflict_file(&self, attempt_id: i64, path: &str) -> Result<ConflictFile> {
        let text = self
            .view_text(attempt_id, path)?
            .ok_or_else(|| Error::NotFound {
                kind: "file",
                id: path.to_string(),
            })?;
        let hunks = parse_hunks(&text).unwrap_or_default();
        Ok(ConflictFile {
            path: path.to_string(),
            conflict: self.open_conflict(attempt_id, path)?,
            hunks,
            text,
        })
    }

    /// Resolve hunks of a conflicted file in the attempt's view. With no
    /// `choices`, `all` applies to every hunk. When no open conflict of the
    /// attempt is left, records a `resolve` snapshot.
    pub fn resolve_conflict(
        &self,
        attempt_id: i64,
        path: &str,
        choices: &[HunkChoice],
        all: Option<Take>,
        actor: &str,
    ) -> Result<ResolveResult> {
        let attempt = self.attempt(attempt_id)?;
        if !attempt.status.is_open() || !self.ensure_view(&attempt)? {
            return Err(Error::AttemptClosed {
                id: attempt_id,
                status: attempt.status.to_string(),
            });
        }
        let file = self.conflict_file(attempt_id, path)?;
        let mut choices = choices.to_vec();
        if choices.is_empty() {
            let take = all.ok_or_else(|| Error::BadHunk {
                spec: path.to_string(),
                reason: "choose hunks or a side for all of them".into(),
            })?;
            choices = file
                .hunks
                .iter()
                .map(|h| HunkChoice {
                    index: h.index,
                    take: take.clone(),
                    text: None,
                })
                .collect();
        }
        let (text, replaced) = apply_choices(&file.text, &choices)?;
        let dest = attempt.view_path.join(path);
        fs::write(&dest, &text).at(&dest)?;
        let remaining = parse_hunks(&text).map(|h| h.len()).unwrap_or(0);
        if remaining == 0 {
            if let Some(c) = &file.conflict {
                let kinds: std::collections::BTreeSet<&str> =
                    choices.iter().map(|c| c.take.name()).collect();
                let how = if kinds.len() == 1 {
                    kinds.into_iter().next().unwrap_or("mixed")
                } else {
                    "mixed"
                };
                self.mark_resolved(c.id, how)?;
            }
        }
        let open: Vec<String> = self
            .conflicts(attempt_id, false)?
            .into_iter()
            .map(|c| c.path)
            .collect();
        let snapshot = if open.is_empty() {
            let meta = serde_json::json!({ "resolved": path, "actor": actor });
            let s = self.snapshot(attempt_id, "resolve", Some(meta))?.snapshot;
            self.record(
                actor,
                "attempt.resolve",
                serde_json::json!({ "attempt": attempt_id, "path": path, "snapshot": s.id }),
            )?;
            Some(s)
        } else {
            self.record(
                actor,
                "attempt.resolve",
                serde_json::json!({ "attempt": attempt_id, "path": path }),
            )?;
            None
        };
        Ok(ResolveResult {
            attempt: attempt_id,
            path: path.to_string(),
            resolved_hunks: replaced,
            remaining_hunks: remaining,
            open,
            snapshot,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "a\n<<<<<<< a2\nmine\n||||||| base\nold\n=======\nmain\n>>>>>>> origin/main\nb\n<<<<<<< a2\nx\n=======\ny\n>>>>>>> origin/main\nc\n";

    #[test]
    fn parses_diff3_and_two_way_hunks() {
        let h = parse_hunks(TEXT).unwrap();
        assert_eq!(h.len(), 2);
        assert_eq!(
            (
                h[0].ours.as_str(),
                h[0].base.as_deref(),
                h[0].theirs.as_str()
            ),
            ("mine\n", Some("old\n"), "main\n")
        );
        assert_eq!((h[0].line, h[0].ours_label.as_str()), (2, "a2"));
        assert_eq!(h[0].theirs_label, "origin/main");
        assert_eq!((h[1].base.as_deref(), h[1].line), (None, 10));
        assert!(parse_hunks("<<<<<<< a\nx\n").is_none());
    }

    #[test]
    fn applies_choices_per_hunk() {
        let c = |index, take| HunkChoice {
            index,
            take,
            text: None,
        };
        let (t, n) = apply_choices(TEXT, &[c(1, Take::Both)]).unwrap();
        assert_eq!(n, 1);
        assert!(t.starts_with("a\nmine\nmain\nb\n<<<<<<< a2\n"));
        let (t, _) = apply_choices(TEXT, &[c(1, Take::Theirs), c(2, Take::Ours)]).unwrap();
        assert_eq!(t, "a\nmain\nb\nx\nc\n");
        let custom = HunkChoice {
            index: 2,
            take: Take::Custom,
            text: Some("x and y".into()),
        };
        let (t, _) = apply_choices(TEXT, &[c(1, Take::Base), custom]).unwrap();
        assert_eq!(t, "a\nold\nb\nx and y\nc\n");
        assert_eq!(
            apply_choices(TEXT, &[c(3, Take::Ours)]).unwrap_err().code(),
            "bad_hunk"
        );
    }
}
