//! The service's own SQLite database: people, their sign-in sessions, machine
//! tokens and the spaces those machines have connected. No code, diffs or
//! events are ever written here.
//!
//! Secrets (session cookies, machine tokens) are stored only as SHA-256
//! hashes, so a copy of the database cannot be used to sign in.

use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};

pub type Result<T> = rusqlite::Result<T>;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS users (
    id          INTEGER PRIMARY KEY,
    login       TEXT NOT NULL UNIQUE COLLATE NOCASE,
    github_id   INTEGER,
    created_at  INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
    hash        TEXT PRIMARY KEY,
    user_id     INTEGER NOT NULL REFERENCES users(id),
    created_at  INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS tokens (
    id           INTEGER PRIMARY KEY,
    user_id      INTEGER NOT NULL REFERENCES users(id),
    name         TEXT NOT NULL,
    hash         TEXT NOT NULL UNIQUE,
    created_at   INTEGER NOT NULL,
    last_used_at INTEGER,
    revoked_at   INTEGER
);
CREATE TABLE IF NOT EXISTS spaces (
    slug        TEXT PRIMARY KEY,
    user_id     INTEGER NOT NULL REFERENCES users(id),
    token_id    INTEGER NOT NULL REFERENCES tokens(id),
    space_id    TEXT NOT NULL,
    name        TEXT NOT NULL,
    workdir     TEXT NOT NULL,
    version     TEXT NOT NULL,
    read_only   INTEGER NOT NULL DEFAULT 0,
    first_seen  INTEGER NOT NULL,
    last_seen   INTEGER NOT NULL,
    UNIQUE (token_id, space_id)
);
"#;

/// Session lifetime: 30 days.
pub const SESSION_SECS: i64 = 30 * 24 * 3600;

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// `n` random bytes as lowercase hex.
pub fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).expect("the OS random number generator failed");
    hex::encode(buf)
}

/// A short random id for URLs, `[a-z0-9]{12}`.
pub fn random_slug() -> String {
    const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    let mut buf = [0u8; 12];
    getrandom::fill(&mut buf).expect("the OS random number generator failed");
    buf.iter()
        .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char)
        .collect()
}

#[derive(Clone, Debug, Serialize)]
pub struct User {
    pub id: i64,
    pub login: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Token {
    pub id: i64,
    pub name: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SpaceRow {
    pub slug: String,
    pub user_id: i64,
    pub token_id: i64,
    pub name: String,
    pub workdir: String,
    pub version: String,
    pub read_only: bool,
    pub last_seen: i64,
    /// Name of the token (machine) it connected with.
    pub machine: String,
}

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        let conn = Connection::open(path)?;
        Self::setup(conn)
    }

    pub fn memory() -> Result<Db> {
        Self::setup(Connection::open_in_memory()?)
    }

    fn setup(conn: Connection) -> Result<Db> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        Ok(Db { conn })
    }

    /// Find or create the person signing in.
    pub fn upsert_user(&self, login: &str, github_id: Option<i64>) -> Result<User> {
        self.conn.execute(
            "INSERT INTO users (login, github_id, created_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(login) DO UPDATE SET github_id = COALESCE(excluded.github_id, github_id)",
            params![login, github_id, now()],
        )?;
        self.conn.query_row(
            "SELECT id, login FROM users WHERE login = ?1",
            [login],
            |r| {
                Ok(User {
                    id: r.get(0)?,
                    login: r.get(1)?,
                })
            },
        )
    }

    /// A new session; returns the cookie value (only its hash is stored).
    pub fn create_session(&self, user_id: i64) -> Result<String> {
        let secret = random_hex(32);
        let t = now();
        self.conn.execute(
            "INSERT INTO sessions (hash, user_id, created_at, expires_at) VALUES (?1, ?2, ?3, ?4)",
            params![sha256_hex(&secret), user_id, t, t + SESSION_SECS],
        )?;
        // Sweep expired ones while here.
        self.conn
            .execute("DELETE FROM sessions WHERE expires_at < ?1", [t])?;
        Ok(secret)
    }

    pub fn session_user(&self, secret: &str) -> Result<Option<User>> {
        self.conn
            .query_row(
                "SELECT u.id, u.login FROM sessions s JOIN users u ON u.id = s.user_id
                 WHERE s.hash = ?1 AND s.expires_at > ?2",
                params![sha256_hex(secret), now()],
                |r| {
                    Ok(User {
                        id: r.get(0)?,
                        login: r.get(1)?,
                    })
                },
            )
            .optional()
    }

    pub fn delete_session(&self, secret: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM sessions WHERE hash = ?1", [sha256_hex(secret)])?;
        Ok(())
    }

    /// A new machine token; returns its row and the secret (shown once).
    pub fn create_token(&self, user_id: i64, name: &str) -> Result<(Token, String)> {
        let secret = format!("avc_{}", random_hex(20));
        let t = now();
        self.conn.execute(
            "INSERT INTO tokens (user_id, name, hash, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![user_id, name, sha256_hex(&secret), t],
        )?;
        let token = Token {
            id: self.conn.last_insert_rowid(),
            name: name.into(),
            created_at: t,
            last_used_at: None,
        };
        Ok((token, secret))
    }

    /// The live token with this secret, and its owner; marks it used.
    pub fn use_token(&self, secret: &str) -> Result<Option<(i64, User)>> {
        let hit = self
            .conn
            .query_row(
                "SELECT t.id, u.id, u.login FROM tokens t JOIN users u ON u.id = t.user_id
                 WHERE t.hash = ?1 AND t.revoked_at IS NULL",
                [sha256_hex(secret)],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        User {
                            id: r.get(1)?,
                            login: r.get(2)?,
                        },
                    ))
                },
            )
            .optional()?;
        if let Some((id, _)) = &hit {
            self.conn.execute(
                "UPDATE tokens SET last_used_at = ?1 WHERE id = ?2",
                params![now(), id],
            )?;
        }
        Ok(hit)
    }

    pub fn tokens(&self, user_id: i64) -> Result<Vec<Token>> {
        let mut st = self.conn.prepare(
            "SELECT id, name, created_at, last_used_at FROM tokens
             WHERE user_id = ?1 AND revoked_at IS NULL ORDER BY id",
        )?;
        let rows = st.query_map([user_id], |r| {
            Ok(Token {
                id: r.get(0)?,
                name: r.get(1)?,
                created_at: r.get(2)?,
                last_used_at: r.get(3)?,
            })
        })?;
        rows.collect()
    }

    /// Revoke a token and forget the spaces it connected. True if it was
    /// this user's live token.
    pub fn revoke_token(&self, user_id: i64, id: i64) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE tokens SET revoked_at = ?1 WHERE id = ?2 AND user_id = ?3 AND revoked_at IS NULL",
            params![now(), id, user_id],
        )?;
        if n > 0 {
            self.conn
                .execute("DELETE FROM spaces WHERE token_id = ?1", [id])?;
        }
        Ok(n > 0)
    }

    /// Record a daemon's hello; returns the space's slug, the same one each
    /// time this machine connects this space.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_space(
        &self,
        user_id: i64,
        token_id: i64,
        space_id: &str,
        name: &str,
        workdir: &str,
        version: &str,
        read_only: bool,
    ) -> Result<String> {
        let t = now();
        let existing: Option<String> = self
            .conn
            .query_row(
                "SELECT slug FROM spaces WHERE token_id = ?1 AND space_id = ?2",
                params![token_id, space_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(slug) = existing {
            self.conn.execute(
                "UPDATE spaces SET name = ?1, workdir = ?2, version = ?3, read_only = ?4, last_seen = ?5
                 WHERE slug = ?6",
                params![name, workdir, version, read_only, t, slug],
            )?;
            return Ok(slug);
        }
        let slug = random_slug();
        self.conn.execute(
            "INSERT INTO spaces (slug, user_id, token_id, space_id, name, workdir, version, read_only, first_seen, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
            params![slug, user_id, token_id, space_id, name, workdir, version, read_only, t],
        )?;
        Ok(slug)
    }

    pub fn touch_space(&self, slug: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE spaces SET last_seen = ?1 WHERE slug = ?2",
            params![now(), slug],
        )?;
        Ok(())
    }

    fn space_rows(&self, filter: &str, arg: &dyn rusqlite::ToSql) -> Result<Vec<SpaceRow>> {
        let sql = format!(
            "SELECT s.slug, s.user_id, s.token_id, s.name, s.workdir, s.version, s.read_only, s.last_seen, t.name
             FROM spaces s JOIN tokens t ON t.id = s.token_id
             WHERE t.revoked_at IS NULL AND {filter} ORDER BY s.name, s.slug"
        );
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map([arg], |r| {
            Ok(SpaceRow {
                slug: r.get(0)?,
                user_id: r.get(1)?,
                token_id: r.get(2)?,
                name: r.get(3)?,
                workdir: r.get(4)?,
                version: r.get(5)?,
                read_only: r.get(6)?,
                last_seen: r.get(7)?,
                machine: r.get(8)?,
            })
        })?;
        rows.collect()
    }

    pub fn spaces(&self, user_id: i64) -> Result<Vec<SpaceRow>> {
        self.space_rows("s.user_id = ?1", &user_id)
    }

    pub fn space(&self, slug: &str) -> Result<Option<SpaceRow>> {
        Ok(self.space_rows("s.slug = ?1", &slug)?.into_iter().next())
    }

    pub fn forget_space(&self, user_id: i64, slug: &str) -> Result<bool> {
        Ok(self.conn.execute(
            "DELETE FROM spaces WHERE slug = ?1 AND user_id = ?2",
            params![slug, user_id],
        )? > 0)
    }

    /// A consistent copy of the database at `path` (for nightly backups).
    pub fn backup_to(&self, path: &Path) -> Result<()> {
        let _ = std::fs::remove_file(path);
        self.conn
            .execute("VACUUM INTO ?1", [path.to_string_lossy().as_ref()])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_sessions_and_spaces() {
        let db = Db::memory().unwrap();
        let ana = db.upsert_user("ana", Some(5)).unwrap();
        assert_eq!(db.upsert_user("ANA", None).unwrap().id, ana.id);

        let s = db.create_session(ana.id).unwrap();
        assert_eq!(db.session_user(&s).unwrap().unwrap().login, "ana");
        assert!(db.session_user("nope").unwrap().is_none());
        db.delete_session(&s).unwrap();
        assert!(db.session_user(&s).unwrap().is_none());

        let (tok, secret) = db.create_token(ana.id, "laptop").unwrap();
        assert!(secret.starts_with("avc_") && secret.len() == 44);
        let (tid, who) = db.use_token(&secret).unwrap().unwrap();
        assert_eq!((tid, who.id), (tok.id, ana.id));

        let slug = db
            .upsert_space(ana.id, tid, "sp1", "repo", "/r", "0.0.1", false)
            .unwrap();
        let again = db
            .upsert_space(ana.id, tid, "sp1", "repo2", "/r", "0.0.1", true)
            .unwrap();
        assert_eq!(slug, again);
        let rows = db.spaces(ana.id).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "repo2");
        assert!(rows[0].read_only);
        assert_eq!(rows[0].machine, "laptop");

        assert!(db.revoke_token(ana.id, tid).unwrap());
        assert!(db.use_token(&secret).unwrap().is_none());
        assert!(db.spaces(ana.id).unwrap().is_empty());
    }

    #[test]
    fn slugs_are_url_safe() {
        let s = random_slug();
        assert_eq!(s.len(), 12);
        assert!(s
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    }
}
