//! Choros landing sets, and which tasks they carry. Ergon stores none of
//! this: it reads `choros status --json` each time. A set carries the tasks
//! its labels name (task refs, written by `choros land --label`); a set
//! without labels carries the tasks whose session it was forked from.
//! Labels win outright, since one session can take on many tasks and land
//! them in separate sets.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::refs::TaskRef;
use crate::task::Tasks;

/// The part of `choros status --json` ergon reads.
#[derive(Deserialize)]
struct Status {
    queues: Vec<Queue>,
}

#[derive(Deserialize)]
struct Queue {
    name: String,
    sets: Vec<Set>,
}

#[derive(Deserialize)]
struct Set {
    n: u32,
    repo: String,
    state: String,
    pr: Option<Pr>,
    sessions: Vec<SessionFork>,
    labels: Vec<String>,
}

#[derive(Deserialize)]
struct Pr {
    number: u64,
}

#[derive(Deserialize)]
struct SessionFork {
    source: String,
}

pub struct Landing {
    pub queue: String,
    pub n: u32,
    pub repo: String,
    pub state: String,
    pub pr: Option<u64>,
    sessions: Vec<String>,
    labels: Vec<TaskRef>,
}

impl Landing {
    /// Still in the queue, so `choros landing open` can reach it.
    pub fn is_open(&self) -> bool {
        matches!(self.state.as_str(), "queued" | "front")
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

/// Every landing set `choros status --json --all` reports when run in `dir`:
/// those of every queue in the trunk `dir` is in, or of every trunk outside
/// one. `None` when choros isn't installed.
pub fn read(dir: &Path) -> Result<Option<Vec<Landing>>> {
    if crate::on_path(Path::new("choros")).is_none() {
        return Ok(None);
    }
    let out = Command::new("choros")
        .args(["status", "--json", "--all"])
        .current_dir(dir)
        .output()
        .with_context(|| format!("running choros status in {}", dir.display()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!(
            "choros status failed in {}: {}",
            dir.display(),
            err.lines()
                .next()
                .unwrap_or("")
                .trim_start_matches("choros: ")
        );
    }
    let status: Status =
        serde_json::from_slice(&out.stdout).context("reading choros status --json")?;
    Ok(Some(
        status
            .queues
            .into_iter()
            .flat_map(|q| {
                q.sets.into_iter().map(move |s| Landing {
                    queue: q.name.clone(),
                    n: s.n,
                    repo: s.repo,
                    state: s.state,
                    pr: s.pr.map(|p| p.number),
                    sessions: s.sessions.into_iter().map(|f| f.source).collect(),
                    // Labels that aren't project-qualified refs are some
                    // other tool's.
                    labels: s
                        .labels
                        .iter()
                        .filter_map(|l| l.parse::<TaskRef>().ok())
                        .filter(|r| r.project.is_some())
                        .collect(),
                })
            })
            .collect(),
    ))
}

/// The directory to ask choros from for `id`: where its session, or its
/// nearest ancestor's, began, if that's still there.
pub fn dir_for(tasks: &Tasks, id: i64) -> Option<&Path> {
    std::iter::once(id)
        .chain(tasks.ancestors(id))
        .find_map(|t| tasks.get(t).claude_cwd.as_deref())
        .map(Path::new)
        .filter(|d| d.is_dir())
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
