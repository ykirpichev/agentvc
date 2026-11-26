//! Deterministic conflict resolution (#91).
//!
//! `git merge-tree` decides which files conflict. Each conflicted text file
//! is then merged again here, line by line (diff3), and every conflict hunk
//! goes through the strategy configured for its path. Most conflicts between
//! parallel attempts have the same few shapes, and none of them needs an
//! agent:
//!
//! - both sides inserted at the same point (a new enum variant, `mod` line,
//!   CLI subcommand, test, README section): keep both;
//! - both sides made the same change, or one side only reformatted;
//! - non-overlapping edits inside one hunk (one side renamed a word, the
//!   other appended to the line): merged token by token;
//! - lockfiles (`Cargo.lock`) and JSON: merged by structure, not by line.
//!
//! The result is deterministic: the same three inputs always give the same
//! bytes. Hunks no strategy can resolve keep diff3-style markers, with the
//! base section, so a human or the review UI can finish them.
//!
//! Strategies per path, first match wins: `agentvc config merge.strategy
//! '<glob>=<strategy>' --add`, then the `agentvc-merge` attribute in
//! `.gitattributes` (git's `merge=union` counts as `union`, `-merge` or
//! `merge=binary` as `none`), then the built-in defaults: `Cargo.lock` is
//! `cargo-lock`, `*.json` is `json`, everything else `auto`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::process::{Command, Stdio};

use globset::{Glob, GlobMatcher};
use serde::Serialize;

use crate::config::{MERGE_FORMATTER, MERGE_STRATEGY};
use crate::error::{Error, Result};
use crate::space::Space;

/// How the conflict hunks of one file are resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Strategy {
    /// Same change, whitespace-only change, both-sides insertions and
    /// non-overlapping word edits; anything else stays a conflict.
    Auto,
    /// Every conflict hunk keeps both sides.
    Union,
    /// Conflict hunks take our side (the attempt being rebased, or the
    /// attempt receiving a take).
    Ours,
    /// Conflict hunks take their side.
    Theirs,
    /// Three-way merge of the parsed JSON, objects by key; falls back to `auto`.
    Json,
    /// Three-way merge of `[[package]]` entries; falls back to `auto`.
    CargoLock,
    /// Run the path's `merge.formatter` over all three sides, then `auto`.
    Format,
    /// Leave every conflict hunk as it is.
    None,
}

impl Strategy {
    pub const ALL: [Strategy; 8] = [
        Strategy::Auto,
        Strategy::Union,
        Strategy::Ours,
        Strategy::Theirs,
        Strategy::Json,
        Strategy::CargoLock,
        Strategy::Format,
        Strategy::None,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Strategy::Auto => "auto",
            Strategy::Union => "union",
            Strategy::Ours => "ours",
            Strategy::Theirs => "theirs",
            Strategy::Json => "json",
            Strategy::CargoLock => "cargo-lock",
            Strategy::Format => "format",
            Strategy::None => "none",
        }
    }

    pub fn parse(s: &str) -> Option<Strategy> {
        Strategy::ALL.into_iter().find(|k| k.name() == s.trim())
    }
}

impl std::fmt::Display for Strategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// A path pattern: without a `/` it matches the file name at any depth, as
/// in `.gitattributes`; with one, the whole path.
#[derive(Clone, Debug)]
struct Pattern {
    matcher: GlobMatcher,
    name_only: bool,
}

impl Pattern {
    fn new(glob: &str) -> Result<Pattern> {
        let glob = glob.trim().trim_start_matches('/');
        let matcher = Glob::new(glob)
            .map_err(|e| Error::BadMergeRule(format!("bad glob {glob:?}: {e}")))?
            .compile_matcher();
        Ok(Pattern {
            matcher,
            name_only: !glob.contains('/'),
        })
    }

    fn matches(&self, path: &str) -> bool {
        if self.name_only {
            let name = path.rsplit('/').next().unwrap_or(path);
            self.matcher.is_match(name)
        } else {
            self.matcher.is_match(path)
        }
    }
}

/// Which strategy and formatter each path gets.
#[derive(Clone, Debug, Default)]
pub struct Rules {
    /// False turns resolution off: git's conflicts are reported as they are.
    pub enabled: bool,
    strategies: Vec<(Pattern, Strategy)>,
    formatters: Vec<(Pattern, String)>,
    /// Strategies from `.gitattributes`, filled in per merge.
    attributes: BTreeMap<String, Strategy>,
}

/// Parse `<glob>=<value>`, splitting at the last `=` so globs may hold one.
fn split_rule(rule: &str) -> Result<(&str, &str)> {
    rule.rsplit_once('=')
        .filter(|(g, v)| !g.trim().is_empty() && !v.trim().is_empty())
        .ok_or_else(|| Error::BadMergeRule(format!("expected <glob>=<value>, got {rule:?}")))
}

impl Rules {
    /// Built-in defaults only.
    pub fn new() -> Rules {
        Rules {
            enabled: true,
            ..Default::default()
        }
    }

    /// No resolution at all.
    pub fn disabled() -> Rules {
        Rules::default()
    }

    /// The defaults plus the space's `merge.strategy` and `merge.formatter`.
    pub fn from_space(space: &Space) -> Result<Rules> {
        let mut rules = Rules::new();
        for r in space.config_values(MERGE_STRATEGY)? {
            rules.add_strategy(&r)?;
        }
        for r in space.config_values(MERGE_FORMATTER)? {
            rules.add_formatter(&r)?;
        }
        Ok(rules)
    }

    /// Add a `<glob>=<strategy>` rule after the existing ones.
    pub fn add_strategy(&mut self, rule: &str) -> Result<()> {
        let (glob, name) = split_rule(rule)?;
        let s = Strategy::parse(name).ok_or_else(|| {
            let names: Vec<_> = Strategy::ALL.iter().map(|s| s.name()).collect();
            Error::BadMergeRule(format!(
                "unknown merge strategy {name:?} (one of {})",
                names.join(", ")
            ))
        })?;
        self.strategies.push((Pattern::new(glob)?, s));
        Ok(())
    }

    /// Add a `<glob>=<command>` formatter; the command reads the file on
    /// stdin and writes it formatted to stdout.
    pub fn add_formatter(&mut self, rule: &str) -> Result<()> {
        let (glob, cmd) = rule
            .split_once('=')
            .filter(|(g, c)| !g.trim().is_empty() && !c.trim().is_empty())
            .ok_or_else(|| {
                Error::BadMergeRule(format!("expected <glob>=<command>, got {rule:?}"))
            })?;
        self.formatters
            .push((Pattern::new(glob)?, cmd.trim().to_string()));
        Ok(())
    }

    /// Read `agentvc-merge` and `merge` attributes for `paths` from the host
    /// repository's `.gitattributes`.
    pub(crate) fn load_attributes(&mut self, workdir: &std::path::Path, paths: &[String]) {
        if paths.is_empty() {
            return;
        }
        let child = Command::new("git")
            .arg("-C")
            .arg(workdir)
            .args(["check-attr", "-z", "--stdin", "agentvc-merge", "merge"])
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut child) = child else { return };
        if let Some(mut stdin) = child.stdin.take() {
            for p in paths {
                let _ = stdin.write_all(p.as_bytes());
                let _ = stdin.write_all(b"\0");
            }
        }
        let Ok(out) = child.wait_with_output() else {
            return;
        };
        let text = String::from_utf8_lossy(&out.stdout);
        let fields: Vec<&str> = text.split('\0').collect();
        // <path> NUL <attribute> NUL <info> NUL, `agentvc-merge` first.
        let mut own: BTreeMap<&str, Strategy> = BTreeMap::new();
        let mut git: BTreeMap<&str, Strategy> = BTreeMap::new();
        for f in fields.chunks(3) {
            let [path, attr, value] = f else { continue };
            match (*attr, *value) {
                ("agentvc-merge", v) => {
                    if let Some(s) = Strategy::parse(v) {
                        own.insert(path, s);
                    } else if v == "unset" {
                        own.insert(path, Strategy::None);
                    }
                }
                ("merge", "union") => {
                    git.insert(path, Strategy::Union);
                }
                ("merge", "unset" | "binary") => {
                    git.insert(path, Strategy::None);
                }
                _ => {}
            }
        }
        for (p, s) in git {
            self.attributes.entry(p.to_string()).or_insert(s);
        }
        for (p, s) in own {
            self.attributes.insert(p.to_string(), s);
        }
    }

    /// The strategy for `path`.
    pub fn strategy_for(&self, path: &str) -> Strategy {
        if let Some((_, s)) = self.strategies.iter().find(|(p, _)| p.matches(path)) {
            return *s;
        }
        if let Some(s) = self.attributes.get(path) {
            return *s;
        }
        let name = path.rsplit('/').next().unwrap_or(path);
        if name == "Cargo.lock" {
            Strategy::CargoLock
        } else if name.ends_with(".json") {
            Strategy::Json
        } else {
            Strategy::Auto
        }
    }

    /// The formatter command for `path`, if one is configured.
    pub fn formatter_for(&self, path: &str) -> Option<&str> {
        self.formatters
            .iter()
            .find(|(p, _)| p.matches(path))
            .map(|(_, c)| c.as_str())
    }
}

/// How one merge orders and labels the two sides.
#[derive(Clone, Debug)]
pub struct Sides {
    /// Label of our side in conflict markers.
    pub ours: String,
    /// Label of their side.
    pub theirs: String,
    /// When both sides inserted at the same point, put their lines first.
    /// Off by default: people merging main into a branch put the branch's
    /// lines first (see `agentvc conflicts replay`).
    pub theirs_first: bool,
}

impl Default for Sides {
    fn default() -> Sides {
        Sides {
            ours: "ours".into(),
            theirs: "theirs".into(),
            theirs_first: false,
        }
    }
}

/// The outcome of merging one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileMerge {
    pub text: String,
    /// Hunks that are still conflicts (with markers in `text`).
    pub conflicts: usize,
    /// Conflict hunks a strategy resolved.
    pub resolved: usize,
    /// The strategy that produced `text`, which may be the fallback.
    pub strategy: Strategy,
}

/// A file whose conflicts a strategy resolved, or tried to.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Resolution {
    pub path: String,
    pub strategy: Strategy,
    /// Conflict hunks resolved.
    pub hunks: usize,
    /// Conflict hunks left with markers; 0 means the file is resolved.
    pub remaining: usize,
}

/// Merge three versions of a text file with `strategy`.
pub fn merge_file(
    base: Option<&str>,
    ours: &str,
    theirs: &str,
    strategy: Strategy,
    formatter: Option<&str>,
    sides: &Sides,
) -> FileMerge {
    let base_text = base.unwrap_or("");
    let structural = match strategy {
        Strategy::Json => json::merge(base, ours, theirs, sides.theirs_first),
        Strategy::CargoLock => cargo_lock::merge(base_text, ours, theirs),
        _ => None,
    };
    if let Some(text) = structural {
        return FileMerge {
            text,
            conflicts: 0,
            resolved: 1,
            strategy,
        };
    }
    let hunk_strategy = match strategy {
        Strategy::Json | Strategy::CargoLock | Strategy::Format => Strategy::Auto,
        s => s,
    };
    let plain = merge_lines(base_text, ours, theirs, hunk_strategy, sides);
    let formatter = match (strategy, formatter) {
        (Strategy::Format | Strategy::Auto, Some(f)) if plain.conflicts > 0 => f,
        _ => {
            return FileMerge {
                strategy: if plain.resolved > 0 && strategy != Strategy::Format {
                    strategy
                } else {
                    hunk_strategy
                },
                ..plain
            }
        }
    };
    // Formatting all three sides removes format-only differences.
    let formatted = (|| {
        let b = format_with(formatter, base_text)?;
        let o = format_with(formatter, ours)?;
        let t = format_with(formatter, theirs)?;
        Some(merge_lines(&b, &o, &t, Strategy::Auto, sides))
    })();
    match formatted {
        Some(f) if f.conflicts < plain.conflicts => FileMerge {
            strategy: Strategy::Format,
            ..f
        },
        _ => plain,
    }
}

fn format_with(cmd: &str, text: &str) -> Option<String> {
    let mut child = Command::new("sh")
        .args(["-c", cmd])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let input = text.to_string();
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let out = child.wait_with_output().ok()?;
    let _ = writer.join();
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// A region of a three-way merge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Chunk<T> {
    /// Merged without conflict.
    Clean(Vec<T>),
    /// Both sides changed the same base region differently.
    Conflict {
        base: Vec<T>,
        ours: Vec<T>,
        theirs: Vec<T>,
    },
}

/// For each element of `base`, the index of the element of `other` it
/// matches in a minimal diff.
fn matches<T: std::hash::Hash + Eq + Ord>(base: &[T], other: &[T]) -> Vec<Option<usize>> {
    let mut m = vec![None; base.len()];
    for op in similar::capture_diff_slices(similar::Algorithm::Myers, base, other) {
        if let similar::DiffOp::Equal {
            old_index,
            new_index,
            len,
        } = op
        {
            for d in 0..len {
                m[old_index + d] = Some(new_index + d);
            }
        }
    }
    m
}

/// Classic diff3: regions where both sides match the base are stable; in
/// between, a region changed on one side takes that side, and one changed
/// differently on both is a conflict.
pub fn diff3<T: std::hash::Hash + Eq + Ord + Clone>(
    base: &[T],
    ours: &[T],
    theirs: &[T],
) -> Vec<Chunk<T>> {
    let mo = matches(base, ours);
    let mt = matches(base, theirs);
    let (mut i, mut j, mut k) = (0, 0, 0);
    let mut out: Vec<Chunk<T>> = Vec::new();
    let clean = |out: &mut Vec<Chunk<T>>, items: &[T]| {
        if items.is_empty() {
            return;
        }
        if let Some(Chunk::Clean(prev)) = out.last_mut() {
            prev.extend_from_slice(items);
        } else {
            out.push(Chunk::Clean(items.to_vec()));
        }
    };
    loop {
        let start = i;
        while i < base.len() && mo[i] == Some(j) && mt[i] == Some(k) {
            i += 1;
            j += 1;
            k += 1;
        }
        clean(&mut out, &base[start..i]);
        let mut i2 = i;
        while i2 < base.len() && (mo[i2].is_none() || mt[i2].is_none()) {
            i2 += 1;
        }
        let (j2, k2) = if i2 < base.len() {
            (mo[i2].unwrap_or(j), mt[i2].unwrap_or(k))
        } else {
            (ours.len(), theirs.len())
        };
        if i2 == i && j2 == j && k2 == k {
            break;
        }
        let (b, o, t) = (&base[i..i2], &ours[j..j2], &theirs[k..k2]);
        if o == b {
            clean(&mut out, t);
        } else if t == b || o == t {
            clean(&mut out, o);
        } else {
            out.push(Chunk::Conflict {
                base: b.to_vec(),
                ours: o.to_vec(),
                theirs: t.to_vec(),
            });
        }
        (i, j, k) = (i2, j2, k2);
    }
    out
}

fn lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

/// Concatenate blocks of lines, adding a newline where a block ends without one.
fn concat(blocks: &[&[&str]]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for b in blocks.iter().filter(|b| !b.is_empty()) {
        if let Some(last) = out.last_mut() {
            if !last.ends_with('\n') {
                last.push('\n');
            }
        }
        out.extend(b.iter().map(|l| l.to_string()));
    }
    out
}

fn squash(lines: &[&str]) -> String {
    lines
        .concat()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Resolve one conflict hunk the `auto` way.
fn auto_hunk(base: &[&str], ours: &[&str], theirs: &[&str], sides: &Sides) -> Option<Vec<String>> {
    let (sb, so, st) = (squash(base), squash(ours), squash(theirs));
    // The same change up to whitespace, or one side only reformatted.
    if so == st {
        return Some(concat(&[if sides.theirs_first { theirs } else { ours }]));
    }
    if so == sb {
        return Some(concat(&[theirs]));
    }
    if st == sb {
        return Some(concat(&[ours]));
    }
    // Both sides inserted at the same point: keep both, unless one
    // insertion already contains the other at its start or end.
    if base.iter().all(|l| l.trim().is_empty()) {
        let (first, second) = if sides.theirs_first {
            (theirs, ours)
        } else {
            (ours, theirs)
        };
        if first.len() >= second.len() && (first.starts_with(second) || first.ends_with(second)) {
            return Some(concat(&[first]));
        }
        if second.starts_with(first) || second.ends_with(first) {
            return Some(concat(&[second]));
        }
        return Some(concat(&[base, first, second]));
    }
    // Edits to different words of the same lines.
    word_merge(base, ours, theirs)
}

fn tokens(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut kind = 0u8;
    for (i, c) in text.char_indices() {
        let k = if c.is_alphanumeric() || c == '_' {
            1
        } else if c.is_whitespace() {
            2
        } else {
            3
        };
        if i > start && (k != kind || k == 3) {
            out.push(&text[start..i]);
            start = i;
        }
        kind = k;
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

fn word_merge(base: &[&str], ours: &[&str], theirs: &[&str]) -> Option<Vec<String>> {
    if base.len().max(ours.len()).max(theirs.len()) > 40 {
        return None;
    }
    let (b, o, t) = (base.concat(), ours.concat(), theirs.concat());
    let chunks = diff3(&tokens(&b), &tokens(&o), &tokens(&t));
    let mut text = String::new();
    for c in chunks {
        match c {
            Chunk::Clean(toks) => text.extend(toks),
            Chunk::Conflict { .. } => return None,
        }
    }
    Some(lines(&text).into_iter().map(str::to_string).collect())
}

/// Line-level three-way merge, resolving conflict hunks with `strategy`.
pub fn merge_lines(
    base: &str,
    ours: &str,
    theirs: &str,
    strategy: Strategy,
    sides: &Sides,
) -> FileMerge {
    let (b, o, t) = (lines(base), lines(ours), lines(theirs));
    let mut out: Vec<String> = Vec::new();
    let mut conflicts = 0;
    let mut resolved = 0;
    for chunk in diff3(&b, &o, &t) {
        match chunk {
            Chunk::Clean(ls) => out.extend(ls.iter().map(|l| l.to_string())),
            Chunk::Conflict { base, ours, theirs } => {
                let r = match strategy {
                    Strategy::Ours => Some(concat(&[&ours])),
                    Strategy::Theirs => Some(concat(&[&theirs])),
                    Strategy::Union => Some(if sides.theirs_first {
                        concat(&[&theirs, &ours])
                    } else {
                        concat(&[&ours, &theirs])
                    }),
                    Strategy::None => None,
                    _ => auto_hunk(&base, &ours, &theirs, sides),
                };
                match r {
                    Some(ls) => {
                        resolved += 1;
                        out.extend(ls);
                    }
                    None => {
                        conflicts += 1;
                        let marker = |m: String, block: &[&str], out: &mut Vec<String>| {
                            if let Some(last) = out.last_mut() {
                                if !last.ends_with('\n') {
                                    last.push('\n');
                                }
                            }
                            out.push(m);
                            out.extend(block.iter().map(|l| l.to_string()));
                        };
                        marker(format!("<<<<<<< {}\n", sides.ours), &ours, &mut out);
                        marker("||||||| base\n".into(), &base, &mut out);
                        marker("=======\n".into(), &theirs, &mut out);
                        marker(format!(">>>>>>> {}\n", sides.theirs), &[], &mut out);
                    }
                }
            }
        }
    }
    FileMerge {
        text: out.concat(),
        conflicts,
        resolved,
        strategy,
    }
}

/// Whether `text` still holds conflict markers.
pub fn has_markers(text: &str) -> bool {
    let mut open = false;
    for l in text.lines() {
        if l.starts_with("<<<<<<< ") || l == "<<<<<<<" {
            open = true;
        } else if open && (l.starts_with(">>>>>>> ") || l == ">>>>>>>") {
            return true;
        }
    }
    false
}

/// One file of a replayed merge.
#[derive(Clone, Debug, Serialize)]
pub struct ReplayFile {
    #[serde(flatten)]
    pub resolution: Resolution,
    /// Whether the resolved file is byte for byte what the merge commit
    /// recorded; `None` when it was not resolved.
    pub matches_commit: Option<bool>,
    /// The merged blob, with markers if hunks are left.
    pub blob: Option<String>,
}

/// A merge commit of the host's history that git could not merge cleanly.
#[derive(Clone, Debug, Serialize)]
pub struct ReplayMerge {
    pub commit: String,
    pub subject: String,
    pub files: Vec<ReplayFile>,
}

/// What resolution would have done to the conflicts in the host's history.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Replay {
    /// Merge commits looked at.
    pub merges: usize,
    /// Of those, the ones where plain git conflicts.
    pub conflicted: Vec<ReplayMerge>,
    /// Conflicted merges whose every file was resolved.
    pub merges_resolved: usize,
    pub files: usize,
    pub files_resolved: usize,
    /// Resolved files identical to what the merge commit recorded.
    pub files_matching: usize,
    pub hunks: usize,
    pub hunks_resolved: usize,
}

/// Re-run the merge commits of the host repository (newest first, at most
/// `limit`) with `rules` and compare with what was committed. The first
/// parent is "ours", as for a branch that merged main in.
pub fn replay(workdir: &std::path::Path, rules: &Rules, limit: usize) -> Result<Replay> {
    let repo = crate::export::HostRepo::open(workdir)?;
    let log = repo.git(
        &[
            "log",
            "--merges",
            "--branches",
            "--remotes",
            &format!("-n{limit}"),
            "--format=%H %P%x09%s",
        ],
        None,
    )?;
    let sides = Sides {
        ours: "ours".into(),
        theirs: "theirs".into(),
        theirs_first: false,
    };
    let mut r = Replay::default();
    for line in log.lines() {
        let Some((ids, subject)) = line.split_once('\t') else {
            continue;
        };
        let ids: Vec<&str> = ids.split(' ').collect();
        let [commit, p1, p2] = ids.as_slice() else {
            continue;
        };
        r.merges += 1;
        let Ok(base) = repo.git(&["merge-base", p1, p2], None) else {
            continue;
        };
        let m = crate::rebase::merge_commits(&repo, Some(&base), p1, p2, rules, &sides)?;
        if m.files.is_empty() && m.conflicts.is_empty() {
            continue;
        }
        let mut files = Vec::new();
        for f in m.files {
            let matches_commit = (f.remaining == 0).then(|| {
                let committed = repo
                    .git(
                        &[
                            "rev-parse",
                            "--verify",
                            "--quiet",
                            &format!("{commit}:{}", f.path),
                        ],
                        None,
                    )
                    .ok();
                committed.as_deref() == m.blobs.get(&f.path).map(String::as_str)
            });
            r.files += 1;
            r.hunks += f.hunks + f.remaining;
            r.hunks_resolved += f.hunks;
            if f.remaining == 0 {
                r.files_resolved += 1;
            }
            if matches_commit == Some(true) {
                r.files_matching += 1;
            }
            let blob = m.blobs.get(&f.path).cloned();
            files.push(ReplayFile {
                resolution: f,
                matches_commit,
                blob,
            });
        }
        if !rules.enabled {
            for p in m.conflicts {
                r.files += 1;
                files.push(ReplayFile {
                    resolution: Resolution {
                        path: p,
                        strategy: Strategy::None,
                        hunks: 0,
                        remaining: 1,
                    },
                    matches_commit: None,
                    blob: None,
                });
            }
        }
        if files.iter().all(|f| f.resolution.remaining == 0) {
            r.merges_resolved += 1;
        }
        r.conflicted.push(ReplayMerge {
            commit: commit.to_string(),
            subject: subject.to_string(),
            files,
        });
    }
    Ok(r)
}

mod json {
    //! A small order-preserving JSON reader and pretty printer. Scalars and
    //! keys keep their source text, so escapes and number formats survive.

    use super::{diff3, Chunk};

    #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
    pub(super) enum Value {
        Scalar(String),
        Array(Vec<Value>),
        Object(Vec<(String, Value)>),
    }

    struct Parser<'a> {
        s: &'a [u8],
        i: usize,
    }

    impl Parser<'_> {
        fn ws(&mut self) {
            while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
                self.i += 1;
            }
        }

        fn string(&mut self) -> Option<String> {
            let start = self.i;
            self.i += 1;
            while self.i < self.s.len() {
                match self.s[self.i] {
                    b'\\' => self.i += 2,
                    b'"' => {
                        self.i += 1;
                        return std::str::from_utf8(&self.s[start..self.i])
                            .ok()
                            .map(str::to_string);
                    }
                    _ => self.i += 1,
                }
            }
            None
        }

        fn value(&mut self) -> Option<Value> {
            self.ws();
            match *self.s.get(self.i)? {
                b'{' => {
                    self.i += 1;
                    let mut fields = Vec::new();
                    self.ws();
                    if self.s.get(self.i) == Some(&b'}') {
                        self.i += 1;
                        return Some(Value::Object(fields));
                    }
                    loop {
                        self.ws();
                        if self.s.get(self.i) != Some(&b'"') {
                            return None;
                        }
                        let key = self.string()?;
                        self.ws();
                        if self.s.get(self.i) != Some(&b':') {
                            return None;
                        }
                        self.i += 1;
                        let v = self.value()?;
                        if fields.iter().any(|(k, _)| *k == key) {
                            return None;
                        }
                        fields.push((key, v));
                        self.ws();
                        match self.s.get(self.i)? {
                            b',' => self.i += 1,
                            b'}' => {
                                self.i += 1;
                                return Some(Value::Object(fields));
                            }
                            _ => return None,
                        }
                    }
                }
                b'[' => {
                    self.i += 1;
                    let mut items = Vec::new();
                    self.ws();
                    if self.s.get(self.i) == Some(&b']') {
                        self.i += 1;
                        return Some(Value::Array(items));
                    }
                    loop {
                        items.push(self.value()?);
                        self.ws();
                        match self.s.get(self.i)? {
                            b',' => self.i += 1,
                            b']' => {
                                self.i += 1;
                                return Some(Value::Array(items));
                            }
                            _ => return None,
                        }
                    }
                }
                b'"' => self.string().map(Value::Scalar),
                _ => {
                    let start = self.i;
                    while self.i < self.s.len()
                        && !matches!(self.s[self.i], b',' | b']' | b'}')
                        && !self.s[self.i].is_ascii_whitespace()
                    {
                        self.i += 1;
                    }
                    let tok = std::str::from_utf8(&self.s[start..self.i]).ok()?;
                    let ok = matches!(tok, "true" | "false" | "null")
                        || (!tok.is_empty()
                            && tok
                                .bytes()
                                .all(|c| c.is_ascii_digit() || b"+-.eE".contains(&c)));
                    ok.then(|| Value::Scalar(tok.to_string()))
                }
            }
        }
    }

    pub(super) fn parse(text: &str) -> Option<Value> {
        let mut p = Parser {
            s: text.as_bytes(),
            i: 0,
        };
        let v = p.value()?;
        p.ws();
        (p.i == p.s.len()).then_some(v)
    }

    fn write(v: &Value, indent: &str, depth: usize, out: &mut String) {
        let pad = |d: usize| indent.repeat(d);
        match v {
            Value::Scalar(s) => out.push_str(s),
            Value::Array(items) if items.is_empty() => out.push_str("[]"),
            Value::Object(fields) if fields.is_empty() => out.push_str("{}"),
            Value::Array(items) => {
                out.push_str("[\n");
                for (n, item) in items.iter().enumerate() {
                    out.push_str(&pad(depth + 1));
                    write(item, indent, depth + 1, out);
                    out.push_str(if n + 1 < items.len() { ",\n" } else { "\n" });
                }
                out.push_str(&pad(depth));
                out.push(']');
            }
            Value::Object(fields) => {
                out.push_str("{\n");
                for (n, (k, item)) in fields.iter().enumerate() {
                    out.push_str(&pad(depth + 1));
                    out.push_str(k);
                    out.push_str(": ");
                    write(item, indent, depth + 1, out);
                    out.push_str(if n + 1 < fields.len() { ",\n" } else { "\n" });
                }
                out.push_str(&pad(depth));
                out.push('}');
            }
        }
    }

    fn merge3(
        base: Option<&Value>,
        ours: &Value,
        theirs: &Value,
        theirs_first: bool,
    ) -> Option<Value> {
        if ours == theirs || base == Some(theirs) {
            return Some(ours.clone());
        }
        if base == Some(ours) {
            return Some(theirs.clone());
        }
        match (base, ours, theirs) {
            (None | Some(Value::Object(_)), Value::Object(o), Value::Object(t)) => {
                let empty = Vec::new();
                let b = match base {
                    Some(Value::Object(b)) => b,
                    _ => &empty,
                };
                let get = |m: &[(String, Value)], k: &str| {
                    m.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone())
                };
                let (first, second) = if theirs_first { (t, o) } else { (o, t) };
                let mut keys: Vec<&String> = first.iter().map(|(k, _)| k).collect();
                for (k, _) in second {
                    if !keys.contains(&k) {
                        keys.push(k);
                    }
                }
                let mut out = Vec::new();
                for k in keys {
                    let (bv, ov, tv) = (get(b, k), get(o, k), get(t, k));
                    let v = if ov == tv {
                        ov
                    } else if bv == ov {
                        tv
                    } else if bv == tv {
                        ov
                    } else {
                        match (&ov, &tv) {
                            (Some(ov), Some(tv)) => {
                                Some(merge3(bv.as_ref(), ov, tv, theirs_first)?)
                            }
                            _ => return None,
                        }
                    };
                    if let Some(v) = v {
                        out.push((k.clone(), v));
                    }
                }
                Some(Value::Object(out))
            }
            (None | Some(Value::Array(_)), Value::Array(o), Value::Array(t)) => {
                let empty = Vec::new();
                let b = match base {
                    Some(Value::Array(b)) => b,
                    _ => &empty,
                };
                let mut out = Vec::new();
                for c in diff3(b, o, t) {
                    match c {
                        Chunk::Clean(items) => out.extend(items),
                        Chunk::Conflict { base, ours, theirs } if base.is_empty() => {
                            if theirs_first {
                                out.extend(theirs);
                                out.extend(ours);
                            } else {
                                out.extend(ours);
                                out.extend(theirs);
                            }
                        }
                        Chunk::Conflict { base, ours, theirs }
                            if base.len() == ours.len() && base.len() == theirs.len() =>
                        {
                            for ((b, o), t) in base.iter().zip(&ours).zip(&theirs) {
                                out.push(merge3(Some(b), o, t, theirs_first)?);
                            }
                        }
                        Chunk::Conflict { .. } => return None,
                    }
                }
                Some(Value::Array(out))
            }
            _ => None,
        }
    }

    /// The indent unit of a pretty-printed document; two spaces by default.
    fn indent_of(text: &str) -> String {
        text.lines()
            .skip(1)
            .find_map(|l| {
                let n = l.len() - l.trim_start().len();
                (n > 0).then(|| l[..n].to_string())
            })
            .unwrap_or_else(|| "  ".into())
    }

    pub(super) fn merge(
        base: Option<&str>,
        ours: &str,
        theirs: &str,
        theirs_first: bool,
    ) -> Option<String> {
        let b = match base {
            Some(b) if !b.trim().is_empty() => Some(parse(b)?),
            _ => None,
        };
        let o = parse(ours)?;
        let t = parse(theirs)?;
        let v = merge3(b.as_ref(), &o, &t, theirs_first)?;
        let mut out = String::new();
        write(&v, &indent_of(ours), 0, &mut out);
        if ours.ends_with('\n') {
            out.push('\n');
        }
        Some(out)
    }
}

mod cargo_lock {
    //! `Cargo.lock` as a set of packages keyed by name, version and source.

    use super::{BTreeMap, BTreeSet};

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Package {
        /// Every line except the dependency list.
        fields: Vec<String>,
        dependencies: Option<BTreeSet<String>>,
    }

    type Key = (String, Vec<u64>, String, String);

    fn version_key(v: &str) -> Vec<u64> {
        v.split(['.', '-', '+'])
            .map(|p| p.parse::<u64>().unwrap_or(u64::MAX))
            .collect()
    }

    fn parse(text: &str) -> Option<(String, BTreeMap<Key, Package>)> {
        let mut parts = text.split("\n[[package]]\n");
        let header = parts.next()?.to_string();
        if header.contains("[[package]]") || header.contains("\n[") {
            return None;
        }
        let mut packages = BTreeMap::new();
        for block in parts {
            let mut fields = Vec::new();
            let mut deps: Option<BTreeSet<String>> = None;
            let mut in_deps = false;
            let (mut name, mut version, mut source) = (None, None, String::new());
            for line in block.lines() {
                if in_deps {
                    if line.trim() == "]" {
                        in_deps = false;
                    } else {
                        deps.as_mut()?
                            .insert(line.trim().trim_end_matches(',').to_string());
                    }
                    continue;
                }
                if line.trim().is_empty() {
                    continue;
                }
                if line.starts_with('[') {
                    return None;
                }
                if line == "dependencies = [" {
                    in_deps = true;
                    deps = Some(BTreeSet::new());
                    continue;
                }
                let value = |l: &str| {
                    l.split_once(" = ")
                        .map(|(_, v)| v.trim_matches('"').to_string())
                };
                if line.starts_with("name = ") {
                    name = value(line);
                } else if line.starts_with("version = ") {
                    version = value(line);
                } else if line.starts_with("source = ") {
                    source = value(line).unwrap_or_default();
                }
                fields.push(line.to_string());
            }
            let (name, version) = (name?, version?);
            let key = (name, version_key(&version), version, source);
            let pkg = Package {
                fields,
                dependencies: deps,
            };
            if packages.insert(key, pkg).is_some() {
                return None;
            }
        }
        // Cargo writes a dependency as "name" while only one version of it
        // is locked, else "name version": qualify every entry so the sides
        // compare equal however many versions each one had.
        let versions: BTreeMap<String, Vec<String>> =
            packages.keys().fold(BTreeMap::new(), |mut m, k| {
                m.entry(k.0.clone()).or_default().push(k.2.clone());
                m
            });
        for pkg in packages.values_mut() {
            if let Some(deps) = pkg.dependencies.take() {
                let mut q = BTreeSet::new();
                for d in deps {
                    let raw = d.trim_matches('"');
                    let mut parts = raw.splitn(3, ' ');
                    let name = parts.next()?.to_string();
                    let version = match parts.next() {
                        Some(v) => v.to_string(),
                        None => match versions.get(&name).map(Vec::as_slice) {
                            Some([v]) => v.clone(),
                            _ => return None,
                        },
                    };
                    q.insert(match parts.next() {
                        Some(src) => format!("{name} {version} {src}"),
                        None => format!("{name} {version}"),
                    });
                }
                pkg.dependencies = Some(q);
            }
        }
        Some((header, packages))
    }

    /// Write a qualified dependency the way cargo does for this lockfile.
    fn dep_text(dep: &str, versions: &BTreeMap<&str, usize>) -> String {
        let mut parts = dep.splitn(3, ' ');
        let name = parts.next().unwrap_or_default();
        let version = parts.next().unwrap_or_default();
        match (versions.get(name).copied().unwrap_or(0), parts.next()) {
            (0 | 1, _) => format!("\"{name}\""),
            (_, Some(src)) => format!("\"{name} {version} {src}\""),
            _ => format!("\"{name} {version}\""),
        }
    }

    fn merge_package(b: Option<&Package>, o: &Package, t: &Package) -> Option<Package> {
        let fields = if o.fields == t.fields {
            o.fields.clone()
        } else if b.map(|b| &b.fields) == Some(&o.fields) {
            t.fields.clone()
        } else if b.map(|b| &b.fields) == Some(&t.fields) {
            o.fields.clone()
        } else {
            return None;
        };
        let none = BTreeSet::new();
        let bd = b.and_then(|b| b.dependencies.as_ref()).unwrap_or(&none);
        let od = o.dependencies.as_ref().unwrap_or(&none);
        let td = t.dependencies.as_ref().unwrap_or(&none);
        let deps: BTreeSet<String> = od
            .union(td)
            .filter(|d| !(bd.contains(*d) && (!od.contains(*d) || !td.contains(*d))))
            .cloned()
            .collect();
        Some(Package {
            fields,
            dependencies: (!deps.is_empty()).then_some(deps),
        })
    }

    pub(super) fn merge(base: &str, ours: &str, theirs: &str) -> Option<String> {
        let (hb, b) = if base.trim().is_empty() {
            (String::new(), BTreeMap::new())
        } else {
            parse(base)?
        };
        let (ho, o) = parse(ours)?;
        let (ht, t) = parse(theirs)?;
        let header = if ho == ht || hb == ht {
            ho
        } else if hb == ho {
            ht
        } else {
            return None;
        };
        let keys: BTreeSet<&Key> = o.keys().chain(t.keys()).collect();
        let mut merged = Vec::new();
        for k in keys {
            let (bp, op, tp) = (b.get(k), o.get(k), t.get(k));
            let pkg = if op == tp {
                op.cloned()
            } else if bp == op {
                tp.cloned()
            } else if bp == tp {
                op.cloned()
            } else {
                match (op, tp) {
                    (Some(op), Some(tp)) => Some(merge_package(bp, op, tp)?),
                    _ => return None,
                }
            };
            if let Some(pkg) = pkg {
                merged.push((k, pkg));
            }
        }
        let mut versions: BTreeMap<&str, usize> = BTreeMap::new();
        for (k, _) in &merged {
            *versions.entry(k.0.as_str()).or_default() += 1;
        }
        let mut out = header.trim_end().to_string();
        out.push('\n');
        for (_, pkg) in &merged {
            out.push_str("\n[[package]]\n");
            for f in &pkg.fields {
                out.push_str(f);
                out.push('\n');
            }
            if let Some(deps) = &pkg.dependencies {
                let mut deps: Vec<String> = deps.iter().map(|d| dep_text(d, &versions)).collect();
                deps.sort();
                out.push_str("dependencies = [\n");
                for d in deps {
                    out.push_str(&format!(" {d},\n"));
                }
                out.push_str("]\n");
            }
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rebase_sides() -> Sides {
        Sides {
            ours: "a2".into(),
            theirs: "main".into(),
            theirs_first: true,
        }
    }

    fn auto(b: &str, o: &str, t: &str) -> FileMerge {
        merge_file(Some(b), o, t, Strategy::Auto, None, &Sides::default())
    }

    #[test]
    fn both_sides_inserting_at_one_point_keeps_both() {
        let base = "enum Cmd {\n    Init,\n}\n";
        let ours = "enum Cmd {\n    Init,\n    Fork,\n}\n";
        let theirs = "enum Cmd {\n    Init,\n    Mcp,\n}\n";
        let m = auto(base, ours, theirs);
        assert_eq!(m.conflicts, 0);
        assert_eq!(m.resolved, 1);
        assert_eq!(m.text, "enum Cmd {\n    Init,\n    Fork,\n    Mcp,\n}\n");
        // A rebase puts what landed upstream first.
        let m = merge_file(
            Some(base),
            ours,
            theirs,
            Strategy::Auto,
            None,
            &rebase_sides(),
        );
        assert_eq!(m.text, "enum Cmd {\n    Init,\n    Mcp,\n    Fork,\n}\n");
        // The same insertion on both sides is kept once.
        let m = auto(base, ours, ours);
        assert_eq!(m.text, ours);
    }

    #[test]
    fn whitespace_only_and_word_level_edits_resolve() {
        let base = "let x = f(a, b);\n";
        // Ours only reformatted; theirs made a real change.
        let m = auto(base, "let x = f(a,  b);\n", "let x = g(a, b);\n");
        assert_eq!((m.conflicts, m.text.as_str()), (0, "let x = g(a, b);\n"));
        // Different words of the same line.
        let base = "        Cmd::Init | Cmd::Hook { .. } => unreachable!(),\n";
        let ours = "        Cmd::Init { .. } | Cmd::Hook { .. } => unreachable!(),\n";
        let theirs = "        Cmd::Init | Cmd::Hook { .. } | Cmd::Mcp { .. } => unreachable!(),\n";
        let m = auto(base, ours, theirs);
        assert_eq!(m.conflicts, 0);
        assert_eq!(
            m.text,
            "        Cmd::Init { .. } | Cmd::Hook { .. } | Cmd::Mcp { .. } => unreachable!(),\n"
        );
    }

    #[test]
    fn real_conflicts_keep_diff3_markers() {
        let m = auto("let n = 1;\n", "let n = 2;\n", "let n = 3;\n");
        assert_eq!(m.conflicts, 1);
        assert_eq!(
            m.text,
            "<<<<<<< ours\nlet n = 2;\n||||||| base\nlet n = 1;\n=======\nlet n = 3;\n>>>>>>> theirs\n"
        );
        assert!(has_markers(&m.text));
        let none = merge_file(
            Some("a\n"),
            "a\nb\n",
            "a\nc\n",
            Strategy::None,
            None,
            &Sides::default(),
        );
        assert_eq!(none.conflicts, 1);
    }

    #[test]
    fn union_ours_theirs() {
        let s = Sides::default();
        let u = merge_file(Some("x\n"), "y\n", "z\n", Strategy::Union, None, &s);
        assert_eq!(u.text, "y\nz\n");
        let o = merge_file(Some("x\n"), "y\n", "z\n", Strategy::Ours, None, &s);
        assert_eq!(o.text, "y\n");
        let t = merge_file(Some("x\n"), "y\n", "z\n", Strategy::Theirs, None, &s);
        assert_eq!(t.text, "z\n");
    }

    #[test]
    fn json_merges_objects_by_key_and_array_insertions() {
        let base = "{\n  \"name\": \"x\",\n  \"plugins\": []\n}\n";
        let ours = "{\n  \"name\": \"x\",\n  \"version\": \"1\",\n  \"plugins\": [\n    { \"name\": \"panel\" }\n  ]\n}\n";
        let theirs = "{\n  \"name\": \"x\",\n  \"plugins\": [\n    { \"name\": \"core\" }\n  ],\n  \"owner\": \"y\"\n}\n";
        let m = merge_file(
            Some(base),
            ours,
            theirs,
            Strategy::Json,
            None,
            &rebase_sides(),
        );
        assert_eq!(m.conflicts, 0);
        assert_eq!(m.strategy, Strategy::Json);
        assert_eq!(
            m.text,
            "{\n  \"name\": \"x\",\n  \"plugins\": [\n    {\n      \"name\": \"core\"\n    },\n    {\n      \"name\": \"panel\"\n    }\n  ],\n  \"owner\": \"y\",\n  \"version\": \"1\"\n}\n"
        );
        // The same key changed differently is not guessed.
        let m = merge_file(
            Some("{\"a\": 1}\n"),
            "{\"a\": 2}\n",
            "{\"a\": 3}\n",
            Strategy::Json,
            None,
            &Sides::default(),
        );
        assert_eq!(m.conflicts, 1);
    }

    #[test]
    fn cargo_lock_merges_packages_and_dependency_lists() {
        let head = "# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 4\n";
        let pkg = |name: &str, version: &str, deps: &[&str]| {
            let mut s = format!("\n[[package]]\nname = \"{name}\"\nversion = \"{version}\"\n");
            if !deps.is_empty() {
                s += "dependencies = [\n";
                for d in deps {
                    s += &format!(" \"{d}\",\n");
                }
                s += "]\n";
            }
            s
        };
        let base = format!(
            "{head}{}{}",
            pkg("app", "0.1.0", &["serde"]),
            pkg("serde", "1.0.0", &[])
        );
        let ours = format!(
            "{head}{}{}{}",
            pkg("app", "0.1.0", &["axum", "serde"]),
            pkg("axum", "0.8.9", &[]),
            pkg("serde", "1.0.0", &[])
        );
        let theirs = format!(
            "{head}{}{}{}",
            pkg("app", "0.1.0", &["base64", "serde"]),
            pkg("base64", "0.23.1", &[]),
            pkg("serde", "1.0.0", &[])
        );
        let m = merge_file(
            Some(&base),
            &ours,
            &theirs,
            Strategy::CargoLock,
            None,
            &Sides::default(),
        );
        assert_eq!(m.conflicts, 0);
        assert_eq!(
            m.text,
            format!(
                "{head}{}{}{}{}",
                pkg("app", "0.1.0", &["axum", "base64", "serde"]),
                pkg("axum", "0.8.9", &[]),
                pkg("base64", "0.23.1", &[]),
                pkg("serde", "1.0.0", &[])
            )
        );
    }

    #[test]
    fn formatter_removes_format_only_conflicts() {
        // `tr -s ' '` stands in for a formatter: it collapses runs of spaces.
        let s = Sides::default();
        let base = "a  b\nkeep\n";
        let ours = "a b\nkeep\nx\n";
        let theirs = "a   b\nkeep\n";
        let m = merge_file(
            Some(base),
            ours,
            theirs,
            Strategy::Format,
            Some("tr -s ' '"),
            &s,
        );
        assert_eq!(m.conflicts, 0);
    }

    #[test]
    fn rules_follow_config_then_defaults() {
        let mut r = Rules::new();
        assert_eq!(r.strategy_for("Cargo.lock"), Strategy::CargoLock);
        assert_eq!(r.strategy_for("ui/package.json"), Strategy::Json);
        assert_eq!(r.strategy_for("src/lib.rs"), Strategy::Auto);
        r.add_strategy("CHANGELOG.md=union").unwrap();
        r.add_strategy("docs/**=ours").unwrap();
        r.add_formatter("*.rs=rustfmt --edition 2021").unwrap();
        assert_eq!(r.strategy_for("CHANGELOG.md"), Strategy::Union);
        assert_eq!(r.strategy_for("docs/a/b.md"), Strategy::Ours);
        assert_eq!(
            r.formatter_for("crates/x/src/lib.rs"),
            Some("rustfmt --edition 2021")
        );
        assert_eq!(
            r.add_strategy("*.md=magic").unwrap_err().code(),
            "bad_merge_rule"
        );
        assert!(r.add_strategy("nonsense").is_err());
    }

    #[test]
    fn gitattributes_choose_strategies_below_config() {
        let tmp = crate::testutil::tempdir();
        let w = tmp.path();
        let ok = Command::new("git")
            .arg("-C")
            .arg(w)
            .args(["init", "-q"])
            .status()
            .unwrap();
        assert!(ok.success());
        std::fs::write(
            w.join(".gitattributes"),
            "CHANGES.md merge=union\n*.lock agentvc-merge=none\nnotes.txt agentvc-merge=theirs\n",
        )
        .unwrap();
        let mut r = Rules::new();
        r.add_strategy("notes.txt=ours").unwrap();
        let paths: Vec<String> = ["CHANGES.md", "x/y.lock", "notes.txt", "a.rs"]
            .map(String::from)
            .to_vec();
        r.load_attributes(w, &paths);
        assert_eq!(r.strategy_for("CHANGES.md"), Strategy::Union);
        assert_eq!(r.strategy_for("x/y.lock"), Strategy::None);
        assert_eq!(r.strategy_for("notes.txt"), Strategy::Ours);
        assert_eq!(r.strategy_for("a.rs"), Strategy::Auto);
    }

    #[test]
    fn diff3_basics() {
        // Edits to different lines merge; the same line edited twice conflicts.
        let c = diff3(
            &["a", "b", "c", "d"],
            &["a", "B", "c", "d"],
            &["a", "b", "c", "D"],
        );
        assert_eq!(c, vec![Chunk::Clean(vec!["a", "B", "c", "D"])]);
        let c = diff3(&["a", "b"], &["a", "x"], &["a", "y"]);
        assert_eq!(
            c,
            vec![
                Chunk::Clean(vec!["a"]),
                Chunk::Conflict {
                    base: vec!["b"],
                    ours: vec!["x"],
                    theirs: vec!["y"],
                },
            ]
        );
    }
}
