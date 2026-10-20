use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::Connection;

/// Schema migrations, applied in order. Migration `i` brings the database to
/// `PRAGMA user_version = i + 1`. Never edit a released migration; append.
const MIGRATIONS: &[&str] = &[r#"
CREATE TABLE projects (
  id         INTEGER PRIMARY KEY,
  uuid       TEXT    NOT NULL UNIQUE,
  name       TEXT    NOT NULL UNIQUE,
  created_at INTEGER NOT NULL,
  retired_at INTEGER
);

CREATE TABLE tasks (
  id             INTEGER PRIMARY KEY,
  project_id     INTEGER NOT NULL REFERENCES projects(id),
  slug           TEXT    NOT NULL,
  parent_id      INTEGER REFERENCES tasks(id),
  intent         TEXT    NOT NULL,
  status         TEXT CHECK (status IN ('todo','in_progress','landing','done','cancelled')),
  priority       INTEGER CHECK (priority BETWEEN 0 AND 4),
  claude_session TEXT,
  claude_cwd     TEXT,
  created_at     INTEGER NOT NULL,
  updated_at     INTEGER NOT NULL,
  deleted_at     INTEGER,
  UNIQUE (project_id, slug),
  CHECK ((claude_session IS NULL) = (claude_cwd IS NULL))
);

CREATE UNIQUE INDEX one_root_per_project ON tasks(project_id) WHERE parent_id IS NULL;
CREATE INDEX tasks_parent ON tasks(parent_id);

CREATE TABLE task_deps (
  task_id    INTEGER NOT NULL REFERENCES tasks(id),
  depends_on INTEGER NOT NULL REFERENCES tasks(id),
  PRIMARY KEY (task_id, depends_on)
);

CREATE INDEX task_deps_depends_on ON task_deps(depends_on);
"#];

/// `$ERGON_DB`, else `$XDG_DATA_HOME/ergon/ergon.db`, else
/// `~/.local/share/ergon/ergon.db`.
pub fn default_path() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("ERGON_DB").filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(p));
    }
    let data = match std::env::var_os("XDG_DATA_HOME").filter(|p| !p.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => {
            let home = std::env::var_os("HOME").context("neither ERGON_DB nor HOME is set")?;
            PathBuf::from(home).join(".local/share")
        }
    };
    Ok(data.join("ergon/ergon.db"))
}

/// Opens (creating if needed) the database at `path` and brings its schema up
/// to date.
pub fn open(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating database directory {}", dir.display()))?;
    }
    let conn =
        Connection::open(path).with_context(|| format!("opening database {}", path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    configure(conn)
}

/// An in-memory database with the current schema, for tests.
pub fn open_in_memory() -> Result<Connection> {
    configure(Connection::open_in_memory()?)
}

fn configure(mut conn: Connection) -> Result<Connection> {
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "foreign_keys", true)?;
    migrate(&mut conn)?;
    Ok(conn)
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version as usize > MIGRATIONS.len() {
        bail!(
            "database schema version {version} is newer than this ergon supports ({}); upgrade ergon",
            MIGRATIONS.len()
        );
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)
            .with_context(|| format!("applying migration {}", i + 1))?;
        tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_version(conn: &Connection) -> usize {
        conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap() as usize
    }

    fn insert_project(conn: &Connection) -> i64 {
        conn.execute(
            "INSERT INTO projects (uuid, name, created_at) VALUES ('u', 'p', 0)",
            [],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn insert_task(
        conn: &Connection,
        project: i64,
        slug: &str,
        parent: Option<i64>,
    ) -> rusqlite::Result<usize> {
        conn.execute(
            "INSERT INTO tasks (project_id, slug, parent_id, intent, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'x', 0, 0)",
            rusqlite::params![project, slug, parent],
        )
    }

    #[test]
    fn migrates_to_latest() {
        let conn = open_in_memory().unwrap();
        assert_eq!(user_version(&conn), MIGRATIONS.len());
    }

    #[test]
    fn reopening_file_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/ergon.db");
        drop(open(&path).unwrap());
        let conn = open(&path).unwrap();
        assert_eq!(user_version(&conn), MIGRATIONS.len());
        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }

    #[test]
    fn refuses_newer_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ergon.db");
        let conn = open(&path).unwrap();
        conn.pragma_update(None, "user_version", MIGRATIONS.len() as i64 + 1)
            .unwrap();
        drop(conn);
        let err = open(&path).unwrap_err().to_string();
        assert!(err.contains("newer"), "{err}");
    }

    #[test]
    fn one_root_per_project() {
        let conn = open_in_memory().unwrap();
        let p = insert_project(&conn);
        insert_task(&conn, p, "root", None).unwrap();
        assert!(insert_task(&conn, p, "Abcd", None).is_err());
        let root = conn.last_insert_rowid();
        insert_task(&conn, p, "Abcd", Some(root)).unwrap();
    }

    #[test]
    fn slugs_unique_per_project() {
        let conn = open_in_memory().unwrap();
        let p = insert_project(&conn);
        insert_task(&conn, p, "root", None).unwrap();
        let root = conn.last_insert_rowid();
        insert_task(&conn, p, "Abcd", Some(root)).unwrap();
        assert!(insert_task(&conn, p, "Abcd", Some(root)).is_err());
    }

    #[test]
    fn session_and_cwd_go_together() {
        let conn = open_in_memory().unwrap();
        let p = insert_project(&conn);
        insert_task(&conn, p, "root", None).unwrap();
        let set = |session: Option<&str>, cwd: Option<&str>| {
            conn.execute(
                "UPDATE tasks SET claude_session = ?1, claude_cwd = ?2",
                rusqlite::params![session, cwd],
            )
        };
        assert!(set(Some("s"), None).is_err());
        assert!(set(None, Some("/tmp")).is_err());
        set(Some("s"), Some("/tmp")).unwrap();
    }

    #[test]
    fn rejects_bad_status_and_priority() {
        let conn = open_in_memory().unwrap();
        let p = insert_project(&conn);
        insert_task(&conn, p, "root", None).unwrap();
        assert!(
            conn.execute("UPDATE tasks SET status = 'blocked'", [])
                .is_err()
        );
        assert!(conn.execute("UPDATE tasks SET priority = 5", []).is_err());
        conn.execute("UPDATE tasks SET status = 'landing', priority = 0", [])
            .unwrap();
    }

    #[test]
    fn foreign_keys_enforced() {
        let conn = open_in_memory().unwrap();
        let p = insert_project(&conn);
        assert!(insert_task(&conn, p, "root", Some(999)).is_err());
    }
}
