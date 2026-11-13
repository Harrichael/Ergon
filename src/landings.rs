//! Choros landings, and which tasks they carry: landing sets, and the lands
//! that went straight to main, which make no set. Ergon keeps a copy of
//! them, as choros's event log last told them (see [`crate::sync`]), so
//! reading them asks choros nothing. A set carries the tasks its labels
//! name (task refs, written by `choros land --label`); a set without labels
//! carries the tasks whose session it was forked from. Labels win outright,
//! since one session can take on many tasks and land them in separate sets.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;

use crate::refs::TaskRef;
use crate::task::{self, Tasks};

/// The part of a line of `choros events` ergon reads: a set as it now
/// stands, or a land straight to main.
#[derive(Deserialize)]
struct Event {
    seq: i64,
    queue: Option<String>,
    set: Option<Set>,
    instant: Option<Instant>,
}

#[derive(Deserialize)]
struct Set {
    n: u32,
    repo: String,
    state: String,
    pr: Option<Pr>,
    #[serde(default)]
    review: Option<Review>,
    sessions: Vec<SessionFork>,
    labels: Vec<String>,
}

#[derive(Deserialize)]
struct Instant {
    repo: String,
    commit: String,
    pr: Option<u64>,
    sessions: Vec<String>,
    labels: Vec<String>,
}

#[derive(Deserialize)]
struct Pr {
    number: u64,
}

/// Where a front or stacked set's PR review stands, as of choros's last pass.
#[derive(Clone, Copy, Deserialize)]
pub struct Review {
    pub draft: bool,
    /// Someone besides the PR's author spoke after the author last did.
    pub unanswered: bool,
    #[serde(default)]
    pub approved: bool,
}

#[derive(Deserialize)]
struct SessionFork {
    source: String,
}

pub struct Landing {
    /// The trunk it is in.
    pub trunk: PathBuf,
    pub at: At,
    pub repo: String,
    pub state: String,
    pub pr: Option<u64>,
    pub review: Option<Review>,
    sessions: Vec<String>,
    labels: Vec<TaskRef>,
}

pub enum At {
    Set {
        queue: String,
        n: u32,
    },
    /// Straight to main, through this commit; always merged.
    Main {
        commit: String,
    },
}

impl Landing {
    /// Still in the queue, so `choros landing open` can reach it.
    pub fn is_open(&self) -> bool {
        matches!(self.state.as_str(), "queued" | "stacked" | "front")
    }

    /// Whether this set carries `id`.
    pub fn carries(&self, tasks: &Tasks, id: i64) -> bool {
        let t = tasks.get(id);
        if self.labels.is_empty() {
            return t
                .claude_session
                .as_ref()
                .is_some_and(|s| self.sessions.contains(s));
        }
        self.labels
            .iter()
            .any(|r| r.slug == t.slug && r.project.as_deref() == Some(&tasks.project.name))
    }
}

/// Takes in a line of `choros events` from `trunk`, and returns its `seq`
/// and whether it tells of a merge.
pub fn apply(conn: &Connection, trunk: &Path, line: &str) -> Result<(i64, bool)> {
    let event: Event = serde_json::from_str(line).context("reading a line of choros events")?;
    let trunk = trunk.to_string_lossy();
    let (key, gone, merged) = match (&event.queue, &event.set, &event.instant) {
        (Some(queue), Some(set), _) => (
            format!("set/{queue}/{}", set.n),
            set.state == "dropped",
            set.state == "merged",
        ),
        (_, _, Some(i)) => (format!("main/{}/{}", i.repo, i.commit), false, true),
        // Something a later choros tells of.
        _ => (String::new(), true, false),
    };
    if gone {
        conn.execute(
            "DELETE FROM landings WHERE trunk = ?1 AND key = ?2",
            params![trunk, key],
        )?;
    } else {
        conn.execute(
            "INSERT INTO landings (trunk, key, body) VALUES (?1, ?2, ?3)
             ON CONFLICT DO UPDATE SET body = ?3",
            params![trunk, key, line],
        )?;
    }
    conn.execute(
        "INSERT INTO choros_logs (trunk, seq) VALUES (?1, ?2) ON CONFLICT DO UPDATE SET seq = ?2",
        params![trunk, event.seq],
    )?;
    Ok((event.seq, merged))
}

/// The last `seq` taken in from `trunk`'s log; 0 before any.
pub fn read_through(conn: &Connection, trunk: &Path) -> Result<i64> {
    Ok(conn
        .query_row(
            "SELECT seq FROM choros_logs WHERE trunk = ?1",
            [trunk.to_string_lossy()],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

/// Drops what is kept of `trunk`, to read its log again from the start.
pub fn forget(conn: &Connection, trunk: &Path) -> Result<()> {
    let trunk = trunk.to_string_lossy();
    conn.execute("DELETE FROM landings WHERE trunk = ?1", [&trunk])?;
    conn.execute("DELETE FROM choros_logs WHERE trunk = ?1", [&trunk])?;
    Ok(())
}

/// Every landing ergon has been told of. `None` before any choros log was
/// read: nothing is known, which isn't the same as nothing landed.
pub fn all(conn: &Connection) -> Result<Option<Vec<Landing>>> {
    if !conn.query_row("SELECT EXISTS (SELECT 1 FROM choros_logs)", [], |r| r.get(0))? {
        return Ok(None);
    }
    let mut rows = conn.prepare("SELECT trunk, body FROM landings ORDER BY trunk, key")?;
    let rows = rows.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut out = Vec::new();
    for row in rows {
        let (trunk, body) = row?;
        let trunk = PathBuf::from(trunk);
        let event: Event = serde_json::from_str(&body).context("reading a kept landing")?;
        match (event.queue, event.set, event.instant) {
            (Some(queue), Some(s), _) => out.push(Landing {
                trunk,
                at: At::Set { queue, n: s.n },
                repo: s.repo,
                state: s.state,
                pr: s.pr.map(|p| p.number),
                review: s.review,
                sessions: s.sessions.into_iter().map(|f| f.source).collect(),
                labels: task_refs(&s.labels),
            }),
            (_, _, Some(i)) => out.push(Landing {
                trunk,
                at: At::Main { commit: i.commit },
                repo: i.repo,
                state: "merged".into(),
                pr: i.pr,
                review: None,
                sessions: i.sessions,
                labels: task_refs(&i.labels),
            }),
            _ => {}
        }
    }
    Ok(Some(out))
}

/// Labels that aren't project-qualified refs are some other tool's.
fn task_refs(labels: &[String]) -> Vec<TaskRef> {
    labels
        .iter()
        .filter_map(|l| TaskRef::exact(l).ok())
        .filter(|r| r.project.is_some())
        .collect()
}

/// `dir`, or the nearest folder above it that exists.
pub fn still_there(dir: &Path) -> Option<&Path> {
    dir.ancestors().find(|a| a.is_dir())
}

/// A set that carries `id` or a task under it, with the ones it carries.
pub struct TaskLanding<'a> {
    pub landing: &'a Landing,
    pub carried: Vec<i64>,
}

pub fn of_task<'a>(tasks: &Tasks, id: i64, landings: &'a [Landing]) -> Vec<TaskLanding<'a>> {
    let subtree: Vec<i64> = std::iter::once(id).chain(tasks.descendants(id)).collect();
    landings
        .iter()
        .filter_map(|l| {
            let carried: Vec<i64> = subtree
                .iter()
                .copied()
                .filter(|&t| l.carries(tasks, t))
                .collect();
            (!carried.is_empty()).then_some(TaskLanding {
                landing: l,
                carried,
            })
        })
        .collect()
}

/// The live tasks in `projects` that `landing` carries.
pub fn tasks_of<'a>(landing: &Landing, projects: &'a [Tasks]) -> Vec<(&'a Tasks, i64)> {
    projects
        .iter()
        .flat_map(|tasks| {
            tasks
                .all()
                .into_iter()
                .filter(|t| !t.is_deleted() && landing.carries(tasks, t.id))
                .map(move |t| (tasks, t.id))
        })
        .collect()
}

/// The landings that carry `id` or a task under it.
pub fn carrying<'a>(tasks: &Tasks, id: i64, all: &'a [Landing]) -> Vec<&'a Landing> {
    let ids: Vec<i64> = std::iter::once(id).chain(tasks.descendants(id)).collect();
    all.iter()
        .filter(|l| ids.iter().any(|&t| l.carries(tasks, t)))
        .collect()
}

/// The landing tasks whose landings have all merged, with the landing tasks
/// under them, deepest first so no task is done before its subtasks. A
/// task's landings are those that carry it or a task under it.
/// A task no landing carries stays landing: nothing says its work went anywhere.
pub fn merged(tasks: &Tasks, all: &[Landing]) -> Vec<i64> {
    let landing: Vec<i64> = tasks
        .all()
        .into_iter()
        .filter(|t| !t.is_deleted() && t.status == Some(task::Status::Landing))
        .map(|t| t.id)
        .collect();
    let mut done = BTreeSet::new();
    for &id in &landing {
        let carrying = carrying(tasks, id, all);
        if !carrying.is_empty() && carrying.iter().all(|l| l.state == "merged") {
            done.extend(
                std::iter::once(id)
                    .chain(tasks.descendants(id))
                    .filter(|&t| tasks.get(t).status == Some(task::Status::Landing)),
            );
        }
    }
    let mut done: Vec<i64> = done.into_iter().collect();
    done.sort_by_key(|&t| std::cmp::Reverse(tasks.ancestors(t).len()));
    done
}
