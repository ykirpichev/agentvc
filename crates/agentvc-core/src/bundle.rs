//! Keeping a space inside its pull request, for machines that may push only
//! one branch (Claude Code cloud sessions, #64).
//!
//! `agentvc pr` (and `agentvc sync` once a PR exists) writes one collapsed
//! comment on the PR holding the whole space: the same rows `agentvc sync`
//! pushes (see [`crate::sync`]), plus, for every attempt, a git patch from
//! its parent attempt's latest snapshot (or, for attempts forked from the
//! root, from the commit it is based on) to its own, and the tail of each run
//! log. It is zlib-compressed JSON in base64, kept under GitHub's comment
//! limit by leaving out the largest patches first (those attempts keep only
//! their history). `agentvc init --from-pr` reads it back: every task,
//! attempt, status and reason comes back with its id, and each attempt's
//! latest tree is rebuilt by applying its patch with git and checked against
//! the recorded tree id, so `diff`, `show` and the review UI work for losers
//! too and open attempts get their views back.
//! Intermediate snapshots keep their metadata but not their trees.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::path::Path;

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::error::{Error, IoContext, Result};
use crate::export::HostRepo;
use crate::objects::Oid;
use crate::space::{Space, ROOT_ATTEMPT, SPACE_DIR};
use crate::sync::{self, CatFile};

/// First line of the PR comment that holds a space.
pub const MARKER: &str = "<!-- agentvc-space:v1 -->";
/// GitHub refuses comment bodies over 65,536 characters.
pub const MAX_BODY: usize = 64_000;
const BUNDLE_FORMAT: u32 = 1;
const LOG_TAIL: u64 = 8 * 1024;

/// A space encoded for a PR comment.
#[derive(Clone, Debug, Serialize)]
pub struct Bundle {
    #[serde(skip)]
    pub body: String,
    pub space_id: String,
    pub tasks: usize,
    pub attempts: usize,
    /// Attempts carried with a patch of their files.
    pub patches: usize,
    /// Attempts (`aN`) whose patch was left out to fit the comment: history only.
    pub omitted: Vec<String>,
    pub chars: usize,
}

/// What [`restore`] rebuilt.
#[derive(Clone, Debug, Serialize)]
pub struct BundleRestore {
    pub space_id: String,
    pub source: String,
    pub tasks: usize,
    pub attempts: usize,
    pub snapshots: usize,
    /// Attempts whose latest tree was rebuilt exactly.
    pub rebuilt: Vec<i64>,
    /// Attempts whose files could not all be rebuilt (omitted files or a
    /// base commit this repo doesn't have): history only.
    pub incomplete: Vec<i64>,
    /// Open attempts whose views were written out.
    pub views: Vec<i64>,
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn b64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut acc = 0u32;
    let mut bits = 0;
    for c in text.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\n' | b'\r' | b' ' => continue,
            _ => return None,
        };
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// The tree of a commit in the host repo, copied into the space's store.
fn base_tree(space: &Space, repo: &HostRepo, git: &mut CatFile, commit: &str) -> Option<Oid> {
    let tree = repo
        .git(
            &["rev-parse", "--verify", "-q", &format!("{commit}^{{tree}}")],
            None,
        )
        .ok()?;
    let oid = Oid::from_hex(&tree).ok()?;
    sync::import_tree(space, git, &oid).ok()?;
    Some(oid)
}

fn compress(v: &Value) -> Result<String> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::best());
    enc.write_all(v.to_string().as_bytes()).at("bundle")?;
    Ok(b64_encode(&enc.finish().at("bundle")?))
}

fn wrap(s: &str, width: usize) -> String {
    s.as_bytes()
        .chunks(width)
        .map(|c| std::str::from_utf8(c).expect("base64 is ascii"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn comment_body(payload: &str, space_id: &str, tasks: usize, attempts: usize, pr: u64) -> String {
    format!(
        "{MARKER}\n<details><summary>agentvc space <code>{space_id}</code>: {tasks} tasks, \
         {attempts} attempts, losers included. Restore with <code>agentvc init --from-pr \
         {pr}</code>.</summary>\n\nKept by <code>agentvc pr</code> / <code>agentvc sync</code>; \
         updated in place.\n\n```text\n{}\n```\n</details>\n",
        wrap(payload, 76)
    )
}

/// A patch from tree `from` to tree `to`, made by git in the host repo.
fn tree_patch(space: &Space, repo: &HostRepo, from: &Oid, to: &Oid) -> Result<String> {
    let objects = repo.objects_dir()?;
    for t in [from, to] {
        crate::export::copy_tree_objects(space, t, &objects)?;
    }
    repo.git_raw(&[
        "diff",
        "--binary",
        "--full-index",
        "--no-renames",
        "--no-color",
        "--no-ext-diff",
        &from.to_hex(),
        &to.to_hex(),
    ])
}

/// Encode the space for a comment on PR `pr`.
pub fn encode(space: &Space, pr: u64) -> Result<Bundle> {
    let id = sync::space_id(space)?;
    let mut doc = sync::dump(space, &id)?;
    let repo = HostRepo::open(space.workdir())?;
    let mut git = CatFile::start(space.workdir())?;

    // Per attempt, how to rebuild its latest snapshot: the commit of its
    // current PR export, which is on the remote already; else a git patch from
    // its parent attempt's latest snapshot (small for chains of follow-ups) or,
    // for attempts forked from the root, from the commit it is based on.
    let mut patches: BTreeMap<i64, Value> = BTreeMap::new();
    // Base-relative patches, for children whose parent gets left out.
    let mut rooted: BTreeMap<i64, Value> = BTreeMap::new();
    for a in space.attempts(None)? {
        if a.id == ROOT_ATTEMPT {
            continue;
        }
        let head = space.head_snapshot(a.id)?;
        // `agentvc gc` pruned this loser's tree: only its metadata travels.
        if head.pruned_at.is_some() {
            continue;
        }
        let pushed = space.exports(a.id)?.into_iter().rev().find(|e| {
            e.snapshot_id == head.id
                && e.pr_url.is_some()
                && space
                    .last_export_to(&e.branch)
                    .ok()
                    .flatten()
                    .is_some_and(|l| l.id == e.id)
        });
        if let Some(e) = pushed {
            patches.insert(
                a.id,
                json!({ "commit": e.commit, "tree": head.tree, "snapshot": head.id }),
            );
            continue;
        }
        let parent = match a.parent_attempt {
            Some(p) if p != ROOT_ATTEMPT && patches.contains_key(&p) => Some(p),
            _ => None,
        };
        let base = space.git_base_commit(&head)?;
        let from_tree = match (parent, &base) {
            (Some(p), _) => Oid::from_hex(&space.head_snapshot(p)?.tree)?,
            (None, Some(b)) => match base_tree(space, &repo, &mut git, b) {
                Some(t) => t,
                None => continue,
            },
            (None, None) => continue,
        };
        let to = Oid::from_hex(&head.tree)?;
        let patch = tree_patch(space, &repo, &from_tree, &to)?;
        patches.insert(
            a.id,
            json!({
                "from_attempt": parent, "base": base, "tree": head.tree,
                "snapshot": head.id, "patch": patch,
            }),
        );
        if parent.is_some() {
            if let Some(bt) = base
                .as_ref()
                .and_then(|b| base_tree(space, &repo, &mut git, b))
            {
                rooted.insert(
                    a.id,
                    json!({
                        "from_attempt": null, "base": base, "tree": head.tree,
                        "snapshot": head.id, "patch": tree_patch(space, &repo, &bt, &to)?,
                    }),
                );
            }
        }
    }

    let mut logs = Map::new();
    for r in doc["tables"]["runs"]
        .as_array()
        .cloned()
        .unwrap_or_default()
    {
        if let (Some(rid), Some(p)) = (r["id"].as_i64(), r["log_path"].as_str()) {
            if let Some(data) = sync::tail(Path::new(p), LOG_TAIL) {
                logs.insert(format!("r{rid}"), json!(b64_encode(&data)));
            }
        }
    }

    let count = |d: &Value, t: &str| d["tables"][t].as_array().map_or(0, Vec::len);
    let (tasks, attempts) = (count(&doc, "tasks"), count(&doc, "attempts"));
    // When the comment is too long, the largest patches go first; those
    // attempts keep only their history.
    let mut dropped: BTreeSet<i64> = BTreeSet::new();
    let mut drop_logs = false;
    let mut drop_ops = false;
    loop {
        let kept: Map<String, Value> = patches
            .iter()
            .filter(|(k, _)| !dropped.contains(*k))
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        let payload = json!({
            "format": BUNDLE_FORMAT,
            "space": doc,
            "patches": kept,
            "logs": if drop_logs { json!({}) } else { Value::Object(logs.clone()) },
        });
        let text = compress(&payload)?;
        let body = comment_body(&text, &id, tasks, attempts, pr);
        if body.len() <= MAX_BODY {
            return Ok(Bundle {
                chars: body.len(),
                body,
                space_id: id,
                tasks,
                attempts,
                patches: patches.len() - dropped.len(),
                omitted: dropped.iter().map(|a| format!("a{a}")).collect(),
            });
        }
        let size = |p: &Value| p["patch"].as_str().map_or(0, str::len);
        let largest = patches
            .iter()
            .filter(|(k, p)| !dropped.contains(*k) && size(p) > 0)
            .max_by_key(|(_, p)| size(p))
            .map(|(k, _)| *k);
        if let Some(k) = largest {
            dropped.insert(k);
            // Children patched against a dropped attempt switch to their
            // base-relative patch, or go too.
            loop {
                let orphans: Vec<i64> = patches
                    .iter()
                    .filter(|(c, p)| {
                        !dropped.contains(*c)
                            && p["from_attempt"]
                                .as_i64()
                                .is_some_and(|f| dropped.contains(&f))
                    })
                    .map(|(c, _)| *c)
                    .collect();
                if orphans.is_empty() {
                    break;
                }
                for c in orphans {
                    match rooted.remove(&c) {
                        Some(r) => {
                            patches.insert(c, r);
                        }
                        None => {
                            dropped.insert(c);
                        }
                    }
                }
            }
        } else if !drop_logs {
            drop_logs = true;
        } else if !drop_ops {
            drop_ops = true;
            doc["tables"]["operations"] = json!([]);
        } else {
            return Err(Error::BundleTooLarge(body.len()));
        }
    }
}

/// The payload of a space comment, from its body.
pub fn decode(body: &str) -> Result<Value> {
    let bad = |why: &str| Error::BadObject(format!("agentvc space comment: {why}"));
    if !body.contains(MARKER) {
        return Err(bad("no agentvc-space marker"));
    }
    let start = body.find("```text\n").ok_or_else(|| bad("no payload"))? + "```text\n".len();
    let end = body[start..]
        .find("```")
        .ok_or_else(|| bad("unterminated payload"))?
        + start;
    let raw = b64_decode(&body[start..end]).ok_or_else(|| bad("not base64"))?;
    let mut text = String::new();
    ZlibDecoder::new(raw.as_slice())
        .read_to_string(&mut text)
        .map_err(|_| bad("not zlib"))?;
    let v: Value = serde_json::from_str(&text).map_err(|_| bad("not json"))?;
    if v["format"].as_u64() != Some(BUNDLE_FORMAT as u64) {
        return Err(bad("unknown format"));
    }
    Ok(v)
}

/// Rebuild a space in `workdir` from a decoded space comment. `source` names
/// where it came from, for the operation log.
pub fn restore(workdir: &Path, payload: &Value, source: &str) -> Result<(Space, BundleRestore)> {
    let workdir = fs::canonicalize(workdir).at(workdir)?;
    let dir = workdir.join(SPACE_DIR);
    if dir.join("meta.db").exists() {
        return Err(Error::SpaceExists(workdir));
    }
    let repo = HostRepo::open(&workdir)?;
    let r = restore_into(&workdir, &repo, payload, source);
    if r.is_err() {
        let _ = fs::remove_dir_all(&dir);
    }
    r
}

/// Apply `patch` (from [`tree_patch`]) to tree `from` with git in a scratch
/// index, and copy the resulting tree into the space's store.
fn apply_patch(
    space: &Space,
    repo: &HostRepo,
    git: &mut CatFile,
    from: &Oid,
    patch: Option<&str>,
) -> Result<Oid> {
    let patch = patch.unwrap_or_default();
    if patch.trim().is_empty() {
        return Ok(*from);
    }
    crate::export::copy_tree_objects(space, from, &repo.objects_dir()?)?;
    let index = space
        .workdir()
        .join(SPACE_DIR)
        .join(format!("bundle-index-{}", std::process::id()));
    let idx = index.to_string_lossy().into_owned();
    let env = [("GIT_INDEX_FILE", idx.as_str())];
    let result = (|| {
        repo.git_env(&["read-tree", &from.to_hex()], None, &env)?;
        repo.git_env(&["apply", "--cached", "--binary", "-"], Some(patch), &env)?;
        let tree = repo.git_env(&["write-tree"], None, &env)?;
        let oid = Oid::from_hex(&tree)?;
        sync::import_tree(space, git, &oid)?;
        Ok(oid)
    })();
    let _ = fs::remove_file(&index);
    result
}

fn restore_into(
    workdir: &Path,
    repo: &HostRepo,
    payload: &Value,
    source: &str,
) -> Result<(Space, BundleRestore)> {
    let doc = &payload["space"];
    let space = sync::empty_space(workdir)?;
    sync::insert_rows(&space, doc)?;
    let space_id = doc["space_id"].as_str().unwrap_or_default().to_string();
    if !space_id.is_empty() {
        space.set_meta(sync::SPACE_ID_KEY, &space_id)?;
    }
    let mut git = CatFile::start(workdir)?;

    let (mut rebuilt, mut incomplete) = (Vec::new(), Vec::new());
    // The commit the space started from: usually the root's first snapshot,
    // and what `diff s1 aN` compares against.
    if let Some(head) = space.meta(crate::export::IMPORT_BASE_KEY)? {
        let _ = base_tree(&space, repo, &mut git, &head);
    }

    // Parents before children: a child's files are relative to its parent.
    let mut patches: Vec<(i64, &Value)> = payload["patches"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| Some((k.parse().ok()?, v)))
                .collect()
        })
        .unwrap_or_default();
    patches.sort_by_key(|(id, _)| *id);
    let mut trees: BTreeMap<i64, Oid> = BTreeMap::new();
    {
        for (aid, p) in patches {
            if let Some(c) = p["commit"].as_str() {
                // Exported to the PR branch: the commit is on the remote.
                if repo.rev(c).is_none() {
                    let _ = repo.git(&["fetch", "--quiet", "--no-tags", "origin", c], None);
                }
                match base_tree(&space, repo, &mut git, c) {
                    Some(t) if Some(t.to_hex().as_str()) == p["tree"].as_str() => {
                        trees.insert(aid, t);
                        rebuilt.push(aid);
                    }
                    _ => incomplete.push(aid),
                }
                continue;
            }
            let from = match p["from_attempt"].as_i64() {
                Some(parent) => trees.get(&parent).copied(),
                None => {
                    let base = p["base"].as_str().unwrap_or_default();
                    // The base is usually on the remote's main branch already.
                    if repo.rev(base).is_none() {
                        let _ = repo.git(&["fetch", "--quiet", "--no-tags", "origin", base], None);
                    }
                    base_tree(&space, repo, &mut git, base)
                }
            };
            let Some(from) = from else {
                incomplete.push(aid);
                continue;
            };
            let tree = match apply_patch(&space, repo, &mut git, &from, p["patch"].as_str()) {
                Ok(t) => t,
                Err(_) => {
                    incomplete.push(aid);
                    continue;
                }
            };
            if Some(tree.to_hex().as_str()) == p["tree"].as_str() {
                trees.insert(aid, tree);
                rebuilt.push(aid);
            } else {
                incomplete.push(aid);
            }
        }
    }

    // Attempts whose patch was left out keep only their history.
    for a in space.attempts(None)? {
        if a.id != ROOT_ATTEMPT && !rebuilt.contains(&a.id) && !incomplete.contains(&a.id) {
            incomplete.push(a.id);
        }
    }
    incomplete.sort_unstable();

    let runs_dir = workdir.join(SPACE_DIR).join("runs");
    if let Some(logs) = payload["logs"].as_object() {
        fs::create_dir_all(&runs_dir).at(&runs_dir)?;
        for (name, data) in logs {
            if let Some(bytes) = data.as_str().and_then(b64_decode) {
                let path = runs_dir.join(format!("{name}.log"));
                fs::write(&path, bytes).at(&path)?;
            }
        }
    }

    let views = sync::open_views(&space)?;
    space.record(
        "human",
        "space.clone",
        json!({ "source": source, "rebuilt": rebuilt, "incomplete": incomplete }),
    )?;
    let count = |t: &str| doc["tables"][t].as_array().map_or(0, Vec::len);
    let result = BundleRestore {
        space_id,
        source: source.to_string(),
        tasks: count("tasks"),
        attempts: count("attempts"),
        snapshots: count("snapshots"),
        rebuilt,
        incomplete,
        views,
    };
    Ok((space, result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::space::{ForkRequest, Ref, Status};
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn base64_round_trips() {
        for n in 0..10 {
            let data: Vec<u8> = (0..n).map(|i| (i * 37) as u8).collect();
            assert_eq!(b64_decode(&b64_encode(&data)).unwrap(), data);
        }
        assert_eq!(b64_encode(b"Man"), "TWFu");
        assert_eq!(b64_encode(b"Ma"), "TWE=");
    }

    fn setup() -> (
        crate::testutil::TestDir,
        std::path::PathBuf,
        std::path::PathBuf,
    ) {
        let tmp = crate::testutil::tempdir();
        let a = tmp.path().join("a");
        fs::create_dir_all(a.join("src")).unwrap();
        git(&a, &["init", "-q", "-b", "main"]);
        fs::write(a.join("src/lib.rs"), "fn f() {}\n").unwrap();
        fs::write(a.join("README"), "hi\n").unwrap();
        git(&a, &["add", "-A"]);
        git(&a, &["commit", "-q", "-m", "base"]);
        let b = tmp.path().join("b");
        git(
            tmp.path(),
            &["clone", "-q", a.to_str().unwrap(), b.to_str().unwrap()],
        );
        (tmp, a, b)
    }

    #[test]
    fn pr_comment_restores_losers_with_their_files() {
        let (_tmp, a, b) = setup();
        let space = Space::init(&a).unwrap();
        let task = space.create_task("goal", None).unwrap();
        let fork = |actor: &str| {
            space
                .fork(&ForkRequest {
                    task_id: Some(task.id),
                    actor: actor.into(),
                    backend: crate::view::BackendChoice::Copy,
                    ..Default::default()
                })
                .unwrap()
                .attempt
        };
        let win = fork("agent:a");
        let lose = fork("agent:b");
        fs::write(win.view_path.join("src/lib.rs"), "fn f() { 1; }\n").unwrap();
        fs::create_dir_all(win.view_path.join("src/new")).unwrap();
        fs::write(win.view_path.join("src/new/x.rs"), "x\n").unwrap();
        space.snapshot(win.id, "manual", None).unwrap();
        fs::remove_file(lose.view_path.join("README")).unwrap();
        space.snapshot(lose.id, "manual", None).unwrap();
        space
            .set_status(lose.id, Status::Abandoned, Some("too slow"))
            .unwrap();
        // A follow-up of the winner carries only its own change.
        let next = space
            .fork(&ForkRequest {
                task_id: Some(task.id),
                from: Some(Ref::Attempt(win.id)),
                backend: crate::view::BackendChoice::Copy,
                ..Default::default()
            })
            .unwrap()
            .attempt;
        fs::write(next.view_path.join("README"), "hi again\n").unwrap();
        space.snapshot(next.id, "manual", None).unwrap();

        let bundle = encode(&space, 7).unwrap();
        assert!(bundle.body.starts_with(MARKER));
        assert!(bundle.body.contains("agentvc init --from-pr 7"));
        assert!(bundle.omitted.is_empty());
        assert_eq!(bundle.patches, 3);

        let payload = decode(&bundle.body).unwrap();
        let p = &payload["patches"][next.id.to_string()];
        assert_eq!(p["from_attempt"], win.id);
        let patch = p["patch"].as_str().unwrap();
        assert!(
            patch.contains("README") && !patch.contains("x.rs"),
            "{patch}"
        );
        let (copy, r) = restore(&b, &payload, "pr#7").unwrap();
        assert_eq!(r.rebuilt, vec![win.id, lose.id, next.id]);
        assert!(r.incomplete.is_empty());
        assert_eq!(r.views, vec![win.id, next.id]);
        let view = copy.attempt(next.id).unwrap().view_path;
        assert_eq!(
            fs::read_to_string(view.join("src/new/x.rs")).unwrap(),
            "x\n"
        );
        let l = copy.attempt(lose.id).unwrap();
        assert_eq!(l.reason.as_deref(), Some("too slow"));
        let d = copy.diff(Ref::Snapshot(1), Ref::Attempt(lose.id)).unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].path, "README");
        let view = copy.attempt(win.id).unwrap().view_path;
        assert_eq!(
            fs::read_to_string(view.join("src/new/x.rs")).unwrap(),
            "x\n"
        );
        assert_eq!(
            restore(&b, &payload, "pr#7").err().unwrap().code(),
            "space_exists"
        );
    }

    #[test]
    fn big_files_are_omitted_to_fit() {
        let (_tmp, a, b) = setup();
        let space = Space::init(&a).unwrap();
        let at = space
            .fork(&ForkRequest {
                backend: crate::view::BackendChoice::Copy,
                ..Default::default()
            })
            .unwrap()
            .attempt;
        // Incompressible, far over the comment limit.
        let mut x: u64 = 1;
        let noise: Vec<u8> = (0..200_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        fs::write(at.view_path.join("big.bin"), noise).unwrap();
        fs::write(at.view_path.join("small.txt"), "ok\n").unwrap();
        space.snapshot(at.id, "manual", None).unwrap();
        let bundle = encode(&space, 1).unwrap();
        assert!(bundle.chars <= MAX_BODY);
        assert_eq!(bundle.omitted, vec![format!("a{}", at.id)]);
        // A follow-up that drops the big file is re-based on the commit
        // when its parent is left out, so it still comes back.
        let child = space
            .fork(&ForkRequest {
                from: Some(Ref::Attempt(at.id)),
                backend: crate::view::BackendChoice::Copy,
                ..Default::default()
            })
            .unwrap()
            .attempt;
        fs::remove_file(child.view_path.join("big.bin")).unwrap();
        fs::write(child.view_path.join("small.txt"), "ok 2\n").unwrap();
        space.snapshot(child.id, "manual", None).unwrap();
        let bundle = encode(&space, 1).unwrap();
        assert_eq!(bundle.omitted, vec![format!("a{}", at.id)]);
        let (copy, r) = restore(&b, &decode(&bundle.body).unwrap(), "pr#1").unwrap();
        assert_eq!(r.incomplete, vec![at.id]);
        assert_eq!(r.rebuilt, vec![child.id]);
        assert_eq!(r.views, vec![child.id]);
        let view = copy.attempt(child.id).unwrap().view_path;
        assert_eq!(
            fs::read_to_string(view.join("small.txt")).unwrap(),
            "ok 2\n"
        );
    }
}
