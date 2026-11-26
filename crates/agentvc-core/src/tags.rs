//! Named snapshots (`agentvc tag`, #74): checkpoints such as "known good
//! before the refactor" that humans and agents can come back to. A name
//! works wherever a ref does (`fork --from`, `diff`, `show`, the review UI),
//! pins its snapshot against `agentvc gc`, and travels with `sync` and the
//! PR comment. Release tags stay in git.

use rusqlite::{params, OptionalExtension};
use serde::Serialize;

use crate::error::{Error, Result};
use crate::space::{now, Ref, Space};

#[derive(Clone, Debug, Serialize)]
pub struct Tag {
    pub name: String,
    pub snapshot_id: i64,
    pub attempt_id: i64,
    pub note: Option<String>,
    pub actor: String,
    pub created_at: i64,
}

/// Why `name` cannot be a tag, if it cannot.
fn invalid(name: &str) -> Option<&'static str> {
    if name.is_empty() || name.len() > 64 {
        return Some("names are 1 to 64 characters");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
    {
        return Some("names use letters, digits, '.', '_', '-' and '/'");
    }
    if !name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
    {
        return Some("names start with a letter or digit");
    }
    if name.contains("..") || name.ends_with('/') || name.ends_with('.') {
        return Some("names cannot contain '..' or end with '/' or '.'");
    }
    // Things that already mean something as refs: aN, sN, tN, N.
    let rest = name.trim_start_matches(['a', 's', 't']);
    if rest.len() + 1 >= name.len() && !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
    {
        return Some("aN, sN, tN and plain numbers are ids, not names");
    }
    None
}

const COLS: &str = "t.name, t.snapshot_id, s.attempt_id, t.note, t.actor, t.created_at";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Tag> {
    Ok(Tag {
        name: r.get(0)?,
        snapshot_id: r.get(1)?,
        attempt_id: r.get(2)?,
        note: r.get(3)?,
        actor: r.get(4)?,
        created_at: r.get(5)?,
    })
}

impl Space {
    /// Name a snapshot. `aN` names the attempt's current state (an open
    /// attempt's view is snapshotted first).
    pub fn create_tag(
        &self,
        name: &str,
        target: Ref,
        note: Option<&str>,
        actor: &str,
    ) -> Result<Tag> {
        if let Some(why) = invalid(name) {
            return Err(Error::BadTagName {
                name: name.to_string(),
                reason: why,
            });
        }
        if self.tag(name)?.is_some() {
            return Err(Error::TagExists(name.to_string()));
        }
        let snap = match target {
            Ref::Attempt(id) => {
                let a = self.attempt(id)?;
                if a.status.is_open() && self.ensure_view(&a).unwrap_or(false) {
                    self.snapshot(id, "tag", None)?.snapshot
                } else {
                    self.head_snapshot(id)?
                }
            }
            Ref::Snapshot(id) => self.snapshot_by_id(id)?,
        };
        // A pruned tree cannot be named back to life.
        snap.tree_oid()?;
        self.db.execute(
            "INSERT INTO tags (name, snapshot_id, note, actor, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![name, snap.id, note, actor, now()],
        )?;
        self.record(
            actor,
            "tag.create",
            serde_json::json!({ "tag": name, "snapshot": snap.id }),
        )?;
        Ok(self.tag(name)?.expect("just inserted"))
    }

    pub fn tag(&self, name: &str) -> Result<Option<Tag>> {
        Ok(self
            .db
            .query_row(
                &format!("SELECT {COLS} FROM tags t JOIN snapshots s ON s.id = t.snapshot_id WHERE t.name = ?1"),
                params![name],
                row,
            )
            .optional()?)
    }

    pub fn tags(&self) -> Result<Vec<Tag>> {
        let mut stmt = self.db.prepare(&format!(
            "SELECT {COLS} FROM tags t JOIN snapshots s ON s.id = t.snapshot_id ORDER BY t.name"
        ))?;
        let rows = stmt.query_map([], row)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn delete_tag(&self, name: &str, actor: &str) -> Result<Tag> {
        let tag = self.tag(name)?.ok_or_else(|| Error::NotFound {
            kind: "tag",
            id: name.to_string(),
        })?;
        self.db
            .execute("DELETE FROM tags WHERE name = ?1", params![name])?;
        self.record(
            actor,
            "tag.delete",
            serde_json::json!({ "tag": name, "snapshot": tag.snapshot_id }),
        )?;
        Ok(tag)
    }

    /// Parse a ref: `aN`, `sN`, a bare number (an attempt), or a tag name
    /// (its snapshot).
    pub fn parse_ref(&self, s: &str) -> Result<Ref> {
        if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) {
            return s
                .parse::<i64>()
                .map(Ref::Attempt)
                .map_err(|_| Error::BadRef(s.into()));
        }
        match s.parse::<Ref>() {
            Ok(r) => Ok(r),
            Err(e) => match self.tag(s)? {
                Some(t) => Ok(Ref::Snapshot(t.snapshot_id)),
                None if invalid(s).is_none() => Err(Error::NotFound {
                    kind: "tag",
                    id: s.to_string(),
                }),
                None => Err(e),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::space::ForkRequest;
    use std::fs;

    fn space() -> (crate::testutil::TestDir, Space) {
        let tmp = crate::testutil::tempdir();
        fs::write(tmp.path().join("main.rs"), "1\n").unwrap();
        let space = Space::init(tmp.path()).unwrap();
        (tmp, space)
    }

    #[test]
    fn names_validate() {
        for bad in ["", "a1", "s22", "t3", "12", "-x", "a b", "x..y", "x/", "é"] {
            assert!(invalid(bad).is_some(), "{bad:?} should be rejected");
        }
        for good in [
            "v1",
            "known-good",
            "demo/2026-11",
            "a",
            "s",
            "alpha2",
            "before.refactor",
        ] {
            assert!(invalid(good).is_none(), "{good:?} should be fine");
        }
    }

    #[test]
    fn tags_resolve_as_refs_and_are_unique() {
        let (_tmp, space) = space();
        let a = space
            .fork(&ForkRequest {
                actor: "t".into(),
                backend: crate::view::BackendChoice::Copy,
                ..Default::default()
            })
            .unwrap()
            .attempt;
        fs::write(a.view_path.join("main.rs"), "2\n").unwrap();
        let t = space
            .create_tag("good", Ref::Attempt(a.id), Some("before refactor"), "human")
            .unwrap();
        assert_eq!(t.attempt_id, a.id);
        // Tagging aN snapshotted the view first.
        let snap = space.snapshot_by_id(t.snapshot_id).unwrap();
        assert_eq!(snap.trigger, "tag");
        assert_eq!(snap.tags, ["good"]);
        assert_eq!(space.attempt(a.id).unwrap().tags, ["good"]);
        assert_eq!(
            space.parse_ref("good").unwrap(),
            Ref::Snapshot(t.snapshot_id)
        );
        assert_eq!(space.parse_ref("a2").unwrap(), Ref::Attempt(2));
        assert_eq!(space.parse_ref("7").unwrap(), Ref::Attempt(7));
        assert_eq!(space.parse_ref("nope").unwrap_err().code(), "not_found");
        assert_eq!(
            space
                .diff(Ref::Attempt(1), space.parse_ref("good").unwrap())
                .unwrap()
                .len(),
            1
        );

        let e = space
            .create_tag("good", Ref::Attempt(1), None, "human")
            .unwrap_err();
        assert_eq!(e.code(), "exists");
        let e = space
            .create_tag("s1", Ref::Attempt(1), None, "human")
            .unwrap_err();
        assert_eq!(e.code(), "bad_tag_name");
        assert_eq!(space.tags().unwrap().len(), 1);
        space.delete_tag("good", "human").unwrap();
        assert!(space.tags().unwrap().is_empty());
        assert_eq!(
            space.delete_tag("good", "human").unwrap_err().code(),
            "not_found"
        );
    }

    #[test]
    fn tags_pin_snapshots_against_gc() {
        let (_tmp, space) = space();
        let a = space
            .fork(&ForkRequest {
                actor: "t".into(),
                backend: crate::view::BackendChoice::Copy,
                ..Default::default()
            })
            .unwrap()
            .attempt;
        fs::write(a.view_path.join("main.rs"), "2\n").unwrap();
        let t = space
            .create_tag("keep", Ref::Attempt(a.id), None, "human")
            .unwrap();
        fs::write(a.view_path.join("main.rs"), "3\n").unwrap();
        space.snapshot(a.id, "manual", None).unwrap();
        space
            .set_status(a.id, crate::Status::Abandoned, Some("x"))
            .unwrap();
        space
            .db
            .execute(
                "UPDATE attempts SET closed_at = closed_at - 100 * 86400",
                [],
            )
            .unwrap();
        let r = crate::gc::gc(&space, &crate::gc::GcOptions::default()).unwrap();
        assert!(!r.pruned_snapshots.contains(&t.snapshot_id));
        assert!(space
            .snapshot_by_id(t.snapshot_id)
            .unwrap()
            .tree_oid()
            .is_ok());
    }
}
