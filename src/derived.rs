//! Values computed from a project snapshot: blocked, effective priority, and
//! the `ls` selection.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::task::Tasks;

/// Blocked state and effective priority of every live task.
pub struct Derived {
    blocked: HashSet<i64>,
    priority: HashMap<i64, Option<u8>>,
}

impl Derived {
    pub fn compute(tasks: &Tasks) -> Derived {
        let root = tasks.root().id;

        let mut blocked = HashSet::new();
        mark_blocked(tasks, root, false, &mut blocked);

        // Pass 1: upward, from stored priorities and open children.
        let mut upward = HashMap::new();
        compute_upward(tasks, root, &mut upward);

        // Pass 2: downward, filling gaps from the parent.
        let mut base = HashMap::new();
        compute_base(tasks, root, None, &upward, &mut base);

        // Pass 3: boost each task to the most urgent task waiting on it.
        let waiters = waiters(tasks);
        let priority = base
            .keys()
            .map(|&id| {
                let boosted = reachable(&waiters, id)
                    .filter(|w| !tasks.effective_status(*w).is_closed())
                    .filter_map(|w| base[&w])
                    .chain(base[&id])
                    .min();
                (id, boosted)
            })
            .collect();

        Derived { blocked, priority }
    }

    pub fn is_blocked(&self, id: i64) -> bool {
        self.blocked.contains(&id)
    }

    pub fn priority(&self, id: i64) -> Option<u8> {
        self.priority.get(&id).copied().flatten()
    }
}

fn mark_blocked(tasks: &Tasks, id: i64, inherited: bool, out: &mut HashSet<i64>) {
    let blocked = inherited || !unfinished_dependencies(tasks, id).is_empty();
    if blocked {
        out.insert(id);
    }
    for c in tasks.children(id) {
        mark_blocked(tasks, c.id, blocked, out);
    }
}

/// The task's own dependencies that aren't done or cancelled.
fn unfinished_dependencies(tasks: &Tasks, id: i64) -> Vec<i64> {
    tasks
        .dependencies(id)
        .into_iter()
        .map(|d| d.id)
        .filter(|&d| !tasks.effective_status(d).is_closed())
        .collect()
}

/// Unfinished dependencies of the task and its ancestors: what it waits on.
pub fn blocked_by(tasks: &Tasks, id: i64) -> Vec<i64> {
    std::iter::once(id)
        .chain(tasks.ancestors(id))
        .flat_map(|t| unfinished_dependencies(tasks, t))
        .collect()
}

fn compute_upward(tasks: &Tasks, id: i64, out: &mut HashMap<i64, Option<u8>>) -> Option<u8> {
    let mut ranks = Vec::new();
    for c in tasks.children(id) {
        let p = compute_upward(tasks, c.id, out);
        if let Some(p) = p.filter(|_| !tasks.effective_status(c.id).is_closed()) {
            ranks.push(p as f64 + 1.0);
        }
    }
    // The root stands for the whole project, so averaging its children would
    // push a project-wide mean onto every unprioritized top-level task.
    let task = tasks.get(id);
    let inferred = if task.is_root() {
        None
    } else {
        geometric_mean_priority(&ranks)
    };
    let p = task.priority.or(inferred);
    out.insert(id, p);
    p
}

/// The geometric mean of ranks (`pN` is rank `N+1`), rounded to the nearest
/// rank with halves going to the more urgent one, as a priority.
fn geometric_mean_priority(ranks: &[f64]) -> Option<u8> {
    if ranks.is_empty() {
        return None;
    }
    let mean = (ranks.iter().map(|r| r.ln()).sum::<f64>() / ranks.len() as f64).exp();
    let rank = (mean - 0.5).ceil().clamp(1.0, 5.0);
    Some(rank as u8 - 1)
}

fn compute_base(
    tasks: &Tasks,
    id: i64,
    parent: Option<u8>,
    upward: &HashMap<i64, Option<u8>>,
    out: &mut HashMap<i64, Option<u8>>,
) {
    let p = upward[&id].or(parent);
    out.insert(id, p);
    for c in tasks.children(id) {
        compute_base(tasks, c.id, p, upward, out);
    }
}

/// For each task, the tasks that directly wait on it: those that depend on
/// it, and their subtasks (which inherit the block).
fn waiters(tasks: &Tasks) -> HashMap<i64, Vec<i64>> {
    let mut out: HashMap<i64, Vec<i64>> = HashMap::new();
    for (t, d) in tasks.live_deps() {
        let entry = out.entry(d).or_default();
        entry.push(t);
        entry.extend(tasks.descendants(t));
    }
    out
}

fn reachable(edges: &HashMap<i64, Vec<i64>>, start: i64) -> impl Iterator<Item = i64> {
    let mut seen = HashSet::from([start]);
    let mut queue = VecDeque::from([start]);
    let mut out = Vec::new();
    while let Some(n) = queue.pop_front() {
        for &m in edges.get(&n).into_iter().flatten() {
            if seen.insert(m) {
                out.push(m);
                queue.push_back(m);
            }
        }
    }
    out.into_iter()
}

/// The coarsest open, unblocked work: most urgent first, then oldest.
pub fn ls(tasks: &Tasks, derived: &Derived) -> Vec<i64> {
    let mut out = Vec::new();
    for c in tasks.children(tasks.root().id) {
        visit(tasks, derived, c.id, &mut out);
    }
    out.sort_by_key(|&id| {
        let t = tasks.get(id);
        (derived.priority(id).unwrap_or(5), t.created_at, t.id)
    });
    out
}

fn visit(tasks: &Tasks, derived: &Derived, id: i64, out: &mut Vec<i64>) {
    if !tasks.effective_status(id).is_open() || derived.is_blocked(id) {
        return;
    }
    let has_blocked_work = tasks
        .descendants(id)
        .into_iter()
        .any(|d| tasks.effective_status(d).is_open() && derived.is_blocked(d));
    if has_blocked_work {
        for c in tasks.children(id) {
            visit(tasks, derived, c.id, out);
        }
    } else {
        out.push(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::ops::{self, Changes};
    use crate::project::{self, Project};
    use crate::task::Status;
    use rusqlite::Connection;

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

        fn add(&mut self, parent: Option<&str>, priority: Option<u8>) -> String {
            ops::add(
                &mut self.conn,
                &self.project,
                parent,
                "Task",
                None,
                priority,
            )
            .unwrap()
        }

        fn dep(&mut self, t: &str, d: &str) {
            ops::dep_add(&mut self.conn, &self.project, t, d).unwrap();
        }

        fn status(&mut self, t: &str, s: Status) {
            let changes = Changes {
                status: Some(Some(s)),
                ..Default::default()
            };
            ops::edit(&mut self.conn, &self.project, t, changes).unwrap();
        }

        fn eval(&self) -> (Tasks, Derived) {
            let tasks = Tasks::load(&self.conn, &self.project).unwrap();
            let derived = Derived::compute(&tasks);
            (tasks, derived)
        }

        fn priority(&self, slug: &str) -> Option<u8> {
            let (tasks, d) = self.eval();
            d.priority(tasks.live(slug).unwrap().id)
        }

        fn blocked(&self, slug: &str) -> bool {
            let (tasks, d) = self.eval();
            d.is_blocked(tasks.live(slug).unwrap().id)
        }

        fn ls(&self) -> Vec<String> {
            let (tasks, d) = self.eval();
            ls(&tasks, &d)
                .into_iter()
                .map(|id| tasks.get(id).slug.clone())
                .collect()
        }
    }

    #[test]
    fn geometric_mean() {
        let p = |ps: &[u8]| {
            geometric_mean_priority(&ps.iter().map(|&p| p as f64 + 1.0).collect::<Vec<_>>())
        };
        assert_eq!(p(&[]), None);
        assert_eq!(p(&[2]), Some(2));
        assert_eq!(p(&[4, 0]), Some(1)); // sqrt(5) = 2.24 -> rank 2
        assert_eq!(p(&[1, 0]), Some(0)); // sqrt(2) = 1.41 -> rank 1
        assert_eq!(p(&[4, 4, 0]), Some(2)); // cbrt(25) = 2.92 -> rank 3
        assert_eq!(p(&[0, 0, 0, 0]), Some(0));
    }

    #[test]
    fn boost_is_transitive_and_includes_subtasks_of_dependents() {
        let mut f = Fixture::new();
        let a = f.add(None, Some(3));
        let urgent_child = f.add(Some(&a), Some(0));
        let b = f.add(None, Some(4));
        let c = f.add(None, Some(4));
        f.dep(&a, &b);
        f.dep(&b, &c);
        // a's upward priority comes from its stored p3; its child is p0 and
        // is blocked by b through a, so b and c are boosted to p0.
        assert_eq!(f.priority(&b), Some(0));
        assert_eq!(f.priority(&c), Some(0));

        // Finished waiters don't boost.
        f.status(&urgent_child, Status::Cancelled);
        assert_eq!(f.priority(&c), Some(3));
        f.status(&a, Status::Cancelled);
        assert_eq!(f.priority(&c), Some(4));
    }

    #[test]
    fn blocked_is_inherited_downward_only() {
        let mut f = Fixture::new();
        let p = f.add(None, None);
        let a = f.add(Some(&p), None);
        let a1 = f.add(Some(&a), None);
        let b = f.add(Some(&p), None);
        let x = f.add(None, None);
        f.dep(&a, &x);
        assert!(f.blocked(&a) && f.blocked(&a1));
        assert!(!f.blocked(&p) && !f.blocked(&b));

        f.status(&x, Status::Cancelled);
        assert!(!f.blocked(&a), "cancelled dependencies are satisfied");
    }

    #[test]
    fn ls_sorts_by_priority_then_age() {
        let mut f = Fixture::new();
        let none = f.add(None, None);
        let old = f.add(None, Some(2));
        let urgent = f.add(None, Some(0));
        f.conn
            .execute(
                "UPDATE tasks SET created_at = created_at + 10 WHERE slug = ?1",
                [&old],
            )
            .unwrap();
        let new = f.add(None, Some(2));
        f.conn
            .execute(
                "UPDATE tasks SET created_at = created_at + 20 WHERE slug = ?1",
                [&new],
            )
            .unwrap();
        assert_eq!(f.ls(), [urgent, old, new, none]);
    }
}
