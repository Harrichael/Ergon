use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Row, params};

/// The file that ties a directory tree to a project. It holds the project's
/// UUID on a single line.
pub const MARKER: &str = ".ergon";

/// The root task's fixed slug.
pub const ROOT_SLUG: &str = crate::slug::ROOT;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub id: i64,
    pub uuid: String,
    pub name: String,
    pub created_at: i64,
    pub retired_at: Option<i64>,
}

const COLUMNS: &str = "id, uuid, name, created_at, retired_at";

fn from_row(row: &Row) -> rusqlite::Result<Project> {
    Ok(Project {
        id: row.get(0)?,
        uuid: row.get(1)?,
        name: row.get(2)?,
        created_at: row.get(3)?,
        retired_at: row.get(4)?,
    })
}

pub fn validate_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !valid {
        bail!("invalid project name {name:?}: use only letters, digits, '_' and '-'");
    }
    Ok(())
}

fn ensure_name_free(conn: &Connection, name: &str) -> Result<()> {
    if find_by_name(conn, name)?.is_some() {
        bail!("a project named {name:?} already exists");
    }
    Ok(())
}

/// Creates a project and its root task, and writes the `.ergon` marker into
/// `dir`. Nothing is created if any step fails.
pub fn init(conn: &mut Connection, name: &str, root_intent: &str, dir: &Path) -> Result<Project> {
    validate_name(name)?;
    if root_intent.trim().is_empty() {
        bail!("the root intent can't be empty");
    }
    let marker = dir.join(MARKER);
    if marker.exists() {
        bail!("{} already exists", marker.display());
    }
    ensure_name_free(conn, name)?;

    let now = crate::now();
    let uuid = uuid::Uuid::new_v4().to_string();
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO projects (uuid, name, created_at) VALUES (?1, ?2, ?3)",
        params![uuid, name, now],
    )?;
    let id = tx.last_insert_rowid();
    tx.execute(
        "INSERT INTO tasks (project_id, slug, intent, status, created_at, updated_at)
         VALUES (?1, ?2, ?3, 'in_progress', ?4, ?4)",
        params![id, ROOT_SLUG, root_intent, now],
    )?;

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
        .with_context(|| format!("creating {}", marker.display()))?;
    let written = writeln!(file, "{uuid}").and_then(|_| file.sync_all());
    if let Err(e) = written
        .map_err(anyhow::Error::from)
        .and_then(|_| Ok(tx.commit()?))
    {
        let _ = std::fs::remove_file(&marker);
        return Err(e.context(format!("creating project {name:?}")));
    }

    Ok(Project {
        id,
        uuid,
        name: name.to_string(),
        created_at: now,
        retired_at: None,
    })
}

pub fn find_by_name(conn: &Connection, name: &str) -> Result<Option<Project>> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM projects WHERE name = ?1"),
            [name],
            from_row,
        )
        .optional()?)
}

pub fn by_name(conn: &Connection, name: &str) -> Result<Project> {
    find_by_name(conn, name)?.with_context(|| format!("no project named {name:?}"))
}

pub fn by_uuid(conn: &Connection, uuid: &str) -> Result<Project> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM projects WHERE uuid = ?1"),
        [uuid],
        from_row,
    )
    .optional()?
    .with_context(|| format!("no project with UUID {uuid} in this database"))
}

/// Projects sorted by name; retired ones only if `include_retired`.
pub fn list(conn: &Connection, include_retired: bool) -> Result<Vec<Project>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM projects
         WHERE ?1 OR retired_at IS NULL
         ORDER BY name"
    ))?;
    let rows = stmt.query_map([include_retired], from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn rename(conn: &Connection, old: &str, new: &str) -> Result<()> {
    validate_name(new)?;
    let project = by_name(conn, old)?;
    if old == new {
        return Ok(());
    }
    ensure_name_free(conn, new)?;
    conn.execute(
        "UPDATE projects SET name = ?1 WHERE id = ?2",
        params![new, project.id],
    )?;
    Ok(())
}

pub fn set_retired(conn: &Connection, name: &str, retired: bool) -> Result<()> {
    let project = by_name(conn, name)?;
    match (project.retired_at.is_some(), retired) {
        (true, true) => bail!("project {name:?} is already retired"),
        (false, false) => bail!("project {name:?} isn't retired"),
        _ => {}
    }
    let retired_at = retired.then(crate::now);
    conn.execute(
        "UPDATE projects SET retired_at = ?1 WHERE id = ?2",
        params![retired_at, project.id],
    )?;
    Ok(())
}

/// The nearest `.ergon` file in `start` or one of its ancestors.
pub fn find_marker(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .map(|dir| dir.join(MARKER))
        .find(|marker| marker.is_file())
}

/// The project named by `--project`, or else by the nearest `.ergon` file
/// above `cwd`.
pub fn current(conn: &Connection, flag: Option<&str>, cwd: &Path) -> Result<Project> {
    if let Some(name) = flag {
        return by_name(conn, name);
    }
    let Some(marker) = find_marker(cwd) else {
        bail!(
            "not in an ergon project (no {MARKER} file found); use --project or run `ergon init`"
        );
    };
    let contents = std::fs::read_to_string(&marker)
        .with_context(|| format!("reading {}", marker.display()))?;
    by_uuid(conn, contents.trim()).with_context(|| format!("resolving {}", marker.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn setup() -> (Connection, tempfile::TempDir) {
        (db::open_in_memory().unwrap(), tempfile::tempdir().unwrap())
    }

    #[test]
    fn validates_names() {
        for ok in ["ergon", "my-proj_2", "A"] {
            validate_name(ok).unwrap();
        }
        for bad in ["", "has space", "slash/name", "dot.name", "ünï"] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn init_creates_project_root_and_marker() {
        let (mut conn, dir) = setup();
        let p = init(&mut conn, "ergon", "Build ergon\n\nDetails", dir.path()).unwrap();

        let marker = std::fs::read_to_string(dir.path().join(MARKER)).unwrap();
        assert_eq!(marker.trim(), p.uuid);

        let (slug, status, parent): (String, String, Option<i64>) = conn
            .query_row(
                "SELECT slug, status, parent_id FROM tasks WHERE project_id = ?1",
                [p.id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (slug.as_str(), status.as_str(), parent),
            ("root", "in_progress", None)
        );
    }

    #[test]
    fn init_refuses_existing_marker_duplicate_name_and_empty_intent() {
        let (mut conn, dir) = setup();
        init(&mut conn, "a", "x", dir.path()).unwrap();
        let err = init(&mut conn, "b", "x", dir.path()).unwrap_err();
        assert!(err.to_string().contains("already exists"));

        let other = tempfile::tempdir().unwrap();
        let err = init(&mut conn, "a", "x", other.path()).unwrap_err();
        assert!(err.to_string().contains("already exists"));
        assert!(init(&mut conn, "c", "  \n", other.path()).is_err());
        assert!(!other.path().join(MARKER).exists());
    }

    #[test]
    fn init_rolls_back_when_marker_unwritable() {
        let (mut conn, dir) = setup();
        let missing = dir.path().join("does-not-exist");
        assert!(init(&mut conn, "a", "x", &missing).is_err());
        assert!(find_by_name(&conn, "a").unwrap().is_none());
    }

    #[test]
    fn rename_and_lookup() {
        let (mut conn, dir) = setup();
        let p = init(&mut conn, "old", "x", dir.path()).unwrap();
        let other = tempfile::tempdir().unwrap();
        init(&mut conn, "taken", "x", other.path()).unwrap();

        assert!(rename(&conn, "old", "taken").is_err());
        assert!(rename(&conn, "old", "bad name").is_err());
        rename(&conn, "old", "new").unwrap();
        assert!(find_by_name(&conn, "old").unwrap().is_none());
        assert_eq!(by_uuid(&conn, &p.uuid).unwrap().name, "new");
    }

    #[test]
    fn retire_hides_from_list() {
        let (mut conn, dir) = setup();
        init(&mut conn, "a", "x", dir.path()).unwrap();
        let other = tempfile::tempdir().unwrap();
        init(&mut conn, "b", "x", other.path()).unwrap();

        set_retired(&conn, "a", true).unwrap();
        assert!(set_retired(&conn, "a", true).is_err());
        let names = |all| {
            list(&conn, all)
                .unwrap()
                .into_iter()
                .map(|p| p.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(false), ["b"]);
        assert_eq!(names(true), ["a", "b"]);

        set_retired(&conn, "a", false).unwrap();
        assert!(set_retired(&conn, "a", false).is_err());
        assert_eq!(names(false), ["a", "b"]);
    }

    #[test]
    fn current_project_resolution() {
        let (mut conn, dir) = setup();
        let p = init(&mut conn, "outer", "x", dir.path()).unwrap();
        let nested = dir.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(current(&conn, None, &nested).unwrap(), p);

        let inner = init(&mut conn, "inner", "x", &dir.path().join("a")).unwrap();
        assert_eq!(current(&conn, None, &nested).unwrap(), inner);
        assert_eq!(current(&conn, Some("outer"), &nested).unwrap(), p);
        assert!(current(&conn, Some("nope"), &nested).is_err());

        let outside = tempfile::tempdir().unwrap();
        assert!(current(&conn, None, outside.path()).is_err());
    }

    #[test]
    fn current_with_unknown_uuid() {
        let (conn, dir) = setup();
        std::fs::write(dir.path().join(MARKER), "not-a-known-uuid\n").unwrap();
        let err = format!("{:#}", current(&conn, None, dir.path()).unwrap_err());
        assert!(err.contains("not-a-known-uuid"), "{err}");
    }
}
