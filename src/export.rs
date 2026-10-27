//! Export and import of whole projects as JSON, independent of the database
//! schema: for backups, and for moving projects between databases. The
//! format has its own version, and later ergons keep reading older formats.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::project::{self, Project};
use crate::slug;
use crate::task::{Status, Tasks, parse_attention, parse_size};

/// The format this ergon writes and the newest it reads. Format 2 added
/// sizes and format 3 attention; an older ergon would drop them, so it must
/// refuse the export.
const FORMAT: u32 = 3;

#[derive(Serialize, Deserialize)]
struct Export {
    format: u32,
    projects: Vec<ProjectData>,
}

#[derive(Serialize, Deserialize)]
struct ProjectData {
    uuid: String,
    name: String,
    created_at: i64,
    retired_at: Option<i64>,
    /// Every task, deleted ones included, oldest first.
    tasks: Vec<TaskData>,
    deps: Vec<Dep>,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
struct Dep {
    task: String,
    depends_on: String,
}

#[derive(Serialize, Deserialize)]
struct TaskData {
    slug: String,
    parent: Option<String>,
    intent: String,
    status: Option<Status>,
    priority: Option<u8>,
    #[serde(default)]
    size: Option<u32>,
    #[serde(default)]
    attention: Option<u8>,
    claude_session: Option<String>,
    claude_cwd: Option<String>,
    created_at: i64,
    updated_at: i64,
    deleted_at: Option<i64>,
    position: i64,
}

/// The projects, in full, as pretty-printed JSON.
pub fn export(conn: &Connection, projects: &[Project]) -> Result<String> {
    let mut out = Export {
        format: FORMAT,
        projects: Vec::new(),
    };
    for p in projects {
        let tasks = Tasks::load(conn, p)?;
        let slug = |id: i64| tasks.get(id).slug.clone();
        let mut deps: Vec<Dep> = tasks
            .deps
            .iter()
            .map(|&(t, d)| Dep {
                task: slug(t),
                depends_on: slug(d),
            })
            .collect();
        deps.sort();
        out.projects.push(ProjectData {
            uuid: p.uuid.clone(),
            name: p.name.clone(),
            created_at: p.created_at,
            retired_at: p.retired_at,
            tasks: tasks
                .all()
                .into_iter()
                .map(|t| TaskData {
                    slug: t.slug.clone(),
                    parent: t.parent_id.map(slug),
                    intent: t.intent.clone(),
                    status: t.status,
                    priority: t.priority,
                    size: t.size,
                    attention: t.attention,
                    claude_session: t.claude_session.clone(),
                    claude_cwd: t.claude_cwd.clone(),
                    created_at: t.created_at,
                    updated_at: t.updated_at,
                    deleted_at: t.deleted_at,
                    position: t.position,
                })
                .collect(),
            deps,
        });
    }
    Ok(serde_json::to_string_pretty(&out)? + "\n")
}

/// Adds the exported projects to the database, all or none, returning each
/// one's name and task count. Projects already in the database are refused.
pub fn import(conn: &mut Connection, json: &str) -> Result<Vec<(String, usize)>> {
    let format: u32 = serde_json::from_str::<serde_json::Value>(json)
        .context("reading the export")?
        .get("format")
        .and_then(|f| f.as_u64())
        .context("reading the export: no format version")? as u32;
    if format > FORMAT {
        bail!(
            "the export is in format {format}, newer than this ergon reads ({FORMAT}); upgrade ergon"
        );
    }
    let export: Export = serde_json::from_str(json).context("reading the export")?;

    let tx = conn.transaction()?;
    let mut imported = Vec::new();
    for p in &export.projects {
        import_project(&tx, p).with_context(|| format!("importing project {}", p.name))?;
        imported.push((p.name.clone(), p.tasks.len()));
    }
    tx.commit()?;
    Ok(imported)
}

fn import_project(conn: &Connection, p: &ProjectData) -> Result<()> {
    project::validate_name(&p.name)?;
    if project::find_by_name(conn, &p.name)?.is_some() {
        bail!("a project named {} already exists", p.name);
    }
    if project::by_uuid(conn, &p.uuid).is_ok() {
        bail!("this database already has a project with UUID {}", p.uuid);
    }
    conn.execute(
        "INSERT INTO projects (uuid, name, created_at, retired_at) VALUES (?1, ?2, ?3, ?4)",
        params![p.uuid, p.name, p.created_at, p.retired_at],
    )?;
    let project_id = conn.last_insert_rowid();

    for t in &p.tasks {
        if !slug::is_valid(&t.slug) {
            bail!("invalid task slug {:?}", t.slug);
        }
        if let Some(n) = t.size {
            parse_size(&n.to_string()).with_context(|| format!("task {}", t.slug))?;
        }
        if let Some(n) = t.attention {
            parse_attention(&n.to_string()).with_context(|| format!("task {}", t.slug))?;
        }
    }
    match p.tasks.iter().filter(|t| t.parent.is_none()).count() {
        1 => {}
        n => bail!("expected one root task, found {n}"),
    }

    // Ids in export order, so tasks created in the same second keep their
    // order. A child may come before its parent, so foreign keys are only
    // checked at commit.
    conn.pragma_update(None, "defer_foreign_keys", true)?;
    let base: i64 = conn.query_row("SELECT COALESCE(MAX(id), 0) FROM tasks", [], |r| r.get(0))?;
    let mut ids: HashMap<&str, i64> = HashMap::new();
    for (i, t) in p.tasks.iter().enumerate() {
        if ids.insert(&t.slug, base + 1 + i as i64).is_some() {
            bail!("task {} appears twice", t.slug);
        }
    }
    let parents: HashMap<&str, &str> = p
        .tasks
        .iter()
        .filter_map(|t| Some((t.slug.as_str(), t.parent.as_deref()?)))
        .collect();
    for t in &p.tasks {
        let mut cur = t.slug.as_str();
        for _ in 0..=p.tasks.len() {
            match parents.get(cur) {
                None => break,
                Some(parent) if !ids.contains_key(parent) => {
                    bail!("task {cur}'s parent {parent} isn't in the export")
                }
                Some(parent) => cur = parent,
            }
        }
        if parents.contains_key(cur) {
            bail!("task {} is its own ancestor", t.slug);
        }
    }

    for t in &p.tasks {
        conn.execute(
            "INSERT INTO tasks (id, project_id, slug, parent_id, intent, status, priority,
               claude_session, claude_cwd, created_at, updated_at, deleted_at, position, size,
               attention)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                ids[t.slug.as_str()],
                project_id,
                t.slug,
                t.parent.as_deref().map(|p| ids[p]),
                t.intent,
                t.status,
                t.priority,
                t.claude_session,
                t.claude_cwd,
                t.created_at,
                t.updated_at,
                t.deleted_at,
                t.position,
                t.size,
                t.attention
            ],
        )
        .with_context(|| format!("adding task {}", t.slug))?;
    }

    for Dep { task, depends_on } in &p.deps {
        let (Some(t), Some(d)) = (ids.get(task.as_str()), ids.get(depends_on.as_str())) else {
            bail!("dependency {task} -> {depends_on} names a task that isn't in the export");
        };
        conn.execute(
            "INSERT INTO task_deps (task_id, depends_on) VALUES (?1, ?2)",
            params![t, d],
        )?;
    }

    let project = project::by_uuid(conn, &p.uuid)?;
    Tasks::load(conn, &project)?.validate()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::ops::{self, Fields};

    fn all(conn: &Connection) -> Vec<Project> {
        project::list(conn, true).unwrap()
    }

    #[test]
    fn round_trips() {
        let mut conn = db::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let p = project::init(&mut conn, "alpha", "Alpha\n\nContext.", dir.path()).unwrap();
        let a = ops::add(
            &mut conn,
            &p,
            None,
            "A",
            Fields {
                priority: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        let b = ops::add(
            &mut conn,
            &p,
            Some(&a),
            "B",
            Fields {
                status: Some(Status::InProgress),
                ..Default::default()
            },
        )
        .unwrap();
        let c = ops::add(
            &mut conn,
            &p,
            None,
            "C",
            Fields {
                attention: Some(7),
                ..Default::default()
            },
        )
        .unwrap();
        let d = ops::add(&mut conn, &p, Some(&c), "D", Fields::default()).unwrap();
        let e = ops::add(
            &mut conn,
            &p,
            None,
            "E",
            Fields {
                size: Some(3),
                ..Default::default()
            },
        )
        .unwrap();
        ops::dep_add(&mut conn, &p, &c, &e).unwrap();
        ops::rm(&mut conn, &p, &d, 0, 0).unwrap();
        // A child created before its parent: import must still order them.
        ops::mv(&mut conn, &p, &a, &c).unwrap();
        conn.execute(
            "UPDATE tasks SET claude_session = 's', claude_cwd = '/w' WHERE slug = ?1",
            [&b],
        )
        .unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        project::init(&mut conn, "beta", "Beta", dir2.path()).unwrap();
        project::set_retired(&conn, "beta", true).unwrap();

        let json = export(&conn, &all(&conn)).unwrap();
        let mut fresh = db::open_in_memory().unwrap();
        let imported = import(&mut fresh, &json).unwrap();
        assert_eq!(imported, [("alpha".into(), 6), ("beta".into(), 1)]);
        assert_eq!(export(&fresh, &all(&fresh)).unwrap(), json);
    }

    #[test]
    fn refuses_bad_exports_entirely() {
        let mut conn = db::open_in_memory().unwrap();
        let newer = r#"{"format": 4, "projects": []}"#;
        let err = import(&mut conn, newer).unwrap_err().to_string();
        assert!(err.contains("newer than this ergon reads"), "{err}");

        let task = |slug: &str, parent: Option<&str>| {
            serde_json::json!({
                "slug": slug, "parent": parent, "intent": "x", "status": "todo",
                "priority": null, "claude_session": null, "claude_cwd": null,
                "created_at": 0, "updated_at": 0, "deleted_at": null, "position": 0
            })
        };
        let project = |name: &str, uuid: &str, tasks| {
            serde_json::json!({
                "uuid": uuid, "name": name, "created_at": 0, "retired_at": null,
                "tasks": tasks, "deps": []
            })
        };
        let cases = [
            (
                vec![task("root", None), task("Abcd", Some("Efgh"))],
                "parent Efgh isn't in the export",
            ),
            (
                vec![
                    task("root", None),
                    task("Abcd", Some("Efgh")),
                    task("Efgh", Some("Abcd")),
                ],
                "is its own ancestor",
            ),
            (
                vec![task("root", None), task("nope!", Some("root"))],
                "invalid task slug",
            ),
            (
                vec![task("Abcd", Some("root"))],
                "expected one root task, found 0",
            ),
            (
                {
                    let mut t = task("Abcd", Some("root"));
                    t["size"] = 4.into();
                    vec![task("root", None), t]
                },
                "invalid size \"4\"",
            ),
        ];
        for (tasks, want) in cases {
            // A good project first, to check that nothing is kept.
            let json = serde_json::json!({
                "format": 1,
                "projects": [project("good", "u1", vec![task("root", None)]), project("bad", "u2", tasks)],
            })
            .to_string();
            let err = format!("{:#}", import(&mut conn, &json).unwrap_err());
            assert!(err.contains(want), "{err}");
            assert!(all(&conn).is_empty());
        }
    }
}
