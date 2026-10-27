//! Values computed from a project snapshot: blocked, effective priority,
//! sizes, attention, and the `ls` selection.

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::{Result, bail};

use crate::task::{Status, Tasks};

/// Blocked state, effective priority, size and attention of every live task.
pub struct Derived {
    blocked: HashSet<i64>,
    priority: HashMap<i64, Option<u8>>,
    size: HashMap<i64, Size>,
    attention: HashMap<i64, Option<u8>>,
    rated: bool,
}

/// A task's size: its own, or the sum of the sized tasks under it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Size {
    /// What isn't done or landing yet.
    pub left: u64,
    pub total: u64,
    /// Some work under it has no size, so the sums are a floor.
    pub partial: bool,
    /// The task's own size, not a sum.
    pub stored: bool,
}

impl Size {
    /// `5`, or `8/13` once some is done, and `+` when partial.
    pub fn label(&self) -> String {
        let n = if self.left == self.total {
            self.total.to_string()
        } else {
            format!("{}/{}", self.left, self.total)
        };
        if self.partial { n + "+" } else { n }
    }
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

        let mut size = HashMap::new();
        compute_size(tasks, root, &mut size);

        let mut attention = HashMap::new();
        compute_attention(tasks, root, None, &mut attention);
        let rated = attention
            .keys()
            .any(|&id| tasks.get(id).attention.is_some());

        Derived {
            blocked,
            priority,
            size,
            attention,
            rated,
        }
    }

    pub fn is_blocked(&self, id: i64) -> bool {
        self.blocked.contains(&id)
    }

    pub fn priority(&self, id: i64) -> Option<u8> {
        self.priority.get(&id).copied().flatten()
    }

    /// `None` for a task with no size and nothing sized under it, and for
    /// the steps of a sized task.
    pub fn size(&self, id: i64) -> Option<Size> {
        self.size.get(&id).copied()
    }

    /// Whether any task has a size, which is when views show them.
    pub fn sized(&self) -> bool {
        !self.size.is_empty()
    }

    /// `None` when unknown.
    pub fn attention(&self, id: i64) -> Option<u8> {
        self.attention.get(&id).copied().flatten()
    }

    /// Whether any task has an attention, which is when views show them.
    pub fn rated(&self) -> bool {
        self.rated
    }
}

/// A task's attention is its own, else its parent's, raised to the highest
/// among its children that aren't done or cancelled. With none of its own or
/// from above, it's unknown unless it has such children and all are known:
/// one unknown subtask makes an unrated parent unknown too.
fn compute_attention(
    tasks: &Tasks,
    id: i64,
    inherited: Option<u8>,
    out: &mut HashMap<i64, Option<u8>>,
) -> Option<u8> {
    let own = tasks.get(id).attention.or(inherited);
    let mut highest = own;
    let (mut open, mut unknown) = (false, false);
    for c in tasks.children(id) {
        let child = compute_attention(tasks, c.id, own, out);
        if tasks.effective_status(c.id).is_closed() {
            continue;
        }
        open = true;
        unknown |= child.is_none();
        highest = highest.max(child);
    }
    let attention = if own.is_none() && (!open || unknown) {
        None
    } else {
        highest
    };
    out.insert(id, attention);
    attention
}

/// A sized task counts its own size and nothing under it. An unsized one adds
/// up its children; a child with nothing sized at all is unsized work, which
/// makes the sums partial. Cancelled work counts for nothing.
fn compute_size(tasks: &Tasks, id: i64, out: &mut HashMap<i64, Size>) -> Option<Size> {
    let status = tasks.effective_status(id);
    if let Some(n) = tasks.get(id).size {
        let size = Size {
            left: if status.is_open() { n as u64 } else { 0 },
            total: n as u64,
            partial: false,
            stored: true,
        };
        out.insert(id, size);
        return Some(size);
    }
    let mut sum: Option<Size> = None;
    let mut gap = false;
    for c in tasks.children(id) {
        let child = compute_size(tasks, c.id, out);
        if tasks.effective_status(c.id) == Status::Cancelled {
            continue;
        }
        match child {
            None => gap = true,
            Some(c) => {
                let s = sum.get_or_insert(Size {
                    left: 0,
                    total: 0,
                    partial: false,
                    stored: false,
                });
                s.left += c.left;
                s.total += c.total;
                s.partial |= c.partial;
            }
        }
    }
    let mut sum = sum?;
    sum.partial |= gap;
    out.insert(id, sum);
    Some(sum)
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

/// What `ls` orders by. Each field sorts ascending unless `descending`;
/// tasks without a value go last either way, and ties fall back to the
/// default: most urgent, then oldest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sort {
    pub field: SortField,
    pub descending: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortField {
    #[default]
    Priority,
    Attention,
    Size,
    Created,
    Updated,
}

impl Sort {
    pub const FIELDS: &str = "priority, attention, size, created or updated";

    pub fn parse(field: &str, order: Option<&str>) -> Result<Sort> {
        let field = match field {
            "priority" => SortField::Priority,
            "attention" => SortField::Attention,
            "size" => SortField::Size,
            "created" => SortField::Created,
            "updated" => SortField::Updated,
            _ => bail!("can't sort by {field:?}: expected {}", Sort::FIELDS),
        };
        let descending = match order {
            None | Some("asc") => false,
            Some("desc") => true,
            Some(o) => bail!("invalid sort order {o:?}: expected asc or desc"),
        };
        Ok(Sort { field, descending })
    }

    fn key(self, tasks: &Tasks, derived: &Derived, id: i64) -> Option<i64> {
        let t = tasks.get(id);
        match self.field {
            SortField::Priority => derived.priority(id).map(i64::from),
            SortField::Attention => derived.attention(id).map(i64::from),
            SortField::Size => derived.size(id).map(|s| s.left as i64),
            SortField::Created => Some(t.created_at),
            SortField::Updated => Some(t.updated_at),
        }
    }
}

/// The coarsest open, unblocked work, in `sort` order.
pub fn ls(tasks: &Tasks, derived: &Derived, sort: Sort) -> Vec<i64> {
    let mut out = Vec::new();
    for c in tasks.children(tasks.root().id) {
        visit(tasks, derived, c.id, &mut out);
    }
    let fallback = |id: i64| {
        let t = tasks.get(id);
        (derived.priority(id).unwrap_or(5), t.created_at, t.id)
    };
    out.sort_by(|&a, &b| {
        let by_field = match (sort.key(tasks, derived, a), sort.key(tasks, derived, b)) {
            (Some(x), Some(y)) if sort.descending => y.cmp(&x),
            (Some(x), Some(y)) => x.cmp(&y),
            (a, b) => b.is_some().cmp(&a.is_some()),
        };
        by_field.then_with(|| fallback(a).cmp(&fallback(b)))
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
    use crate::ops::{self, Changes, Fields};
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
            let fields = Fields {
                priority,
                ..Default::default()
            };
            ops::add(&mut self.conn, &self.project, parent, "Task", fields).unwrap()
        }

        fn sized(&mut self, parent: Option<&str>, size: u32) -> String {
            let fields = Fields {
                size: Some(size),
                ..Default::default()
            };
            ops::add(&mut self.conn, &self.project, parent, "Task", fields).unwrap()
        }

        /// `left/total`, with `+` when partial and `*` when stored.
        fn size(&self, slug: &str) -> Option<String> {
            let (tasks, d) = self.eval();
            d.size(tasks.live(slug).unwrap().id).map(|s| {
                format!(
                    "{}/{}{}{}",
                    s.left,
                    s.total,
                    if s.partial { "+" } else { "" },
                    if s.stored { "*" } else { "" }
                )
            })
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

        fn rate(&mut self, t: &str, attention: u8) {
            let changes = Changes {
                attention: Some(Some(attention)),
                ..Default::default()
            };
            ops::edit(&mut self.conn, &self.project, t, changes).unwrap();
        }

        fn attention(&self, slug: &str) -> Option<u8> {
            let (tasks, d) = self.eval();
            d.attention(tasks.live(slug).unwrap().id)
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
            ls(&tasks, &d, Sort::default())
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

    /// A sized task is one unit of work: its steps carry nothing. Anything
    /// above sums the units under it, tells left from total once some are
    /// done, drops cancelled ones, and flags unsized work as a floor.
    #[test]
    fn sizes_add_up_from_the_units_of_work() {
        let mut f = Fixture::new();
        let epic = f.add(None, None);
        let api = f.sized(Some(&epic), 5);
        let step = f.add(Some(&api), None);
        let ui = f.sized(Some(&epic), 8);
        let docs = f.add(Some(&epic), None);
        assert_eq!(f.size(&api).as_deref(), Some("5/5*"));
        assert_eq!(f.size(&step), None, "a step of a unit");
        assert_eq!(f.size(&epic).as_deref(), Some("13/13+"), "docs has no size");

        f.status(&api, Status::Done);
        f.status(&docs, Status::Cancelled);
        assert_eq!(f.size(&epic).as_deref(), Some("8/13"));
        f.status(&ui, Status::Landing);
        assert_eq!(f.size(&epic).as_deref(), Some("0/13"));
        assert_eq!(
            f.size(&f.eval().0.root().slug.clone()).as_deref(),
            Some("0/13")
        );
    }

    /// Attention passes down to unrated subtasks and up as the highest of the
    /// open work below. An unrated task is unknown while anything open under
    /// it is; a rated one never is. Done and cancelled work keeps its own
    /// rating but stops raising its parent's.
    #[test]
    fn attention_passes_down_and_the_highest_goes_up() {
        let mut f = Fixture::new();
        let root = f.eval().0.root().slug.clone();
        let epic = f.add(None, None);
        let api = f.add(Some(&epic), None);
        let ui = f.add(Some(&epic), None);
        let ui_step = f.add(Some(&ui), None);
        f.rate(&api, 3);
        assert_eq!(f.attention(&epic), None, "ui is unrated");
        assert_eq!(f.attention(&root), None);

        f.rate(&ui, 2);
        assert_eq!(f.attention(&ui_step), Some(2), "inherited");
        assert_eq!(f.attention(&epic), Some(3));
        assert_eq!(f.attention(&root), Some(3));

        let choice = f.add(Some(&api), None);
        assert_eq!(f.attention(&choice), Some(3), "inherited");
        f.rate(&choice, 9);
        assert_eq!(f.attention(&api), Some(9), "raised from its own 3");
        assert_eq!(f.attention(&epic), Some(9));

        f.status(&choice, Status::Done);
        assert_eq!(f.attention(&choice), Some(9), "keeps its own");
        assert_eq!(f.attention(&api), Some(3));

        let unrated = f.add(Some(&epic), None);
        assert_eq!(f.attention(&epic), None);
        f.status(&unrated, Status::Cancelled);
        assert_eq!(f.attention(&epic), Some(3));
        f.status(&epic, Status::Done);
        assert_eq!(f.attention(&epic), None, "nothing open under it");

        f.rate(&root, 1);
        assert_eq!(f.attention(&root), Some(1), "rated, so not unknown");
        assert_eq!(f.attention(&epic), Some(1), "inherited");
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
