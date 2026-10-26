use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::Connection;

/// One schema change.
enum Step {
    Sql(&'static str),
    /// For changes SQL alone can't make, like rebuilding a table to change a
    /// constraint. Foreign keys are off while steps run and checked after.
    #[allow(dead_code)] // None yet.
    Code(fn(&Connection) -> rusqlite::Result<()>),
}

/// Schema migrations, applied in order. Step `i` brings the database to
/// `PRAGMA user_version = i + 1`. Released steps are frozen (a test checks
/// their hashes): never edit one, append another.
const MIGRATIONS: &[Step] = &[Step::Sql(V1), Step::Sql(V2), Step::Sql(V3)];

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// How many pre-upgrade backups to keep next to the database.
const BACKUPS_KEPT: usize = 3;

const V1: &str = r#"
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
"#;

const V2: &str = r#"
-- Children have no order in meaning, but keep a stable one for display.
-- Existing children keep their creation order.
ALTER TABLE tasks ADD COLUMN position INTEGER NOT NULL DEFAULT 0;
UPDATE tasks SET position = (
  SELECT COUNT(*) FROM tasks s
  WHERE s.parent_id = tasks.parent_id
    AND (s.created_at, s.id) < (tasks.created_at, tasks.id)
);
"#;

const V3: &str = r#"
-- A task's size, a Fibonacci number; ergon checks which values are allowed.
ALTER TABLE tasks ADD COLUMN size INTEGER CHECK (size > 0);
"#;

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
    // Before anything that locks, so a concurrent ergon waits instead of failing.
    conn.busy_timeout(BUSY_TIMEOUT)?;
    use_wal(&conn)?;
    configure(conn, Some(path))
}

/// Switches to WAL mode. That needs the database to itself, and SQLite won't
/// wait for it with the busy timeout, so retry while another ergon has it.
fn use_wal(conn: &Connection) -> Result<()> {
    let mode: String = conn.pragma_query_value(None, "journal_mode", |r| r.get(0))?;
    if mode.eq_ignore_ascii_case("wal") {
        return Ok(());
    }
    let deadline = std::time::Instant::now() + BUSY_TIMEOUT;
    loop {
        match conn.pragma_update(None, "journal_mode", "WAL") {
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::DatabaseBusy
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            r => return r.context("switching to WAL mode"),
        }
    }
}

/// An in-memory database with the current schema, for tests.
pub fn open_in_memory() -> Result<Connection> {
    configure(Connection::open_in_memory()?, None)
}

fn configure(conn: Connection, path: Option<&Path>) -> Result<Connection> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    if let Some((from, backup)) = migrate(&conn, MIGRATIONS, path)?
        && let Some(backup) = backup
    {
        eprintln!(
            "ergon: upgraded the database from schema {from} to {}; the old one is at {}",
            MIGRATIONS.len(),
            backup.display()
        );
    }
    conn.pragma_update(None, "foreign_keys", true)?;
    Ok(conn)
}

fn user_version(conn: &Connection) -> Result<usize> {
    let v: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    Ok(v as usize)
}

/// Brings the schema up to date with `steps`, backing up an existing database
/// at `path` first. Returns the version it started from and the backup, if it
/// changed anything.
fn migrate(
    conn: &Connection,
    steps: &[Step],
    path: Option<&Path>,
) -> Result<Option<(usize, Option<PathBuf>)>> {
    if user_version(conn).context("reading the schema version")? == steps.len() {
        return Ok(None);
    }
    // Foreign keys can't be switched inside a transaction, and table
    // rebuilds need them off.
    conn.pragma_update(None, "foreign_keys", false)
        .context("disabling foreign keys")?;
    // Take the write lock before reading the version: of two processes
    // opening an old database, one migrates and the other then finds it done.
    conn.execute_batch("BEGIN IMMEDIATE")
        .context("locking the database to upgrade it")?;
    match migrate_locked(conn, steps, path) {
        Ok(done) => {
            conn.execute_batch("COMMIT")?;
            Ok(done)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

fn migrate_locked(
    conn: &Connection,
    steps: &[Step],
    path: Option<&Path>,
) -> Result<Option<(usize, Option<PathBuf>)>> {
    let from = user_version(conn)?;
    if from > steps.len() {
        bail!(
            "database schema version {from} is newer than this ergon supports ({}); upgrade ergon",
            steps.len()
        );
    }
    if from == steps.len() {
        return Ok(None);
    }
    let backup = match path {
        Some(path) if from > 0 => Some(backup(path, from)?),
        _ => None,
    };
    for (i, step) in steps.iter().enumerate().skip(from) {
        match step {
            Step::Sql(sql) => conn.execute_batch(sql),
            Step::Code(f) => f(conn),
        }
        .with_context(|| format!("applying migration {}", i + 1))?;
    }
    let broken: i64 = conn.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
        r.get(0)
    })?;
    if broken > 0 {
        bail!("migrating left {broken} broken references; nothing was changed");
    }
    conn.pragma_update(None, "user_version", steps.len() as i64)?;
    Ok(Some((from, backup)))
}

/// `<path><suffix>`, e.g. `ergon.db.v1.bak`.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Copies the database, as it is at schema `version`, to
/// `<path>.v<version>.bak`, keeping only the newest few such backups. The
/// caller holds the write lock, so the copy is the state it's about to change.
fn backup(path: &Path, version: usize) -> Result<PathBuf> {
    let dest = with_suffix(path, &format!(".v{version}.bak"));
    let tmp = with_suffix(path, ".bak.tmp");
    let _ = std::fs::remove_file(&tmp);
    {
        use rusqlite::backup::{Backup, StepResult};
        // A connection of its own: the one holding the lock can't be read
        // by a backup.
        let src = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut out = Connection::open(&tmp)?;
        match Backup::new(&src, &mut out)?.step(-1)? {
            StepResult::Done => {}
            other => bail!("backing up the database: {other:?}"),
        }
    }
    std::fs::rename(&tmp, &dest)
        .with_context(|| format!("backing up the database to {}", dest.display()))?;

    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let prefix = format!("{name}.v");
    let mut old: Vec<(usize, PathBuf)> = std::fs::read_dir(
        dest.parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?
    .filter_map(|e| e.ok())
    .filter_map(|e| {
        let file = e.file_name().to_string_lossy().into_owned();
        let v = file
            .strip_prefix(&prefix)?
            .strip_suffix(".bak")?
            .parse()
            .ok()?;
        Some((v, e.path()))
    })
    .collect();
    old.sort_by_key(|(v, _)| std::cmp::Reverse(*v));
    for (_, p) in old.into_iter().skip(BACKUPS_KEPT) {
        let _ = std::fs::remove_file(p);
    }
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(conn: &Connection) -> usize {
        user_version(conn).unwrap()
    }

    /// Data written by each released schema version, oldest first.
    const FIXTURES: &[(usize, &str)] = &[(1, include_str!("../tests/fixtures/v1.sql"))];

    /// A database file at schema `v` holding `data`, in WAL mode as ergon
    /// always left it.
    fn database_at(path: &Path, v: usize, data: &str) {
        let conn = Connection::open(path).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        for step in &MIGRATIONS[..v] {
            match step {
                Step::Sql(sql) => conn.execute_batch(sql).unwrap(),
                Step::Code(f) => f(&conn).unwrap(),
            }
        }
        conn.execute_batch(data).unwrap();
        conn.pragma_update(None, "user_version", v as i64).unwrap();
    }

    fn rows<T: rusqlite::types::FromSql>(conn: &Connection, sql: &str) -> Vec<T> {
        let mut stmt = conn.prepare(sql).unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn schema(conn: &Connection) -> Vec<String> {
        rows(
            conn,
            "SELECT type || ' ' || name || ': ' || COALESCE(sql, '') FROM sqlite_master ORDER BY name",
        )
    }

    fn files(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| !n.ends_with("-wal") && !n.ends_with("-shm"))
            .collect();
        names.sort();
        names
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
        assert_eq!(version(&conn), MIGRATIONS.len());
    }

    #[test]
    fn reopening_file_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/ergon.db");
        drop(open(&path).unwrap());
        let conn = open(&path).unwrap();
        assert_eq!(version(&conn), MIGRATIONS.len());
        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }

    #[test]
    fn upgrades_released_versions_keeping_data() {
        let fresh = open_in_memory().unwrap();
        for &(v, data) in FIXTURES {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("ergon.db");
            database_at(&path, v, data);
            let count = |c: &Connection| -> Vec<i64> {
                rows(
                    c,
                    "SELECT COUNT(*) FROM projects UNION ALL SELECT COUNT(*) FROM tasks
                     UNION ALL SELECT COUNT(*) FROM task_deps",
                )
            };
            let before = count(&Connection::open(&path).unwrap());

            let conn = open(&path).unwrap();
            assert_eq!(version(&conn), MIGRATIONS.len(), "from v{v}");
            assert_eq!(count(&conn), before, "rows kept from v{v}");
            assert_eq!(schema(&conn), schema(&fresh), "schema from v{v}");
            let ok: Vec<String> = rows(&conn, "PRAGMA integrity_check");
            assert_eq!(ok, ["ok"]);
        }
    }

    #[test]
    fn v2_numbers_children_in_creation_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ergon.db");
        database_at(&path, 1, FIXTURES[0].1);
        let conn = open(&path).unwrap();
        // Frontend was created after Backend; Docs after API, though its id is
        // higher than Frontend's. The deleted task still gets a position.
        let got: Vec<String> = rows(
            &conn,
            "SELECT slug || '@' || position FROM tasks WHERE project_id = 1 ORDER BY id",
        );
        assert_eq!(
            got,
            ["root@0", "T4AL@0", "6Ahd@0", "gJmw@1", "fEmH@1", "aAQk@0"]
        );
    }

    #[test]
    fn backs_up_before_upgrading() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ergon.db");
        database_at(&path, 1, FIXTURES[0].1);
        drop(open(&path).unwrap());
        drop(open(&path).unwrap());
        assert_eq!(files(dir.path()), ["ergon.db", "ergon.db.v1.bak"]);

        let backup = Connection::open(dir.path().join("ergon.db.v1.bak")).unwrap();
        assert_eq!(version(&backup), 1);
        let tasks: Vec<i64> = rows(&backup, "SELECT COUNT(*) FROM tasks");
        assert_eq!(tasks, [8]);

        // A new database has nothing to back up.
        let fresh = tempfile::tempdir().unwrap();
        drop(open(&fresh.path().join("ergon.db")).unwrap());
        assert_eq!(files(fresh.path()), ["ergon.db"]);
    }

    #[test]
    fn keeps_the_newest_backups() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ergon.db");
        for v in 1..=4 {
            std::fs::write(dir.path().join(format!("ergon.db.v{v}.bak")), "").unwrap();
        }
        std::fs::write(dir.path().join("other.db.v1.bak"), "").unwrap();
        drop(open(&path).unwrap());
        backup(&path, 10).unwrap();
        assert_eq!(
            files(dir.path()),
            [
                "ergon.db",
                "ergon.db.v10.bak",
                "ergon.db.v3.bak",
                "ergon.db.v4.bak",
                "other.db.v1.bak"
            ]
        );
    }

    #[test]
    fn concurrent_opens_migrate_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ergon.db");
        database_at(&path, 1, FIXTURES[0].1);
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|s| {
            let opens: Vec<_> = (0..8)
                .map(|_| {
                    s.spawn(|| {
                        barrier.wait();
                        open(&path).map(|c| version(&c))
                    })
                })
                .collect();
            for o in opens {
                let v = o.join().unwrap().map_err(|e| format!("{e:#}")).unwrap();
                assert_eq!(v, MIGRATIONS.len());
            }
        });
        assert_eq!(files(dir.path()), ["ergon.db", "ergon.db.v1.bak"]);
    }

    #[test]
    fn concurrent_first_opens() {
        for _ in 0..10 {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("ergon.db");
            let barrier = std::sync::Barrier::new(8);
            std::thread::scope(|s| {
                let opens: Vec<_> = (0..8)
                    .map(|_| {
                        s.spawn(|| {
                            barrier.wait();
                            open(&path)
                                .map(|c| version(&c))
                                .map_err(|e| format!("{e:#}"))
                        })
                    })
                    .collect();
                for o in opens {
                    assert_eq!(o.join().unwrap().unwrap(), MIGRATIONS.len());
                }
            });
        }
    }

    /// FNV-1a, to notice edits to released steps.
    fn hash(s: &str) -> u64 {
        s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
            (h ^ b as u64).wrapping_mul(0x0100_0000_01b3)
        })
    }

    #[test]
    fn released_steps_are_frozen() {
        // Append a hash when releasing a step; never change one.
        const RELEASED: &[u64] = &[
            0xcd8d_2925_364c_ec89,
            0x1c24_1903_3f02_e878,
            0x07e4_3aef_e5a3_812f,
        ];
        let hashes: Vec<u64> = MIGRATIONS
            .iter()
            .map(|s| match s {
                Step::Sql(sql) => hash(sql),
                Step::Code(_) => 0,
            })
            .collect();
        assert_eq!(&hashes[..RELEASED.len()], RELEASED, "{hashes:#x?}");
    }

    #[test]
    fn code_steps_run_without_foreign_keys() {
        // Rebuilding a table that others reference only works with foreign
        // keys off.
        fn rebuild(conn: &Connection) -> rusqlite::Result<()> {
            conn.execute_batch(
                "CREATE TABLE projects_new (
                   id INTEGER PRIMARY KEY, uuid TEXT NOT NULL UNIQUE, name TEXT NOT NULL UNIQUE,
                   created_at INTEGER NOT NULL, retired_at INTEGER);
                 INSERT INTO projects_new SELECT * FROM projects;
                 DROP TABLE projects;
                 ALTER TABLE projects_new RENAME TO projects;",
            )
        }
        let conn = open_in_memory().unwrap();
        let p = insert_project(&conn);
        insert_task(&conn, p, "root", None).unwrap();
        let mut steps: Vec<Step> = MIGRATIONS
            .iter()
            .map(|s| match s {
                Step::Sql(sql) => Step::Sql(sql),
                Step::Code(f) => Step::Code(*f),
            })
            .collect();
        steps.push(Step::Code(rebuild));
        migrate(&conn, &steps, None).unwrap();
        assert_eq!(version(&conn), steps.len());
        let tasks: Vec<i64> = rows(&conn, "SELECT COUNT(*) FROM tasks");
        assert_eq!(tasks, [1]);
    }

    #[test]
    fn broken_references_roll_back() {
        fn orphan(conn: &Connection) -> rusqlite::Result<()> {
            conn.execute_batch(
                "INSERT INTO tasks (project_id, slug, intent, created_at, updated_at)
                 VALUES (999, 'root', 'x', 0, 0)",
            )
        }
        let conn = open_in_memory().unwrap();
        let mut steps: Vec<Step> = MIGRATIONS
            .iter()
            .map(|s| match s {
                Step::Sql(sql) => Step::Sql(sql),
                Step::Code(f) => Step::Code(*f),
            })
            .collect();
        steps.push(Step::Code(orphan));
        let err = migrate(&conn, &steps, None).unwrap_err().to_string();
        assert!(err.contains("broken references"), "{err}");
        assert_eq!(version(&conn), MIGRATIONS.len());
        let tasks: Vec<i64> = rows(&conn, "SELECT COUNT(*) FROM tasks");
        assert_eq!(tasks, [0]);
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
