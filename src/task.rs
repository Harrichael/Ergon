use std::collections::{BTreeSet, HashMap};
use std::fmt;

use anyhow::{Context, Result, bail};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{Connection, Row};

use crate::project::Project;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Todo,
    InProgress,
    Landing,
    Done,
    Cancelled,
}

impl Status {
    pub const ALL: [Status; 5] = [
        Status::Todo,
        Status::InProgress,
        Status::Landing,
        Status::Done,
        Status::Cancelled,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Todo => "todo",
            Status::InProgress => "in_progress",
            Status::Landing => "landing",
            Status::Done => "done",
            Status::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Result<Status> {
        let s = s.replace('-', "_");
        Status::ALL
            .into_iter()
            .find(|st| st.as_str() == s)
            .with_context(|| {
                format!(
                    "invalid status {s:?}: expected todo, in_progress, landing, done or cancelled"
                )
            })
    }

    /// `todo` or `in_progress`.
    pub fn is_open(self) -> bool {
        matches!(self, Status::Todo | Status::InProgress)
    }

    /// `done` or `cancelled`.
    pub fn is_closed(self) -> bool {
        matches!(self, Status::Done | Status::Cancelled)
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ToSql for Status {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.as_str().into())
    }
}

impl FromSql for Status {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        Status::parse(value.as_str()?).map_err(|e| FromSqlError::Other(e.into()))
    }
}

/// Parses `p0`..`p4` into 0..4.
pub fn parse_priority(s: &str) -> Result<u8> {
    match s.as_bytes() {
        [b'p' | b'P', d @ b'0'..=b'4'] => Ok(d - b'0'),
        _ => bail!("invalid priority {s:?}: expected p0, p1, p2, p3 or p4"),
    }
}

/// Parses a size: a Fibonacci number, 1 or more.
pub fn parse_size(s: &str) -> Result<u32> {
    let n: u32 = s
        .parse()
        .ok()
        .filter(|&n| is_fibonacci(n))
        .with_context(|| {
            format!("invalid size {s:?}: expected a Fibonacci number: 1, 2, 3, 5, 8, 13, ...")
        })?;
    Ok(n)
}

/// Parses an attention: 1 (just execute) to 10 (the user's decision).
pub fn parse_attention(s: &str) -> Result<u8> {
    s.parse()
        .ok()
        .filter(|n| (1..=10).contains(n))
        .with_context(|| format!("invalid attention {s:?}: expected 1 to 10"))
}

fn is_fibonacci(n: u32) -> bool {
    let (mut a, mut b) = (1u64, 2u64);
    while a < n as u64 {
        (a, b) = (b, a + b);
    }
    n >= 1 && a == n as u64
}

pub const TITLE_MAX_CHARS: usize = 100;

/// The first line of `intent`, capped at 100 characters.
pub fn title(intent: &str) -> &str {
    let line = intent.trim_start().lines().next().unwrap_or("").trim_end();
    match line.char_indices().nth(TITLE_MAX_CHARS) {
        Some((i, _)) => &line[..i],
        None => line,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: i64,
    pub project_id: i64,
    pub slug: String,
    pub parent_id: Option<i64>,
    pub intent: String,
    pub status: Option<Status>,
    pub priority: Option<u8>,
    /// A size marks the task as one unit of work; its subtasks are steps.
    pub size: Option<u32>,
    /// 1 (just execute) to 10 (the user's decision).
    pub attention: Option<u8>,
    pub claude_session: Option<String>,
    pub claude_cwd: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub deleted_at: Option<i64>,
    /// Display order among siblings; it carries no meaning.
    pub position: i64,
}

impl Task {
    pub fn title(&self) -> &str {
        title(&self.intent)
    }

    pub fn is_root(&self) -> bool {
        self.parent_id.is_none()
    }

    pub fn is_deleted(&self) -> bool {
        self.deleted_at.is_some()
    }
}

const COLUMNS: &str = "id, project_id, slug, parent_id, intent, status, priority, \
     claude_session, claude_cwd, created_at, updated_at, deleted_at, position, size, attention";

fn from_row(row: &Row) -> rusqlite::Result<Task> {
    Ok(Task {
        id: row.get(0)?,
        project_id: row.get(1)?,
        slug: row.get(2)?,
        parent_id: row.get(3)?,
        intent: row.get(4)?,
        status: row.get(5)?,
        priority: row.get(6)?,
        claude_session: row.get(7)?,
        claude_cwd: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
        deleted_at: row.get(11)?,
        position: row.get(12)?,
        size: row.get(13)?,
        attention: row.get(14)?,
    })
}

/// A snapshot of every task in one project, deleted ones included, with the
/// tree and dependency indexes needed to evaluate the model's rules.
pub struct Tasks {
    pub project: Project,
    tasks: HashMap<i64, Task>,
    by_slug: HashMap<String, i64>,
    children: HashMap<i64, Vec<i64>>,
    root: i64,
    /// `(task, depends_on)` pairs.
    pub deps: BTreeSet<(i64, i64)>,
}

/// When each of the project's done tasks last became done, from its
/// history; only tasks done by a status of their own are here.
pub fn done_at(conn: &Connection, project: &Project) -> Result<HashMap<i64, i64>> {
    // Rows by id, not time: changes in the same second keep their order.
    let mut stmt = conn.prepare(
        "SELECT h.task_id, MIN(h.at) FROM task_history h JOIN tasks t ON t.id = h.task_id
         WHERE t.project_id = ?1 AND t.status = 'done' AND h.status = 'done'
           AND h.id > COALESCE((SELECT MAX(o.id) FROM task_history o
                                WHERE o.task_id = h.task_id AND o.status IS NOT 'done'), 0)
         GROUP BY h.task_id",
    )?;
    let rows = stmt
        .query_map([project.id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

impl Tasks {
    pub fn load(conn: &Connection, project: &Project) -> Result<Tasks> {
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM tasks WHERE project_id = ?1 ORDER BY created_at, id"
        ))?;
        let list = stmt
            .query_map([project.id], from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut stmt = conn.prepare(
            "SELECT d.task_id, d.depends_on FROM task_deps d
             JOIN tasks t ON t.id = d.task_id WHERE t.project_id = ?1",
        )?;
        let deps = stmt
            .query_map([project.id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;

        let mut root = None;
        let mut children: HashMap<i64, Vec<i64>> = HashMap::new();
        let mut by_position: Vec<&Task> = list.iter().collect();
        by_position.sort_by_key(|t| (t.position, t.created_at, t.id));
        for t in by_position {
            match t.parent_id {
                Some(p) => children.entry(p).or_default().push(t.id),
                None => root = Some(t.id),
            }
        }
        Ok(Tasks {
            project: project.clone(),
            root: root.with_context(|| format!("project {} has no root task", project.name))?,
            by_slug: list.iter().map(|t| (t.slug.clone(), t.id)).collect(),
            tasks: list.into_iter().map(|t| (t.id, t)).collect(),
            children,
            deps,
        })
    }

    pub fn get(&self, id: i64) -> &Task {
        &self.tasks[&id]
    }

    pub fn root(&self) -> &Task {
        self.get(self.root)
    }

    /// Every task, deleted ones included, oldest first.
    pub fn all(&self) -> Vec<&Task> {
        let mut out: Vec<&Task> = self.tasks.values().collect();
        out.sort_by_key(|t| (t.created_at, t.id));
        out
    }

    /// A task by slug, deleted or not.
    pub fn find(&self, slug: &str) -> Result<&Task> {
        match self.by_slug.get(slug) {
            Some(id) => Ok(self.get(*id)),
            None => bail!("no task {}/{slug}", self.project.name),
        }
    }

    /// A live (not deleted) task by slug.
    pub fn live(&self, slug: &str) -> Result<&Task> {
        let t = self.find(slug)?;
        if t.is_deleted() {
            bail!("{} is deleted", self.reference(t.id));
        }
        Ok(t)
    }

    /// `project/slug`.
    pub fn reference(&self, id: i64) -> String {
        format!("{}/{}", self.project.name, self.get(id).slug)
    }

    /// Live children, in display order.
    pub fn children(&self, id: i64) -> impl Iterator<Item = &Task> {
        self.children
            .get(&id)
            .into_iter()
            .flatten()
            .map(|c| self.get(*c))
            .filter(|t| !t.is_deleted())
    }

    /// Descendants, deleted or not, depth first.
    pub fn all_descendants(&self, id: i64) -> Vec<i64> {
        let mut out = Vec::new();
        let mut stack = vec![id];
        while let Some(t) = stack.pop() {
            for &c in self.children.get(&t).into_iter().flatten().rev() {
                out.push(c);
                stack.push(c);
            }
        }
        out
    }

    /// Live tasks outside the subtree at `id` that depend on a task inside it.
    pub fn outside_dependents(&self, id: i64) -> Vec<i64> {
        let subtree: std::collections::HashSet<i64> =
            std::iter::once(id).chain(self.descendants(id)).collect();
        let deps: BTreeSet<i64> = self
            .live_deps()
            .filter(|(t, d)| subtree.contains(d) && !subtree.contains(t))
            .map(|(t, _)| t)
            .collect();
        deps.into_iter().collect()
    }

    /// Live descendants, depth first.
    pub fn descendants(&self, id: i64) -> Vec<i64> {
        let mut out = Vec::new();
        let mut stack: Vec<i64> = self.children(id).map(|t| t.id).collect();
        stack.reverse();
        while let Some(t) = stack.pop() {
            out.push(t);
            let mut kids: Vec<i64> = self.children(t).map(|c| c.id).collect();
            kids.reverse();
            stack.extend(kids);
        }
        out
    }

    /// Ancestors, nearest first.
    pub fn ancestors(&self, id: i64) -> Vec<i64> {
        let mut out = Vec::new();
        let mut cur = self.get(id).parent_id;
        while let Some(p) = cur {
            out.push(p);
            cur = self.get(p).parent_id;
        }
        out
    }

    pub fn is_ancestor(&self, ancestor: i64, of: i64) -> bool {
        self.ancestors(of).contains(&ancestor)
    }

    /// The task's own status, else its nearest ancestor's.
    pub fn effective_status(&self, id: i64) -> Status {
        let mut cur = Some(id);
        while let Some(t) = cur {
            let task = self.get(t);
            if let Some(s) = task.status {
                return s;
            }
            cur = task.parent_id;
        }
        // Unreachable while the root keeps a status; todo is the safe reading.
        Status::Todo
    }

    /// Checks every rule a change could break, naming the first violation.
    pub fn validate(&self) -> Result<()> {
        let root = self.root();
        if root.is_deleted() {
            bail!("the root task can't be deleted");
        }
        match root.status {
            None => bail!("the root task must have a status"),
            Some(Status::Cancelled) => bail!("the root task can't be cancelled"),
            _ => {}
        }

        // In creation order, so the violation reported is deterministic.
        let mut live: Vec<&Task> = self.tasks.values().filter(|t| !t.is_deleted()).collect();
        live.sort_by_key(|t| (t.created_at, t.id));
        let effective: HashMap<i64, Status> = live
            .iter()
            .map(|t| (t.id, self.effective_status(t.id)))
            .collect();

        for t in &live {
            if let Some(p) = t.parent_id.filter(|p| self.get(*p).is_deleted()) {
                bail!(
                    "{} can't be live while its parent {} is deleted",
                    self.reference(t.id),
                    self.reference(p)
                );
            }
            if t.parent_id == Some(self.root) && t.status.is_none() {
                bail!(
                    "{} is a top-level task, so it must have a status",
                    self.reference(t.id)
                );
            }
            if t.size.is_some()
                && let Some(a) = self
                    .ancestors(t.id)
                    .into_iter()
                    .find(|&a| self.get(a).size.is_some())
            {
                let (ra, rt) = (self.reference(a), self.reference(t.id));
                bail!(
                    "{ra} and its subtask {rt} can't both be sized: a size marks one \
                     unit of work, and its subtasks are steps of it. Unsize one: \
                     `ergon edit {ra} --size none` or `ergon edit {rt} --size none`"
                );
            }
            let status = effective[&t.id];
            for a in self.ancestors(t.id) {
                let reason = match effective[&a] {
                    Status::Done if !status.is_closed() => "done",
                    Status::Landing if !(status.is_closed() || status == Status::Landing) => {
                        "landing"
                    }
                    _ => continue,
                };
                bail!(
                    "{} is {reason}, so its subtask {} can't be {status}",
                    self.reference(a),
                    self.reference(t.id)
                );
            }
        }
        self.validate_deps()
    }

    fn is_live(&self, id: i64) -> bool {
        self.tasks.get(&id).is_some_and(|t| !t.is_deleted())
    }

    /// Dependencies between live tasks, as `(task, depends_on)`.
    pub fn live_deps(&self) -> impl Iterator<Item = (i64, i64)> + '_ {
        self.deps
            .iter()
            .copied()
            .filter(|&(t, d)| self.is_live(t) && self.is_live(d))
    }

    /// Live tasks that `id` depends on, oldest first.
    pub fn dependencies(&self, id: i64) -> Vec<&Task> {
        let mut out: Vec<&Task> = self
            .live_deps()
            .filter(|&(t, _)| t == id)
            .map(|(_, d)| self.get(d))
            .collect();
        out.sort_by_key(|t| (t.created_at, t.id));
        out
    }

    /// Live tasks that depend on `id`, oldest first.
    pub fn dependents(&self, id: i64) -> Vec<&Task> {
        let mut out: Vec<&Task> = self
            .live_deps()
            .filter(|&(_, d)| d == id)
            .map(|(t, _)| self.get(t))
            .collect();
        out.sort_by_key(|t| (t.created_at, t.id));
        out
    }

    fn validate_deps(&self) -> Result<()> {
        for &(t, d) in &self.deps {
            if !self.tasks.contains_key(&d) {
                bail!("{} depends on a task in another project", self.reference(t));
            }
        }
        for (t, d) in self.live_deps() {
            let (rt, rd) = (self.reference(t), self.reference(d));
            if t == self.root || d == self.root {
                bail!("the root task can't have dependencies or dependents");
            }
            if t == d {
                bail!("{rt} can't depend on itself");
            }
            if self.is_ancestor(d, t) {
                bail!("{rt} can't depend on {rd}, its ancestor");
            }
            if self.is_ancestor(t, d) {
                bail!("{rt} can't depend on {rd}, its subtask");
            }
        }
        if let Some(cycle) = self.find_cycle() {
            let refs: Vec<String> = cycle.iter().map(|&id| self.reference(id)).collect();
            bail!(
                "these tasks would wait on each other: {}",
                refs.join(" -> ")
            );
        }
        Ok(())
    }

    /// A cycle in what live tasks wait on, as a path that starts and ends at
    /// the same task. A task waits on its dependencies, its subtasks inherit
    /// those waits (blocking passes down), and a task waits on its subtasks
    /// (it can't be done before them). So `a -> b -> c -> a1` deadlocks when
    /// `a1` is a subtask of `a`, though the dependency edges alone don't loop.
    fn find_cycle(&self) -> Option<Vec<i64>> {
        let mut edges: HashMap<i64, BTreeSet<i64>> = HashMap::new();
        for (t, d) in self.live_deps() {
            for x in std::iter::once(t).chain(self.descendants(t)) {
                edges.entry(x).or_default().insert(d);
            }
        }
        for t in self.tasks.values().filter(|t| !t.is_deleted()) {
            if let Some(p) = t.parent_id {
                edges.entry(p).or_default().insert(t.id);
            }
        }

        // The shortest cycle, found by a breadth-first search back to each
        // start; ties go to the lowest start id, so the report is stable.
        let mut starts: Vec<i64> = edges.keys().copied().collect();
        starts.sort();
        let mut best: Option<Vec<i64>> = None;
        for start in starts {
            let mut prev: HashMap<i64, i64> = HashMap::new();
            let mut queue = std::collections::VecDeque::from([start]);
            'search: while let Some(n) = queue.pop_front() {
                for &m in edges.get(&n).into_iter().flatten() {
                    if m == start {
                        let mut path = vec![start];
                        let mut cur = n;
                        while cur != start {
                            path.push(cur);
                            cur = prev[&cur];
                        }
                        let last = path.len();
                        path[1..last].reverse();
                        path.push(start);
                        if best.as_ref().is_none_or(|b| path.len() < b.len()) {
                            best = Some(path);
                        }
                        break 'search;
                    }
                    if m != start && !prev.contains_key(&m) {
                        prev.insert(m, n);
                        queue.push_back(m);
                    }
                }
            }
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles() {
        assert_eq!(title("Fix it\n\nBecause"), "Fix it");
        assert_eq!(title("\n  \nSecond line  \nthird"), "Second line");
        assert_eq!(title(""), "");
        let long = "é".repeat(150);
        assert_eq!(title(&long).chars().count(), 100);
    }

    #[test]
    fn statuses_and_priorities() {
        for s in Status::ALL {
            assert_eq!(Status::parse(s.as_str()).unwrap(), s);
        }
        assert_eq!(Status::parse("in-progress").unwrap(), Status::InProgress);
        assert!(Status::parse("blocked").is_err());
        assert_eq!(parse_priority("p0").unwrap(), 0);
        assert_eq!(parse_priority("P4").unwrap(), 4);
        for bad in ["p5", "2", "p", "p10"] {
            assert!(parse_priority(bad).is_err(), "{bad}");
        }
        for good in [1, 2, 3, 5, 8, 13, 21, 89, 2971215073] {
            assert_eq!(parse_size(&good.to_string()).unwrap(), good);
        }
        for bad in ["0", "4", "6", "100", "-1", "x", "2971215074"] {
            assert!(parse_size(bad).is_err(), "{bad}");
        }
    }
}
