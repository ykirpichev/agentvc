//! Per-space settings (`agentvc config`), stored in the space's metadata.
//!
//! Settings save repeating the same flags on every command, for example a
//! cloud thread that may only push to its own branch and must end every
//! commit with attribution trailers. Command-line flags win over
//! environment variables, which win over the stored value.

use serde::Serialize;

use crate::error::{Error, Result};
use crate::space::Space;

/// A setting `agentvc config` knows about.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Key {
    pub name: &'static str,
    /// Holds a list of values (`agentvc config KEY VALUE --add`).
    pub multi: bool,
    /// Environment variable that overrides the stored value.
    pub env: Option<&'static str>,
    pub about: &'static str,
}

pub const PR_BRANCH: &str = "pr.branch";
pub const PR_BASE: &str = "pr.base";
pub const PR_TRAILER: &str = "pr.trailer";
pub const PR_BODY_PREFIX: &str = "pr.body-prefix";
pub const GIT_CAPTURE_COMMITS: &str = crate::viewgit::CAPTURE_COMMITS;

pub const GC_AUTO: &str = "gc.auto";
pub const GC_KEEP_ABANDONED: &str = "gc.keepAbandonedDays";
pub const GC_KEEP_VIEWS: &str = "gc.keepViewsDays";

pub const KEYS: &[Key] = &[
    Key {
        name: PR_BRANCH,
        multi: false,
        env: Some("AGENTVC_PR_BRANCH"),
        about: "branch `agentvc pr` pushes to (default agentvc/aN)",
    },
    Key {
        name: PR_BASE,
        multi: false,
        env: Some("AGENTVC_PR_BASE"),
        about: "branch PRs merge into (default: the repo's default branch)",
    },
    Key {
        name: PR_TRAILER,
        multi: true,
        env: None,
        about: "`Key: value` trailer added to every exported commit",
    },
    Key {
        name: PR_BODY_PREFIX,
        multi: false,
        env: Some("AGENTVC_PR_BODY_PREFIX"),
        about: "file whose text goes above the generated PR body",
    },
    Key {
        name: GC_AUTO,
        multi: false,
        env: None,
        about: "loose objects before snapshots start `agentvc gc --auto` (default 4000, 0 turns it off)",
    },
    Key {
        name: GC_KEEP_ABANDONED,
        multi: false,
        env: None,
        about: "days `agentvc gc` keeps the trees of abandoned attempts (default 30)",
    },
    Key {
        name: GC_KEEP_VIEWS,
        multi: false,
        env: None,
        about: "days `agentvc gc` keeps the views of closed attempts (default 7)",
    },
    Key {
        name: GIT_CAPTURE_COMMITS,
        multi: false,
        env: None,
        about: "record `git commit` in a view as a snapshot (default true; false leaves host git config alone)",
    },
];

pub fn key(name: &str) -> Result<&'static Key> {
    KEYS.iter()
        .find(|k| k.name == name)
        .ok_or_else(|| Error::BadConfigKey(name.to_string()))
}

fn meta_key(name: &str) -> String {
    format!("config.{name}")
}

impl Space {
    /// Stored values of a setting (ignoring the environment).
    pub fn config_values(&self, name: &str) -> Result<Vec<String>> {
        key(name)?;
        Ok(match self.meta(&meta_key(name))? {
            Some(v) => serde_json::from_str(&v).unwrap_or_default(),
            None => Vec::new(),
        })
    }

    /// Replace the stored values of a setting; an empty list unsets it.
    pub fn set_config(&self, name: &str, values: &[String]) -> Result<()> {
        key(name)?;
        if values.is_empty() {
            self.db
                .execute("DELETE FROM meta WHERE key = ?1", [meta_key(name)])?;
            return Ok(());
        }
        self.set_meta(
            &meta_key(name),
            &serde_json::to_string(values).expect("strings"),
        )
    }

    /// The effective single value: the environment variable when set and
    /// not empty, else the stored value.
    pub fn config(&self, name: &str) -> Result<Option<String>> {
        let k = key(name)?;
        if let Some(var) = k.env {
            if let Ok(v) = std::env::var(var) {
                if !v.trim().is_empty() {
                    return Ok(Some(v));
                }
            }
        }
        Ok(self.config_values(name)?.pop())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips_and_rejects_unknown_keys() {
        let tmp = crate::testutil::tempdir();
        let space = Space::init(tmp.path()).unwrap();
        assert_eq!(space.config(PR_BRANCH).unwrap(), None);
        space.set_config(PR_BRANCH, &["claude/x".into()]).unwrap();
        assert_eq!(
            space.config(PR_BRANCH).unwrap().as_deref(),
            Some("claude/x")
        );
        let t = vec!["A: 1".to_string(), "B: 2".to_string()];
        space.set_config(PR_TRAILER, &t).unwrap();
        assert_eq!(space.config_values(PR_TRAILER).unwrap(), t);
        space.set_config(PR_BRANCH, &[]).unwrap();
        assert_eq!(space.config(PR_BRANCH).unwrap(), None);
        assert_eq!(space.config("nope").unwrap_err().code(), "bad_config_key");
    }
}
