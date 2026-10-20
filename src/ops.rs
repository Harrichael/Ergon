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

/// Creates a task and returns its slug. The parent defaults to the root, and
/// a direct child of the root defaults to `todo`.
pub fn add(
    conn: &mut Connection,
    project: &Project,
    parent: Option<&str>,
    intent: &str,
    status: Option<Status>,
    priority: Option<u8>,
) -> Result<String> {
    let intent = clean_intent(intent)?;
    let tx = conn.transaction()?;
    let tasks = Tasks::load(&tx, project)?;
    let parent = match parent {
        Some(s) => tasks.live(s)?,
        None => tasks.root(),
    };
    let status = status.or(parent.is_root().then_some(Status::Todo));
    let slug = (0..1000)
        .map(|_| slug::random())
        .find(|s| tasks.find(s).is_err())
        .context("couldn't find an unused slug")?;
    let now = crate::now();
    tx.execute(
        "INSERT INTO tasks (project_id, slug, parent_id, intent, status, priority, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
        params![project.id, slug, parent.id, intent, status, priority, now],
    )?;
    commit_valid(tx, project)?;
    Ok(slug)
}

/// Fields to change; `None` leaves a field as it is. For status and priority,
/// `Some(None)` unsets the field.
#[derive(Debug, Default)]
pub struct Changes {
    pub intent: Option<String>,
    pub status: Option<Option<Status>>,
    pub priority: Option<Option<u8>>,
}

/// Applies `changes` to a task. Cancelling a task cancels its descendants
/// that have a stored, unfinished status; their references are returned.
pub fn edit(
    conn: &mut Connection,
    project: &Project,
    slug: &str,
    changes: Changes,
) -> Result<Vec<String>> {
    if changes.intent.is_none() && changes.status.is_none() && changes.priority.is_none() {
        bail!("nothing to change: pass --intent, --status or --priority");
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
    if (&intent, status, priority) == (&task.intent, task.status, task.priority) {
        return Ok(Vec::new());
    }

    let now = crate::now();
    tx.execute(
        "UPDATE tasks SET intent = ?1, status = ?2, priority = ?3, updated_at = ?4 WHERE id = ?5",
        params![intent, status, priority, now, task.id],
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

/// Moves a task, with its subtree, under a new parent. A task without a
/// status that moves to the top level gets `todo`.
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
        "UPDATE tasks SET parent_id = ?1 WHERE id = ?2",
        params![parent.id, task.id],
    )?;
    if parent.is_root() && task.status.is_none() {
        tx.execute(
            "UPDATE tasks SET status = 'todo', updated_at = ?1 WHERE id = ?2",
            params![crate::now(), task.id],
        )?;
    }
    commit_valid(tx, project)
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
            add(&mut self.conn, &self.project, parent, "Task", status, None).unwrap()
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

        fn stored(&self, slug: &str) -> Option<Status> {
            self.tasks().live(slug).unwrap().status
        }

        fn effective(&self, slug: &str) -> Status {
            let tasks = self.tasks();
            tasks.effective_status(tasks.live(slug).unwrap().id)
        }
    }

    #[test]
    fn top_level_tasks_default_to_todo() {
        let mut f = Fixture::new();
        let top = f.add(None, None);
        let child = f.add(Some(&top), None);
        assert_eq!(f.stored(&top), Some(Todo));
        assert_eq!(f.stored(&child), None);
        assert_eq!(f.effective(&child), Todo);

        let landing = f.add(None, Some(Landing));
        assert_eq!(f.stored(&landing), Some(Landing));

        let err = f.set(&top, None).unwrap_err();
        assert!(format!("{err:#}").contains("top-level"), "{err:#}");
    }

    #[test]
    fn add_validates_input() {
        let mut f = Fixture::new();
        assert!(add(&mut f.conn, &f.project, None, "  ", None, None).is_err());
        assert!(add(&mut f.conn, &f.project, Some("AAAA"), "x", None, None).is_err());
        let slug = add(
            &mut f.conn,
            &f.project,
            None,
            "  Title\n\nBody \n",
            None,
            Some(1),
        )
        .unwrap();
        let tasks = f.tasks();
        let t = tasks.live(&slug).unwrap();
        assert_eq!((t.intent.as_str(), t.priority), ("Title\n\nBody", Some(1)));
    }

    #[test]
    fn done_parent_requires_closed_descendants() {
        let mut f = Fixture::new();
        let parent = f.add(None, None);
        let child = f.add(Some(&parent), Some(Todo));
        let grandchild = f.add(Some(&child), None);

        let err = f.set(&parent, Some(Done)).unwrap_err();
        assert!(format!("{err:#}").contains("is done"), "{err:#}");

        f.set(&child, Some(Done)).unwrap();
        f.set(&parent, Some(Done)).unwrap();
        assert_eq!(f.effective(&grandchild), Done);

        // Adding or reopening anything open underneath is refused.
        let err = add(&mut f.conn, &f.project, Some(&child), "x", Some(Todo), None).unwrap_err();
        assert!(format!("{err:#}").contains("can't be todo"), "{err:#}");
        assert!(f.set(&grandchild, Some(InProgress)).is_err());
        assert!(f.set(&child, None).is_ok(), "inheriting done is fine");
        // An unset status under a done parent inherits done, so it's allowed.
        f.add(Some(&child), None);
        f.add(Some(&child), Some(Cancelled));
    }

    #[test]
    fn done_parent_checks_through_cancelled_children() {
        let mut f = Fixture::new();
        let parent = f.add(None, None);
        let child = f.add(Some(&parent), Some(Cancelled));
        let grandchild = f.add(Some(&child), Some(Todo));
        let err = f.set(&parent, Some(Done)).unwrap_err();
        assert!(format!("{err:#}").contains(&grandchild), "{err:#}");
    }

    #[test]
    fn landing_parent_requires_landed_descendants() {
        let mut f = Fixture::new();
        let parent = f.add(None, None);
        let child = f.add(Some(&parent), Some(InProgress));
        assert!(f.set(&parent, Some(Landing)).is_err());
        f.set(&child, Some(Landing)).unwrap();
        f.set(&parent, Some(Landing)).unwrap();
        assert!(f.set(&child, Some(Todo)).is_err());
        f.set(&child, Some(Done)).unwrap();
    }

    #[test]
    fn cancel_cascades_to_unfinished_descendants() {
        let mut f = Fixture::new();
        let parent = f.add(None, None);
        let todo = f.add(Some(&parent), Some(Todo));
        let landing = f.add(Some(&todo), Some(Landing));
        let done = f.add(Some(&parent), Some(Done));
        let unset = f.add(Some(&parent), None);

        let mut cancelled = f.set(&parent, Some(Cancelled)).unwrap();
        cancelled.sort();
        let mut expected = vec![format!("p/{todo}"), format!("p/{landing}")];
        expected.sort();
        assert_eq!(cancelled, expected);
        assert_eq!(f.stored(&done), Some(Done));
        assert_eq!(f.stored(&unset), None);
        assert_eq!(f.effective(&unset), Cancelled);
    }

    #[test]
    fn root_rules() {
        let mut f = Fixture::new();
        assert!(f.set("root", None).is_err());
        assert!(f.set("root", Some(Cancelled)).is_err());
        f.set("root", Some(Todo)).unwrap();
        let t = f.add(None, None);
        assert!(mv(&mut f.conn, &f.project, "root", &t).is_err());
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

    #[test]
    fn move_rules() {
        let mut f = Fixture::new();
        let a = f.add(None, None);
        let b = f.add(Some(&a), None);
        let c = f.add(Some(&b), None);

        assert!(mv(&mut f.conn, &f.project, &a, &a).is_err());
        assert!(mv(&mut f.conn, &f.project, &a, &c).is_err());

        // Moving to the top level pins a status.
        mv(&mut f.conn, &f.project, &b, "root").unwrap();
        assert_eq!(f.stored(&b), Some(Todo));
        assert_eq!(f.effective(&c), Todo);

        // Moving open work under a done task is refused.
        let done = f.add(None, Some(Done));
        let err = mv(&mut f.conn, &f.project, &b, &done).unwrap_err();
        assert!(format!("{err:#}").contains("is done"), "{err:#}");
        mv(&mut f.conn, &f.project, &c, &a).unwrap();
        assert_eq!(
            f.tasks().live(&c).unwrap().parent_id,
            Some(f.tasks().live(&a).unwrap().id)
        );
    }
}
