//! Choros landings, and which tasks they carry: landing sets, and the lands
//! that went straight to main, which make no set. Ergon stores none of this:
//! it reads `choros status --json` each time. A set carries the tasks
//! its labels name (task refs, written by `choros land --label`); a set
//! without labels carries the tasks whose session it was forked from.
//! Labels win outright, since one session can take on many tasks and land
//! them in separate sets.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::refs::TaskRef;
use crate::task::{self, Tasks};

/// The part of `choros status --json` ergon reads.
#[derive(Deserialize)]
struct Status {
    queues: Vec<Queue>,
    #[serde(default)]
    instant: Vec<Instant>,
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

/// Where a front set's PR review stands, as of choros's last pass.
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
    let sets = status.queues.into_iter().flat_map(|q| {
        q.sets.into_iter().map(move |s| Landing {
            at: At::Set {
                queue: q.name.clone(),
                n: s.n,
            },
            repo: s.repo,
            state: s.state,
            pr: s.pr.map(|p| p.number),
            review: s.review,
            sessions: s.sessions.into_iter().map(|f| f.source).collect(),
            labels: task_refs(&s.labels),
        })
    });
    let instant = status.instant.into_iter().map(|i| Landing {
        at: At::Main { commit: i.commit },
        repo: i.repo,
        state: "merged".into(),
        pr: i.pr,
        review: None,
        sessions: i.sessions,
        labels: task_refs(&i.labels),
    });
    Ok(Some(sets.chain(instant).collect()))
}

/// Labels that aren't project-qualified refs are some other tool's.
fn task_refs(labels: &[String]) -> Vec<TaskRef> {
    labels
        .iter()
        .filter_map(|l| l.parse::<TaskRef>().ok())
        .filter(|r| r.project.is_some())
        .collect()
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

/// What choros reports in the folders a set of tasks were worked in.
pub struct Seen<'a> {
    tasks: &'a Tasks,
    fallback: &'a Path,
    /// Unreadable folders, like ones outside any trunk, hold `None`.
    read: HashMap<&'a Path, Option<Vec<Landing>>>,
}

impl<'a> Seen<'a> {
    /// Reads every folder the subtrees of `ids` were worked in (`fallback`
    /// for tasks without one), each once.
    pub fn read(tasks: &'a Tasks, ids: &[i64], fallback: &'a Path) -> Seen<'a> {
        let mut seen = Seen {
            tasks,
            fallback,
            read: HashMap::new(),
        };
        for &id in ids {
            for d in seen.dirs(&seen.subtree(id)) {
                seen.read.entry(d).or_insert_with(|| read(d).ok().flatten());
            }
        }
        seen
    }

    /// The landings that carry `id` or a task under it, read from the
    /// folders of that subtree. `None` when one of them couldn't be read,
    /// or `id` wasn't among those `read` was given.
    pub fn of(&self, id: i64) -> Option<Vec<&Landing>> {
        let ids = self.subtree(id);
        let sets = self
            .dirs(&ids)
            .into_iter()
            .map(|d| self.read.get(d)?.as_ref())
            .collect::<Option<Vec<_>>>()?;
        Some(
            sets.into_iter()
                .flatten()
                .filter(|l| ids.iter().any(|&t| l.carries(self.tasks, t)))
                .collect(),
        )
    }

    fn subtree(&self, id: i64) -> Vec<i64> {
        std::iter::once(id)
            .chain(self.tasks.descendants(id))
            .collect()
    }

    fn dirs(&self, ids: &[i64]) -> BTreeSet<&'a Path> {
        ids.iter()
            .map(|&t| dir_for(self.tasks, t).unwrap_or(self.fallback))
            .collect()
    }
}

/// The landing tasks whose landings have all merged, with the landing tasks
/// under them, deepest first so no task is done before its subtasks. A
/// task's landings are those that carry it or a task under it.
/// A task no landing carries stays landing: nothing says its work went anywhere.
pub fn merged(tasks: &Tasks, fallback: &Path) -> Vec<i64> {
    let landing: Vec<i64> = tasks
        .all()
        .into_iter()
        .filter(|t| !t.is_deleted() && t.status == Some(task::Status::Landing))
        .map(|t| t.id)
        .collect();
    let seen = Seen::read(tasks, &landing, fallback);
    let mut done = BTreeSet::new();
    for &id in &landing {
        let Some(carrying) = seen.of(id) else {
            continue;
        };
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
