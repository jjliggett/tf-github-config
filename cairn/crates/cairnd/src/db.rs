//! SQLite storage: one writer connection, a few reader connections (WAL mode).
//!
//! Queries are microseconds long, so they run inline on the async worker that
//! needs them instead of hopping to a blocking pool (no extra threads at idle).

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::Connection;

const MIGRATIONS: &[&str] = &[
    // 1: initial schema
    r#"
    CREATE TABLE spaces (
        id          TEXT PRIMARY KEY,
        name        TEXT NOT NULL UNIQUE,
        email       TEXT NOT NULL DEFAULT '',
        created_at  TEXT NOT NULL
    );
    CREATE TABLE stacks (
        id               TEXT PRIMARY KEY,
        space_id         TEXT NOT NULL REFERENCES spaces(id),
        name             TEXT NOT NULL,
        description      TEXT NOT NULL DEFAULT '',
        engine_version   TEXT,
        auto_apply       INTEGER NOT NULL DEFAULT 0,
        project_root     TEXT NOT NULL DEFAULT '',
        labels           TEXT NOT NULL DEFAULT '[]',
        lock_id          TEXT,
        lock_info        TEXT,
        current_state_id TEXT,
        latest_run_id    TEXT,
        created_at       TEXT NOT NULL,
        updated_at       TEXT NOT NULL,
        UNIQUE (space_id, name)
    );
    CREATE TABLE config_versions (
        id          TEXT PRIMARY KEY,
        stack_id    TEXT NOT NULL REFERENCES stacks(id) ON DELETE CASCADE,
        status      TEXT NOT NULL,
        auto_queue  INTEGER NOT NULL DEFAULT 0,
        speculative INTEGER NOT NULL DEFAULT 0,
        sha256      TEXT,
        size        INTEGER,
        created_at  TEXT NOT NULL
    );
    CREATE TABLE runs (
        seq            INTEGER PRIMARY KEY AUTOINCREMENT,
        id             TEXT NOT NULL UNIQUE,
        stack_id       TEXT NOT NULL REFERENCES stacks(id) ON DELETE CASCADE,
        kind           TEXT NOT NULL,
        status         TEXT NOT NULL,
        config_id      TEXT,
        auto_apply     INTEGER NOT NULL DEFAULT 0,
        message        TEXT NOT NULL DEFAULT '',
        triggered_by   TEXT NOT NULL DEFAULT '',
        has_changes    INTEGER,
        adds           INTEGER,
        changes        INTEGER,
        destroys       INTEGER,
        plan_summary   TEXT,
        error          TEXT,
        created_at     TEXT NOT NULL,
        status_times   TEXT NOT NULL DEFAULT '{}'
    );
    CREATE INDEX runs_by_stack ON runs (stack_id, seq DESC);
    CREATE INDEX runs_by_status ON runs (status, seq);
    CREATE TABLE state_versions (
        id             TEXT PRIMARY KEY,
        stack_id       TEXT NOT NULL REFERENCES stacks(id) ON DELETE CASCADE,
        serial         INTEGER NOT NULL,
        lineage        TEXT NOT NULL DEFAULT '',
        sha256         TEXT NOT NULL,
        size           INTEGER NOT NULL,
        run_id         TEXT,
        outputs        TEXT NOT NULL DEFAULT '{}',
        resource_count INTEGER NOT NULL DEFAULT 0,
        created_at     TEXT NOT NULL
    );
    CREATE INDEX state_by_stack ON state_versions (stack_id, serial DESC);
    CREATE TABLE variables (
        id         TEXT PRIMARY KEY,
        stack_id   TEXT NOT NULL REFERENCES stacks(id) ON DELETE CASCADE,
        key        TEXT NOT NULL,
        value      TEXT NOT NULL,
        category   TEXT NOT NULL,
        sensitive  INTEGER NOT NULL DEFAULT 0,
        hcl        INTEGER NOT NULL DEFAULT 0,
        UNIQUE (stack_id, key, category)
    );
    CREATE TABLE tokens (
        id           TEXT PRIMARY KEY,
        sha256       TEXT NOT NULL UNIQUE,
        description  TEXT NOT NULL DEFAULT '',
        created_at   TEXT NOT NULL
    );
    "#,
    // 2: identity and access
    r#"
    ALTER TABLE spaces ADD COLUMN parent_id TEXT REFERENCES spaces(id);
    ALTER TABLE spaces ADD COLUMN description TEXT NOT NULL DEFAULT '';
    CREATE TABLE users (
        id           TEXT PRIMARY KEY,
        subject      TEXT NOT NULL UNIQUE,
        email        TEXT NOT NULL DEFAULT '',
        name         TEXT NOT NULL DEFAULT '',
        groups       TEXT NOT NULL DEFAULT '[]',
        site_admin   INTEGER NOT NULL DEFAULT 0,
        created_at   TEXT NOT NULL,
        last_seen_at TEXT
    );
    CREATE TABLE role_bindings (
        id             TEXT PRIMARY KEY,
        space_id       TEXT NOT NULL REFERENCES spaces(id) ON DELETE CASCADE,
        principal_kind TEXT NOT NULL,
        principal      TEXT NOT NULL,
        role           TEXT NOT NULL,
        created_at     TEXT NOT NULL,
        UNIQUE (space_id, principal_kind, principal)
    );
    ALTER TABLE tokens ADD COLUMN kind TEXT NOT NULL DEFAULT 'service';
    ALTER TABLE tokens ADD COLUMN owner TEXT;
    ALTER TABLE tokens ADD COLUMN site_admin INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE tokens ADD COLUMN expires_at TEXT;
    -- Tokens created before roles existed were all-powerful; keep them working.
    UPDATE tokens SET site_admin = 1;
    CREATE TABLE audit_log (
        seq       INTEGER PRIMARY KEY AUTOINCREMENT,
        at        TEXT NOT NULL,
        principal TEXT NOT NULL,
        method    TEXT NOT NULL,
        path      TEXT NOT NULL,
        status    INTEGER NOT NULL
    );
    "#,
];

pub struct Db {
    writer: Mutex<Connection>,
    readers: Vec<Mutex<Connection>>,
    next: AtomicUsize,
}

fn open(path: &Path) -> Result<Connection> {
    let c = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    c.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;
         PRAGMA cache_size = -1024;
         PRAGMA temp_store = MEMORY;",
    )?;
    Ok(c)
}

/// Process-wide SQLite settings; must run before the first connection.
fn configure_sqlite() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        // Memory statistics take a global mutex on every SQLite allocation,
        // which serializes all reader connections. We don't use them.
        rusqlite::ffi::sqlite3_config(rusqlite::ffi::SQLITE_CONFIG_MEMSTATUS, 0);
    });
}

impl Db {
    pub fn open(path: &Path, readers: usize) -> Result<Self> {
        configure_sqlite();
        let writer = open(path)?;
        migrate(&writer)?;
        let readers = (0..readers.max(1))
            .map(|_| open(path).map(Mutex::new))
            .collect::<Result<Vec<_>>>()?;
        Ok(Db { writer: Mutex::new(writer), readers, next: AtomicUsize::new(0) })
    }

    /// Run a read-only closure on one of the reader connections.
    pub fn read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        // Prefer an idle reader; fall back to waiting on the round-robin choice.
        for r in &self.readers {
            if let Ok(c) = r.try_lock() {
                return f(&c);
            }
        }
        let i = self.next.fetch_add(1, Ordering::Relaxed) % self.readers.len();
        let c = self.readers[i].lock().unwrap_or_else(|e| e.into_inner());
        f(&c)
    }

    /// Run a closure inside a write transaction on the single writer connection.
    pub fn write<T>(&self, f: impl FnOnce(&rusqlite::Transaction) -> Result<T>) -> Result<T> {
        let mut c = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let tx = c.transaction()?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }
}

fn migrate(c: &Connection) -> Result<()> {
    c.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (v INTEGER NOT NULL)")?;
    let v: i64 = c
        .query_row("SELECT COALESCE(MAX(v), 0) FROM schema_version", [], |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(v as usize) {
        c.execute_batch(&format!("BEGIN; {sql}; INSERT INTO schema_version (v) VALUES ({}); COMMIT;", i + 1))
            .with_context(|| format!("applying migration {}", i + 1))?;
    }
    Ok(())
}
