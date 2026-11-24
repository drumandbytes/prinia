//! The history: plain SQLite, with each turn's content compressed and then encrypted.
//! Metadata (time, model, tokens, a hash of the directory) stays readable without the key, so
//! stats and prompt integrations never need it; content never is.
use crate::key::{self, Key};
use anyhow::{Context, Result, anyhow, bail};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub struct Meta<'a> {
    pub dir: &'a str,
    pub provider: &'a str,
    pub model: &'a str,
    pub mode: &'a str,
    pub tier: &'a str,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub duration_ms: u64,
    pub cache_key: &'a str,
}

/// The encrypted part of a turn.
#[derive(Default)]
pub struct Content {
    pub instruction: String,
    pub payload: String,
    pub response: String,
    pub cwd: String,
    pub repo: Option<String>,
    pub branch: Option<String>,
}

pub struct Turn {
    pub id: i64,
    pub when: String,
    pub provider: String,
    pub model: String,
    pub mode: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub content: Content,
}

/// Seconds relative to now for `since`/`older`; `dir` is a directory hash.
#[derive(Default)]
pub struct Filter {
    pub since: Option<i64>,
    pub older: Option<i64>,
    pub dir: Option<String>,
}

pub struct Store {
    conn: Connection,
    data_key: Key,
}

const SCHEMA_VERSION: i64 = 1;

/// One database per user, in the global data directory. Debug builds read PRINIA_DB so
/// development and tests never touch the real history.
pub fn path() -> Result<PathBuf> {
    #[cfg(debug_assertions)]
    if let Some(p) = std::env::var_os("PRINIA_DB") {
        return Ok(PathBuf::from(p));
    }
    let dirs = directories::ProjectDirs::from("", "", "prinia")
        .ok_or_else(|| anyhow!("cannot determine data directory"))?;
    std::fs::create_dir_all(dirs.data_dir())?;
    Ok(dirs.data_dir().join("history.db"))
}

/// Not secret-keyed on purpose: a prompt integration must be able to compute it without the key.
pub fn dir_hash(dir: &Path) -> String {
    blake3::hash(dir.to_string_lossy().as_bytes()).to_hex()[..16].to_string()
}

fn seal(key: &Key, plain: &[u8]) -> Result<Vec<u8>> {
    let mut nonce = [0u8; 24];
    getrandom::fill(&mut nonce).map_err(|e| anyhow!("no system randomness: {e}"))?;
    let cipher = XChaCha20Poly1305::new_from_slice(key).map_err(|_| anyhow!("bad key length"))?;
    let ct = cipher
        .encrypt(&XNonce::from(nonce), plain)
        .map_err(|_| anyhow!("encryption failed"))?;
    Ok([nonce.as_slice(), &ct].concat())
}

fn unseal(key: &Key, blob: &[u8]) -> Result<Vec<u8>> {
    if blob.len() < 24 {
        bail!("corrupt record");
    }
    let (nonce, ct) = blob.split_at(24);
    let nonce: [u8; 24] = nonce.try_into().expect("24 bytes");
    let cipher = XChaCha20Poly1305::new_from_slice(key).map_err(|_| anyhow!("bad key length"))?;
    cipher
        .decrypt(&XNonce::from(nonce), ct)
        .map_err(|_| anyhow!("cannot decrypt (wrong key?)"))
}

#[derive(Default)]
pub struct Stats {
    pub calls: u64,
    pub cache_hits: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Usage totals, read without the key (a prompt must stay fast and never trigger a key prompt).
/// `since_days`: 0 is today only. A missing history is all zeros.
pub fn read_stats(dir: Option<&str>, since_days: Option<i64>) -> Result<Stats> {
    let path = path()?;
    if !path.exists() {
        return Ok(Stats::default());
    }
    let conn = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(std::time::Duration::from_millis(300))?;
    let n = |r: &rusqlite::Row, i| r.get::<_, i64>(i).map(|v| v as u64);
    Ok(conn.query_row(
        "SELECT COALESCE(SUM(calls),0), COALESCE(SUM(cache_hits),0), COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0)
         FROM stats WHERE (?1 IS NULL OR dir = ?1) AND (?2 IS NULL OR day >= date('now','localtime', '-' || ?2 || ' days'))",
        params![dir, since_days],
        |r| {
            Ok(Stats {
                calls: n(r, 0)?,
                cache_hits: n(r, 1)?,
                input_tokens: n(r, 2)?,
                output_tokens: n(r, 3)?,
            })
        },
    )?)
}

/// The data key from an unfinished rotation, if `master` unlocks it.
fn pending_key(conn: &Connection, master: &Key) -> Option<Vec<u8>> {
    let w: Vec<u8> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'data_key_pending'",
            [],
            |r| r.get(0),
        )
        .optional()
        .ok()??;
    unseal(master, &w).ok()
}

fn promote_pending(conn: &Connection) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "UPDATE meta SET value = (SELECT value FROM meta WHERE key = 'data_key_pending') WHERE key = 'data_key'",
        [],
    )?;
    tx.execute("DELETE FROM meta WHERE key = 'data_key_pending'", [])?;
    Ok(tx.commit()?)
}

fn pack(key: &Key, c: &Content) -> Result<Vec<u8>> {
    let v = json!({"i": c.instruction, "p": c.payload, "r": c.response, "cwd": c.cwd, "repo": c.repo, "branch": c.branch});
    seal(key, &zstd::encode_all(v.to_string().as_bytes(), 3)?)
}

fn unpack(key: &Key, blob: &[u8]) -> Result<Content> {
    let v: Value = serde_json::from_slice(&zstd::decode_all(unseal(key, blob)?.as_slice())?)?;
    let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
    let o = |k: &str| v[k].as_str().map(str::to_owned);
    Ok(Content {
        instruction: s("i"),
        payload: s("p"),
        response: s("r"),
        cwd: s("cwd"),
        repo: o("repo"),
        branch: o("branch"),
    })
}

impl Store {
    /// Opens the history, creating the database, the data key and (only for a new database) the
    /// master key as needed. A missing master key never silently orphans existing history.
    pub fn open(keychain: bool) -> Result<Self> {
        Self::open_at(&path()?, keychain)
    }

    pub fn open_at(path: &Path, keychain: bool) -> Result<Self> {
        let conn =
            Connection::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        // parallel prinia processes (`--each`, shell prompts) share this file
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            bail!("the history was written by a newer version of prinia (schema {version})");
        }
        conn.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value BLOB NOT NULL);
             CREATE TABLE IF NOT EXISTS turns (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts INTEGER NOT NULL,
                dir TEXT NOT NULL,
                provider TEXT NOT NULL,
                model TEXT NOT NULL,
                mode TEXT NOT NULL,
                tier TEXT NOT NULL,
                input_tokens INTEGER NOT NULL,
                output_tokens INTEGER NOT NULL,
                duration_ms INTEGER NOT NULL,
                cache_key TEXT NOT NULL,
                content BLOB NOT NULL
             );
             CREATE INDEX IF NOT EXISTS turns_dir ON turns(dir, ts);
             CREATE INDEX IF NOT EXISTS turns_cache ON turns(cache_key, ts);
             CREATE TABLE IF NOT EXISTS stats (
                day TEXT NOT NULL,
                dir TEXT NOT NULL,
                calls INTEGER NOT NULL DEFAULT 0,
                cache_hits INTEGER NOT NULL DEFAULT 0,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (day, dir)
             );
             CREATE TABLE IF NOT EXISTS summaries (
                key TEXT PRIMARY KEY,
                content BLOB NOT NULL
             );
             PRAGMA user_version = {SCHEMA_VERSION};"
        ))?;

        let read_wrapped = || -> Result<Option<Vec<u8>>> {
            Ok(conn
                .query_row("SELECT value FROM meta WHERE key = 'data_key'", [], |r| {
                    r.get(0)
                })
                .optional()?)
        };
        let mut wrapped = read_wrapped()?;
        if wrapped.is_none() {
            // First use. Take the write lock before creating any key, so two processes starting
            // at once cannot each make their own master key and lock each other out.
            conn.execute_batch("BEGIN IMMEDIATE")?;
            wrapped = read_wrapped()?;
            if wrapped.is_none() {
                let master = match key::load(keychain)? {
                    Some(m) => m,
                    None => {
                        let m = key::random()?;
                        key::save(&m, keychain)?;
                        m
                    }
                };
                let dk = key::random()?;
                conn.execute(
                    "INSERT INTO meta (key, value) VALUES ('data_key', ?1)",
                    params![seal(&master, &dk)?],
                )?;
                conn.execute_batch("COMMIT")?;
                return Ok(Store { conn, data_key: dk });
            }
            conn.execute_batch("COMMIT")?;
        }
        let data_key = match (wrapped, key::load(keychain)?) {
            (Some(w), Some(master)) => {
                let dk = match unseal(&master, &w) {
                    Ok(k) => k,
                    // `key rotate` saved the new key file but stopped before the database caught up
                    Err(e) => match pending_key(&conn, &master) {
                        Some(k) => {
                            promote_pending(&conn)?;
                            k
                        }
                        None => return Err(e).context("the key does not match this history"),
                    },
                };
                dk.try_into().map_err(|_| anyhow!("corrupt data key"))?
            }
            (Some(_), None) => bail!(
                "the history exists but its key is missing; restore it with `prinia key import`"
            ),
            (None, _) => bail!("the history has no data key"),
        };
        Ok(Store { conn, data_key })
    }

    /// Re-wrap the data key under a new master key. The history itself is not rewritten.
    /// Crash-safe in three steps: the new wrapping is stored beside the old, then the key is
    /// saved, then the new wrapping replaces the old. `open_at` finishes it if the process stopped
    /// between the last two.
    pub fn rewrap(&self, new_master: &Key, keychain: bool) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('data_key_pending', ?1)",
            params![seal(new_master, &self.data_key)?],
        )?;
        if let Err(e) = key::save(new_master, keychain) {
            self.conn
                .execute("DELETE FROM meta WHERE key = 'data_key_pending'", [])?;
            return Err(e);
        }
        promote_pending(&self.conn)
    }

    /// Whether `master` unlocks the history at `path` (used before importing a key).
    pub fn key_fits(path: &Path, master: &Key) -> Result<bool> {
        if !path.exists() {
            return Ok(true);
        }
        let conn = Connection::open(path)?;
        let wrapped: Option<Vec<u8>> = conn
            .query_row("SELECT value FROM meta WHERE key = 'data_key'", [], |r| {
                r.get(0)
            })
            .optional()
            .unwrap_or(None);
        Ok(wrapped.is_none_or(|w| unseal(master, &w).is_ok())
            || pending_key(&conn, master).is_some())
    }

    /// A keyed hash, so the stored value reveals nothing about the prompt it stands for.
    pub fn cache_key(&self, parts: &[&str]) -> String {
        blake3::keyed_hash(&self.data_key, json!(parts).to_string().as_bytes())
            .to_hex()
            .to_string()
    }

    pub fn record(&self, m: &Meta, c: &Content) -> Result<()> {
        self.conn.execute(
            "INSERT INTO turns (ts, dir, provider, model, mode, tier, input_tokens, output_tokens, duration_ms, cache_key, content)
             VALUES (unixepoch(), ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![m.dir, m.provider, m.model, m.mode, m.tier, m.input_tokens as i64, m.output_tokens as i64, m.duration_ms as i64, m.cache_key, pack(&self.data_key, c)?],
        )?;
        self.bump(m.dir, 0, m.input_tokens, m.output_tokens)
    }

    /// The newest response recorded for this exact call, if it is younger than `ttl` seconds.
    pub fn cached(&self, cache_key: &str, ttl: i64, dir: &str) -> Result<Option<String>> {
        let blob: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT content FROM turns WHERE cache_key = ?1 AND ts >= unixepoch() - ?2 ORDER BY ts DESC, id DESC LIMIT 1",
                params![cache_key, ttl],
                |r| r.get(0),
            )
            .optional()?;
        let Some(blob) = blob else { return Ok(None) };
        self.bump(dir, 1, 0, 0)?;
        Ok(Some(unpack(&self.data_key, &blob)?.response))
    }

    fn bump(&self, dir: &str, hit: u64, input: u64, output: u64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO stats (day, dir, calls, cache_hits, input_tokens, output_tokens)
             VALUES (date('now','localtime'), ?1, 1, ?2, ?3, ?4)
             ON CONFLICT (day, dir) DO UPDATE SET calls = calls + 1, cache_hits = cache_hits + ?2,
                input_tokens = input_tokens + ?3, output_tokens = output_tokens + ?4",
            params![dir, hit as i64, input as i64, output as i64],
        )?;
        Ok(())
    }

    /// Newest first.
    pub fn list(&self, f: &Filter, limit: usize) -> Result<Vec<Turn>> {
        self.select(
            "WHERE (?1 IS NULL OR ts >= unixepoch() - ?1) AND (?2 IS NULL OR ts < unixepoch() - ?2) AND (?3 IS NULL OR dir = ?3)
             ORDER BY ts DESC, id DESC LIMIT ?4",
            params![f.since, f.older, f.dir, limit as i64],
        )
    }

    pub fn get(&self, id: i64) -> Result<Option<Turn>> {
        Ok(self
            .select("WHERE id = ?1", params![id])?
            .into_iter()
            .next())
    }

    fn select(&self, tail: &str, p: &[&dyn rusqlite::ToSql]) -> Result<Vec<Turn>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT id, datetime(ts,'unixepoch','localtime'), provider, model, mode, input_tokens, output_tokens, content FROM turns {tail}"
        ))?;
        let rows = stmt.query_map(p, |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, Vec<u8>>(7)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, when, provider, model, mode, i, o, blob) = row?;
            out.push(Turn {
                id,
                when,
                provider,
                model,
                mode,
                input_tokens: i as u64,
                output_tokens: o as u64,
                content: unpack(&self.data_key, &blob)?,
            });
        }
        Ok(out)
    }

    /// Ids to delete so the history fits `max_bytes` of stored content and `max_age` seconds,
    /// oldest first.
    pub fn over_limits(&self, max_bytes: u64, max_age: Option<i64>) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, length(content), (?1 IS NOT NULL AND ts < unixepoch() - ?1) FROM turns ORDER BY ts, id",
        )?;
        let rows: Vec<(i64, i64, bool)> = stmt
            .query_map(params![max_age], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut total: i64 = rows.iter().map(|r| r.1).sum();
        let mut doomed = Vec::new();
        for (id, len, expired) in rows {
            if expired || total > max_bytes as i64 {
                doomed.push(id);
                total -= len;
            }
        }
        Ok(doomed)
    }

    pub fn content_bytes(&self) -> Result<u64> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(SUM(length(content)), 0) FROM turns",
            [],
            |r| r.get::<_, i64>(0),
        )? as u64)
    }

    /// Give freed space back to the file system.
    pub fn compact(&self) -> Result<()> {
        self.conn.execute_batch("VACUUM")?;
        Ok(())
    }

    /// A stored summary of a block of turns (see `context`), keyed by the exact turns it covers.
    pub fn summary_get(&self, key: &str) -> Result<Option<String>> {
        let blob: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT content FROM summaries WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?;
        blob.map(|b| Ok(String::from_utf8_lossy(&unseal(&self.data_key, &b)?).into_owned()))
            .transpose()
    }

    pub fn summary_put(&self, key: &str, text: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO summaries (key, content) VALUES (?1, ?2)",
            params![key, seal(&self.data_key, text.as_bytes())?],
        )?;
        Ok(())
    }

    pub fn delete(&self, ids: &[i64]) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        // a summary may contain what was just deleted; they are rebuilt on demand
        tx.execute("DELETE FROM summaries", [])?;
        let mut n = 0;
        for id in ids {
            n += tx.execute("DELETE FROM turns WHERE id = ?1", params![id])?;
        }
        tx.commit()?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_survives_a_crash_after_the_key_file_was_replaced() {
        let d = std::env::temp_dir().join(format!("prinia-rot-{}", std::process::id()));
        std::fs::create_dir_all(d.join("cfg")).unwrap();
        // SAFETY: no other test reads the config location while this one runs a store
        unsafe { std::env::set_var("PRINIA_CONFIG", d.join("cfg/config.toml")) };
        let db = d.join("h.db");
        let s = Store::open_at(&db, false).unwrap();
        let meta = Meta {
            dir: "d",
            provider: "p",
            model: "m",
            mode: "pipe",
            tier: "t",
            input_tokens: 1,
            output_tokens: 1,
            duration_ms: 1,
            cache_key: "k",
        };
        let content = Content {
            response: "kept".into(),
            ..Default::default()
        };
        s.record(&meta, &content).unwrap();
        // the crash: new key saved and its wrapping stored, but `data_key` still the old one
        let new = key::random().unwrap();
        s.conn
            .execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('data_key_pending', ?1)",
                params![seal(&new, &s.data_key).unwrap()],
            )
            .unwrap();
        key::save(&new, false).unwrap();
        drop(s);
        let s = Store::open_at(&db, false).unwrap();
        assert_eq!(s.get(1).unwrap().unwrap().content.response, "kept");
        let pending: Option<Vec<u8>> = s
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'data_key_pending'",
                [],
                |r| r.get(0),
            )
            .optional()
            .unwrap();
        assert!(pending.is_none(), "the rotation was finished");
        drop(s); // Windows will not delete a file that is still open
        std::fs::remove_dir_all(&d).unwrap();
    }
}
