//! Choros's land hook: what `choros land` runs before and after each land
//! (`land = "ergon land-hook"` in a trunk's `.choros-config/hooks.toml`),
//! so one command lands the work and moves its tasks.
//!
//! A land carries the tasks its labels name, or with none named, the ones
//! its Claude Code session is recorded on. Before the land, choros is told
//! them all as labels, the most urgent priority among them, and a PR body
//! that names them. After it:
//!
//! - `choros land continue` is a piece of work that goes on: the tasks
//!   being worked on stay as they are, and a subtask of one, carried by
//!   the land, goes to landing.
//! - `choros land archive` is the last of it: every carried task goes to
//!   landing. It is refused while a subtask of one, or another task of the
//!   same session, is still open, since the workspace goes with it.
//!
//! A land can't be taken back, so what would be refused afterwards is
//! refused before.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::derived::Derived;
use crate::project::{self, Project};
use crate::refs::TaskRef;
use crate::task::{Status, Tasks};

#[derive(Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Continue,
    Archive,
}

/// The land, as choros tells it. It says more than ergon reads.
#[derive(Deserialize)]
pub struct Told {
    pub mode: Mode,
    pub labels: Vec<String>,
    pub priority: Option<String>,
    pub title: Option<String>,
    pub body: Option<String>,
    pub session: Option<String>,
}

/// What choros should land with instead.
#[derive(Serialize)]
pub struct Reply {
    pub labels: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

/// One project's part in a land.
struct Part {
    project: Project,
    tasks: Tasks,
    /// In tree order.
    carried: Vec<i64>,
    /// Deepest first, so no task is landing before its subtasks.
    to_landing: Vec<i64>,
    /// Carried subtasks a `continue` leaves alone, each with the open task
    /// under it that says it isn't finished.
    unfinished: Vec<(i64, i64)>,
}

/// The tasks `told` carries, by project, and what becomes of them. Labels
/// that name no project of ours are some other tool's. `gate` refuses an
/// archive with work still open; once the land is made there is nothing
/// left to refuse.
fn parts(conn: &Connection, told: &Told, gate: bool) -> Result<Vec<Part>> {
    let projects = project::list(conn, true)?;
    let mut carried: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    let mut loaded: BTreeMap<i64, (Project, Tasks)> = BTreeMap::new();
    for p in &projects {
        loaded.insert(p.id, (p.clone(), Tasks::load(conn, p)?));
    }
    for label in &told.labels {
        let Ok(r) = TaskRef::exact(label) else {
            continue;
        };
        let Some(p) = projects
            .iter()
            .find(|p| Some(&p.name) == r.project.as_ref())
        else {
            continue;
        };
        let tasks = &loaded[&p.id].1;
        let id = tasks
            .live(&r.slug)
            .with_context(|| format!("--label {label}"))?
            .id;
        // The root is the plan, not work a land carries.
        if id != tasks.root().id {
            carried.entry(p.id).or_default().push(id);
        }
    }
    // An archive takes the workspace, so it is of the session's tasks
    // whatever else it names.
    if (carried.is_empty() || told.mode == Mode::Archive)
        && let Some(session) = told.session.as_deref()
    {
        for (id, (_, tasks)) in &loaded {
            let mine = of_session(tasks, session);
            if !mine.is_empty() {
                carried.entry(*id).or_default().extend(mine);
            }
        }
    }
    carried
        .into_iter()
        .map(|(id, ids)| {
            let (project, tasks) = loaded.remove(&id).expect("loaded above");
            part(project, tasks, ids, told.mode, gate)
        })
        .collect()
}

fn part(
    project: Project,
    tasks: Tasks,
    mut carried: Vec<i64>,
    mode: Mode,
    gate: bool,
) -> Result<Part> {
    let root = tasks.root().id;
    let order = tasks.descendants(root);
    carried.sort_by_key(|id| order.iter().position(|o| o == id));
    carried.dedup();
    let open = |id: i64| {
        matches!(
            tasks.get(id).status,
            Some(Status::Todo | Status::InProgress)
        )
    };
    // A closed task's work is already in.
    let movable = |id: i64| !tasks.effective_status(id).is_closed();

    let mut unfinished = Vec::new();
    let mut to_landing: Vec<i64> = match mode {
        Mode::Continue => carried
            .iter()
            .copied()
            // A step of work under way, not a task with a session of its
            // own: that one is still being worked on.
            .filter(|&id| {
                tasks.get(id).claude_session.is_none()
                    && tasks
                        .ancestors(id)
                        .iter()
                        .any(|&a| a != root && tasks.get(a).claude_session.is_some())
            })
            .filter(
                |&id| match tasks.descendants(id).into_iter().find(|&d| open(d)) {
                    Some(d) => {
                        unfinished.push((id, d));
                        false
                    }
                    None => true,
                },
            )
            .collect(),
        Mode::Archive => {
            let mut left: Vec<i64> = carried
                .iter()
                .flat_map(|&id| tasks.descendants(id))
                .filter(|&id| open(id))
                .collect();
            left.extend(
                carried
                    .iter()
                    .filter_map(|&id| tasks.get(id).claude_session.as_deref())
                    .flat_map(|s| of_session(&tasks, s)),
            );
            left.retain(|id| !carried.contains(id));
            left.sort();
            left.dedup();
            if gate && !left.is_empty() {
                let list: Vec<String> = left
                    .iter()
                    .map(|&id| format!("{} ({})", tasks.reference(id), tasks.effective_status(id)))
                    .collect();
                bail!(
                    "`choros land archive` says the work here is all out, but these are still \
                     open: {}. Finish or cancel them, set them to landing, or name them with \
                     --label to land them with it; `choros land continue` keeps the workspace",
                    list.join(", ")
                );
            }
            carried.clone()
        }
    };
    to_landing.retain(|&id| movable(id) && tasks.get(id).status != Some(Status::Landing));
    to_landing.sort_by_key(|&id| std::cmp::Reverse(tasks.ancestors(id).len()));
    Ok(Part {
        project,
        tasks,
        carried,
        to_landing,
        unfinished,
    })
}

/// The open tasks `session` is recorded on, but for the root: its session
/// plans, and what it lands is no task's work.
fn of_session(tasks: &Tasks, session: &str) -> Vec<i64> {
    tasks
        .all()
        .into_iter()
        .filter(|t| !t.is_deleted() && !t.is_root() && t.claude_session.as_deref() == Some(session))
        .filter(|t| tasks.effective_status(t.id).is_open())
        .map(|t| t.id)
        .collect()
}

/// Sets `ids` to landing, all or none, by the rules every change keeps.
/// With `keep` false, only tries it.
fn set_landing(conn: &mut Connection, project: &Project, ids: &[i64], keep: bool) -> Result<()> {
    let tx = conn.transaction()?;
    for id in ids {
        tx.execute(
            "UPDATE tasks SET status = 'landing', updated_at = ?1 WHERE id = ?2",
            params![crate::now(), id],
        )?;
    }
    Tasks::load(&tx, project)?.validate().context("refused")?;
    if keep {
        tx.commit()?;
    }
    Ok(())
}

/// Before a land: what to land with, or `None` when it carries no task of
/// ours. Fails when the statuses the land would bring couldn't be set.
pub fn before(conn: &mut Connection, told: &Told) -> Result<Option<Reply>> {
    let parts = parts(conn, told, true)?;
    if parts.is_empty() {
        return Ok(None);
    }
    for p in &parts {
        set_landing(conn, &p.project, &p.to_landing, false)?;
    }
    let carried: Vec<(&Part, i64)> = parts
        .iter()
        .flat_map(|p| p.carried.iter().map(move |&id| (p, id)))
        .collect();
    let refs: Vec<String> = carried
        .iter()
        .map(|(p, id)| p.tasks.reference(*id))
        .collect();

    let mut labels = told.labels.clone();
    labels.extend(refs.iter().filter(|r| !told.labels.contains(r)).cloned());
    let priority = match &told.priority {
        Some(_) => None,
        None => parts
            .iter()
            .flat_map(|p| {
                let derived = Derived::compute(&p.tasks);
                p.carried
                    .iter()
                    .filter_map(move |&id| derived.priority(id))
                    .collect::<Vec<_>>()
            })
            .min()
            .map(|p| format!("p{p}")),
    };
    let title = match (&told.title, told.mode, carried.as_slice()) {
        (None, Mode::Archive, [(p, only)]) => Some(p.tasks.get(*only).title().to_string()),
        _ => None,
    };
    // Only a body that was written gets the tasks named under it: choros
    // asks a session that gave none to write one, and that must still happen.
    let body = told.body.as_deref().and_then(|given| {
        let text = format!(
            "{}\n{given}",
            title.as_deref().or(told.title.as_deref()).unwrap_or("")
        );
        if refs.iter().all(|r| text.contains(r.as_str())) {
            return None;
        }
        let list: Vec<String> = carried
            .iter()
            .map(|(p, id)| format!("- {}  {}", p.tasks.reference(*id), p.tasks.get(*id).title()))
            .collect();
        Some(format!(
            "{}\n\nTasks:\n{}\n",
            given.trim_end(),
            list.join("\n")
        ))
    });
    Ok(Some(Reply {
        labels,
        priority,
        title,
        body,
    }))
}

/// After a land: sets the statuses. Returns a line for each thing worth
/// telling whoever landed.
pub fn after(conn: &mut Connection, told: &Told) -> Result<Vec<String>> {
    let mut said = Vec::new();
    for p in parts(conn, told, false)? {
        set_landing(conn, &p.project, &p.to_landing, true)
            .context("the work landed, but its tasks couldn't be set to landing")?;
        for &id in p.to_landing.iter().rev() {
            said.push(format!("{} is now landing", p.tasks.reference(id)));
        }
        for (id, under) in &p.unfinished {
            said.push(format!(
                "{} stays {}: {} under it is still open",
                p.tasks.reference(*id),
                p.tasks.effective_status(*id),
                p.tasks.reference(*under)
            ));
        }
    }
    Ok(said)
}
