//! Changes to tasks. Each runs in a transaction and is committed only if the
//! resulting project still satisfies every rule in `Tasks::validate`.

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, Transaction, params};

use crate::project::Project;
use crate::slug;
use crate::task::{Status, Tasks};

fn clean_intent(intent: &str) -> Result<String> {
    let intent = intent.trim();
    if intent.is_empty() {
        bail!("the intent can't be empty");
    }
    Ok(intent.to_string())
}

fn commit_valid(tx: Transaction, project: &Project) -> Result<()> {
    Tasks::load(&tx, project)?.validate().context("refused")?;
    tx.commit()?;
    Ok(())
}

/// The position that puts a new child of `parent` after its siblings.
fn next_position(conn: &Connection, parent: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(position) + 1, 0) FROM tasks WHERE parent_id = ?1",
        [parent],
        |r| r.get(0),
    )?)
}

/// A new task's stored fields besides its intent; each may be left unset.
#[derive(Debug, Default)]
pub struct Fields {
    pub status: Option<Status>,
    pub priority: Option<u8>,
    pub size: Option<u32>,
    pub attention: Option<u8>,
}

/// Creates a task and returns its slug. The parent defaults to the root, and
/// a direct child of the root defaults to `todo`.
pub fn add(
    conn: &mut Connection,
    project: &Project,
    parent: Option<&str>,
    intent: &str,
    fields: Fields,
) -> Result<String> {
    let Fields {
        status,
        priority,
        size,
        attention,
    } = fields;
    let intent = clean_intent(intent)?;
    let tx = conn.transaction()?;
    let tasks = Tasks::load(&tx, project)?;
    let parent = match parent {
        Some(s) => tasks.live(s)?,
        None => tasks.root(),
    };
    let status = status.or(parent.is_root().then_some(Status::Todo));
    let mut rng = crate::rng(0);
    let slug = (0..1000)
        .map(|_| slug::random(&mut rng))
        .find(|s| tasks.find(s).is_err())
        .context("couldn't find an unused slug")?;
    let now = crate::now();
    tx.execute(
        "INSERT INTO tasks (project_id, slug, parent_id, intent, status, priority, size, attention, created_at, updated_at, position)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, ?10)",
        params![project.id, slug, parent.id, intent, status, priority, size, attention, now, next_position(&tx, parent.id)?],
    )?;
    commit_valid(tx, project)?;
    Ok(slug)
}

/// Fields to change; `None` leaves a field as it is. For status, priority,
/// size and attention, `Some(None)` unsets the field.
#[derive(Debug, Default)]
pub struct Changes {
    pub intent: Option<String>,
    pub status: Option<Option<Status>>,
    pub priority: Option<Option<u8>>,
    pub size: Option<Option<u32>>,
    pub attention: Option<Option<u8>>,
}

/// Applies `changes` to a task. Cancelling a task cancels its descendants
/// that have a stored, unfinished status; their references are returned.
pub fn edit(
    conn: &mut Connection,
    project: &Project,
    slug: &str,
    changes: Changes,
) -> Result<Vec<String>> {
    if changes.intent.is_none()
        && changes.status.is_none()
        && changes.priority.is_none()
        && changes.size.is_none()
        && changes.attention.is_none()
    {
        bail!("nothing to change: pass --intent, --status, --priority, --size or --attention");
    }
    let tx = conn.transaction()?;
    let tasks = Tasks::load(&tx, project)?;
    let task = tasks.live(slug)?;

    let intent = match changes.intent {
        Some(i) => clean_intent(&i)?,
        None => task.intent.clone(),
    };
    let status = changes.status.unwrap_or(task.status);
    let priority = changes.priority.unwrap_or(task.priority);
    let size = changes.size.unwrap_or(task.size);
    let attention = changes.attention.unwrap_or(task.attention);
    if (&intent, status, priority, size, attention)
        == (
            &task.intent,
            task.status,
            task.priority,
            task.size,
            task.attention,
        )
    {
        return Ok(Vec::new());
    }

    let now = crate::now();
    tx.execute(
        "UPDATE tasks SET intent = ?1, status = ?2, priority = ?3, size = ?4, attention = ?5,
           updated_at = ?6
         WHERE id = ?7",
        params![intent, status, priority, size, attention, now, task.id],
    )?;

    let mut cancelled = Vec::new();
    if status == Some(Status::Cancelled) && task.status != Some(Status::Cancelled) {
        for d in tasks.descendants(task.id) {
            let unfinished = matches!(
                tasks.get(d).status,
                Some(Status::Todo | Status::InProgress | Status::Landing)
            );
            if unfinished {
                tx.execute(
                    "UPDATE tasks SET status = 'cancelled', updated_at = ?1 WHERE id = ?2",
                    params![now, d],
                )?;
                cancelled.push(tasks.reference(d));
            }
        }
    }

    commit_valid(tx, project)?;
    Ok(cancelled)
}

/// Moves a task, with its subtree, under a new parent, after its new
/// siblings. A task without a status that moves to the top level gets `todo`.
pub fn mv(conn: &mut Connection, project: &Project, slug: &str, new_parent: &str) -> Result<()> {
    let tx = conn.transaction()?;
    let tasks = Tasks::load(&tx, project)?;
    let task = tasks.live(slug)?;
    let parent = tasks.live(new_parent)?;
    if task.is_root() {
        bail!("the root task can't be moved");
    }
    if parent.id == task.id || tasks.is_ancestor(task.id, parent.id) {
        bail!(
            "can't move {} under itself or one of its subtasks",
            tasks.reference(task.id)
        );
    }
    if task.parent_id == Some(parent.id) {
        return Ok(());
    }

    tx.execute(
        "UPDATE tasks SET parent_id = ?1, position = ?2 WHERE id = ?3",
        params![parent.id, next_position(&tx, parent.id)?, task.id],
    )?;
    if parent.is_root() && task.status.is_none() {
        tx.execute(
            "UPDATE tasks SET status = 'todo', updated_at = ?1 WHERE id = ?2",
            params![crate::now(), task.id],
        )?;
    }
    commit_valid(tx, project)
}

/// Records that `task` depends on `depends_on`.
pub fn dep_add(
    conn: &mut Connection,
    project: &Project,
    task: &str,
    depends_on: &str,
) -> Result<()> {
    let tx = conn.transaction()?;
    let tasks = Tasks::load(&tx, project)?;
    let (t, d) = (tasks.live(task)?, tasks.live(depends_on)?);
    if tasks.deps.contains(&(t.id, d.id)) {
        bail!(
            "{} already depends on {}",
            tasks.reference(t.id),
            tasks.reference(d.id)
        );
    }
    tx.execute(
        "INSERT INTO task_deps (task_id, depends_on) VALUES (?1, ?2)",
        params![t.id, d.id],
    )?;
    commit_valid(tx, project)
}

/// Removes the dependency of `task` on `depends_on`.
pub fn dep_rm(
    conn: &mut Connection,
    project: &Project,
    task: &str,
    depends_on: &str,
) -> Result<()> {
    let tasks = Tasks::load(conn, project)?;
    let (t, d) = (tasks.live(task)?, tasks.live(depends_on)?);
    let removed = conn.execute(
        "DELETE FROM task_deps WHERE task_id = ?1 AND depends_on = ?2",
        params![t.id, d.id],
    )?;
    if removed == 0 {
        bail!(
            "{} doesn't depend on {}",
            tasks.reference(t.id),
            tasks.reference(d.id)
        );
    }
    Ok(())
}

/// Soft-deletes a task and its subtree. `subtasks` and `dependents` must
/// match the real counts, as a guard against deleting more than intended.
pub fn rm(
    conn: &mut Connection,
    project: &Project,
    slug: &str,
    subtasks: usize,
    dependents: usize,
) -> Result<()> {
    let tx = conn.transaction()?;
    let tasks = Tasks::load(&tx, project)?;
    let task = tasks.live(slug)?;
    if task.is_root() {
        bail!("the root task can't be deleted");
    }
    let descendants = tasks.descendants(task.id);
    let actual = (descendants.len(), tasks.outside_dependents(task.id).len());
    if (subtasks, dependents) != actual {
        bail!(
            "refused: the counts don't match; to delete {r}, run: \
             ergon rm {r} --subtasks {} --dependents {}",
            actual.0,
            actual.1,
            r = tasks.reference(task.id),
        );
    }
    let now = crate::now();
    for id in std::iter::once(task.id).chain(descendants) {
        tx.execute(
            "UPDATE tasks SET deleted_at = ?1 WHERE id = ?2",
            params![now, id],
        )?;
    }
    commit_valid(tx, project)
}

/// Restores a deleted task, its deleted ancestors, and the descendants that
/// were deleted along with it. Returns the references restored.
pub fn restore(conn: &mut Connection, project: &Project, slug: &str) -> Result<Vec<String>> {
    let tx = conn.transaction()?;
    let tasks = Tasks::load(&tx, project)?;
    let task = tasks.find(slug)?;
    let Some(deleted_at) = task.deleted_at else {
        bail!("{} isn't deleted", tasks.reference(task.id));
    };
    let ancestors = tasks.ancestors(task.id).into_iter().rev();
    let same_rm = tasks
        .all_descendants(task.id)
        .into_iter()
        .filter(|&d| tasks.get(d).deleted_at == Some(deleted_at));
    let restored: Vec<i64> = ancestors
        .filter(|&a| tasks.get(a).is_deleted())
        .chain(std::iter::once(task.id))
        .chain(same_rm)
        .collect();
    for &id in &restored {
        tx.execute("UPDATE tasks SET deleted_at = NULL WHERE id = ?1", [id])?;
    }
    commit_valid(tx, project)?;
    Ok(restored.into_iter().map(|id| tasks.reference(id)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{db, project};
    use Status::*;

    struct Fixture {
        conn: Connection,
        project: Project,
        _dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let mut conn = db::open_in_memory().unwrap();
            let dir = tempfile::tempdir().unwrap();
            let project = project::init(&mut conn, "p", "Root", dir.path()).unwrap();
            Fixture {
                conn,
                project,
                _dir: dir,
            }
        }

        fn add(&mut self, parent: Option<&str>, status: Option<Status>) -> String {
            let fields = Fields {
                status,
                ..Default::default()
            };
            add(&mut self.conn, &self.project, parent, "Task", fields).unwrap()
        }

        fn set(&mut self, slug: &str, status: Option<Status>) -> Result<Vec<String>> {
            let changes = Changes {
                status: Some(status),
                ..Default::default()
            };
            edit(&mut self.conn, &self.project, slug, changes)
        }

        fn tasks(&self) -> Tasks {
            Tasks::load(&self.conn, &self.project).unwrap()
        }
    }

    #[test]
    fn edit_updates_timestamp_only_on_change() {
        let mut f = Fixture::new();
        let t = f.add(None, None);
        f.conn
            .execute("UPDATE tasks SET updated_at = 5", [])
            .unwrap();
        let updated = |f: &Fixture| f.tasks().live(&t).unwrap().updated_at;

        f.set(&t, Some(Todo)).unwrap();
        assert_eq!(updated(&f), 5, "no-op edit");
        f.add(Some(&t), None);
        assert_eq!(updated(&f), 5, "adding a child");

        let changes = Changes {
            priority: Some(Some(3)),
            ..Default::default()
        };
        edit(&mut f.conn, &f.project, &t, changes).unwrap();
        assert!(updated(&f) > 5);
        assert!(edit(&mut f.conn, &f.project, &t, Changes::default()).is_err());
    }

    /// Sized tasks never nest, whichever end the change comes from: sizing
    /// a step, sizing the parent of a unit, or moving a unit under another.
    #[test]
    fn sized_tasks_never_nest() {
        let mut f = Fixture::new();
        let sized = |n| Fields {
            size: Some(n),
            ..Default::default()
        };
        let size = |f: &mut Fixture, t: &str, n: Option<u32>| {
            let changes = Changes {
                size: Some(n),
                ..Default::default()
            };
            edit(&mut f.conn, &f.project, t, changes)
        };
        let unit = add(&mut f.conn, &f.project, None, "Unit", sized(5)).unwrap();
        let step = f.add(Some(&unit), None);
        let other = add(&mut f.conn, &f.project, None, "Other", sized(3)).unwrap();

        let err = size(&mut f, &step, Some(2)).unwrap_err();
        assert!(format!("{err:#}").contains(&format!("ergon edit p/{unit} --size none")));
        let parent = f.add(None, None);
        mv(&mut f.conn, &f.project, &unit, &parent).unwrap();
        assert!(size(&mut f, &parent, Some(8)).is_err());
        assert!(mv(&mut f.conn, &f.project, &other, &unit).is_err());

        size(&mut f, &unit, None).unwrap();
        size(&mut f, &step, Some(2)).unwrap();
    }

    fn dep(f: &mut Fixture, t: &str, d: &str) -> Result<()> {
        dep_add(&mut f.conn, &f.project, t, d)
    }

    #[test]
    fn dependencies_stay_in_one_project() {
        let mut f = Fixture::new();
        let a = f.add(None, None);
        let dir = tempfile::tempdir().unwrap();
        let other = project::init(&mut f.conn, "q", "Other", dir.path()).unwrap();
        let x = add(&mut f.conn, &other, None, "x", Fields::default()).unwrap();
        // Slugs resolve within the given project, so the other project's task
        // isn't reachable.
        assert!(dep(&mut f, &a, &x).is_err());
    }

    #[test]
    fn deleted_slugs_are_not_reused() {
        let mut f = Fixture::new();
        let a = f.add(None, None);
        rm(&mut f.conn, &f.project, &a, 0, 0).unwrap();
        // The unique index covers deleted rows, so a collision can't be inserted.
        let dup = f.conn.execute(
            "INSERT INTO tasks (project_id, slug, parent_id, intent, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'x', 0, 0)",
            params![f.project.id, a, f.tasks().root().id],
        );
        assert!(dup.is_err());
    }
}
