//! The order open work can run in, for `ergon gantt`. There are no dates:
//! work starts once everything it waits on has finished, with no limit on
//! how much runs at once, and the axis counts size (or attention).
//!
//! The pieces placed are open tasks with nothing open under them. A sized
//! task is one unit of work, so its steps are the exception: they run one
//! after another, sharing its size evenly, so they add back up to it. Work
//! that waits on a step waits only on that step's share. Landing, done and
//! cancelled work hold nothing back, as in `ls`.

use std::collections::{HashMap, HashSet};

use crate::derived::{self, Derived};
use crate::task::Tasks;

/// What a piece's length on the axis comes from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Length {
    #[default]
    Size,
    Attention,
}

/// Where a piece runs on the axis. `known` is false for work with no size
/// (or no attention), which runs for 1 so it still shows. A step's share of
/// its sized task can start and end between whole numbers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Span {
    pub start: f64,
    pub end: f64,
    pub known: bool,
}

/// Every open piece of work in the project, placed on the axis.
pub struct Schedule {
    spans: HashMap<i64, Span>,
}

/// A piece of work to place, and the sized task it's a step of, if any.
struct Piece {
    id: i64,
    sized: Option<i64>,
    length: f64,
    known: bool,
}

impl Schedule {
    pub fn compute(tasks: &Tasks, derived: &Derived, length: Length) -> Schedule {
        let length = |t: i64| {
            let n = match length {
                Length::Size => tasks.get(t).size.map(u64::from),
                Length::Attention => derived.attention(t).map(u64::from),
            };
            (n.unwrap_or(1) as f64, n.is_some())
        };
        let mut pieces = Vec::new();
        for c in tasks.children(tasks.root().id) {
            find_pieces(tasks, c.id, &length, &mut pieces);
        }

        let ids: HashSet<i64> = pieces.iter().map(|p| p.id).collect();
        let waits: HashMap<i64, Vec<i64>> = pieces
            .iter()
            .map(|p| {
                let mut on: Vec<i64> = derived::blocked_by(tasks, p.id)
                    .into_iter()
                    .flat_map(|d| {
                        if ids.contains(&d) {
                            vec![d]
                        } else {
                            ids.iter()
                                .copied()
                                .filter(|&q| tasks.is_ancestor(d, q))
                                .collect()
                        }
                    })
                    .collect();
                on.sort();
                on.dedup();
                (p.id, on)
            })
            .collect();

        // Placed by time, earliest first, among the pieces whose waits are all
        // placed: anything else waits on one of those, so it can't start
        // sooner. A sized task's steps are thus ordered only once what they
        // wait on is placed, and a step never runs before what it waits on.
        // Ties go to tree order. Greedy, so a valid order but not always the
        // tightest: a long step ready first can hold up a step ready just
        // after it that other work waits on.
        let mut spans: HashMap<i64, Span> = HashMap::new();
        let mut free: HashMap<i64, f64> = HashMap::new();
        while spans.len() < pieces.len() {
            let (start, p) = pieces
                .iter()
                .filter(|p| !spans.contains_key(&p.id))
                .filter_map(|p| {
                    let after = waits[&p.id]
                        .iter()
                        .map(|w| spans.get(w).map(|s| s.end))
                        .collect::<Option<Vec<f64>>>()?;
                    let busy = p.sized.and_then(|s| free.get(&s).copied());
                    let start = after.into_iter().chain(busy).fold(0.0, f64::max);
                    Some((start, p))
                })
                .reduce(|best, next| if next.0 < best.0 { next } else { best })
                .expect("ergon refuses tasks that wait on each other");
            let end = start + p.length;
            spans.insert(
                p.id,
                Span {
                    start,
                    end,
                    known: p.known,
                },
            );
            if let Some(s) = p.sized {
                free.insert(s, end);
            }
        }
        Schedule { spans }
    }

    /// The spans that make up a task: its own, or every one under it. Empty
    /// for a task with no open work.
    pub fn parts(&self, tasks: &Tasks, id: i64) -> Vec<Span> {
        if let Some(s) = self.spans.get(&id) {
            return vec![*s];
        }
        let mut out: Vec<Span> = tasks
            .descendants(id)
            .into_iter()
            .filter_map(|d| self.spans.get(&d).copied())
            .collect();
        out.sort_by(|a, b| a.start.total_cmp(&b.start));
        out
    }
}

fn find_pieces(tasks: &Tasks, id: i64, length: &impl Fn(i64) -> (f64, bool), out: &mut Vec<Piece>) {
    let open = |t: i64| tasks.effective_status(t).is_open();
    if !open(id) {
        return;
    }
    if tasks.get(id).size.is_some() {
        let steps: Vec<i64> = tasks
            .descendants(id)
            .into_iter()
            .filter(|&t| open(t) && !tasks.children(t).any(|c| open(c.id)))
            .collect();
        let (n, known) = length(id);
        if steps.is_empty() {
            out.push(Piece {
                id,
                sized: None,
                length: n,
                known,
            });
            return;
        }
        let share = n / steps.len() as f64;
        out.extend(steps.into_iter().map(|t| Piece {
            id: t,
            sized: Some(id),
            length: share,
            known,
        }));
        return;
    }
    let children: Vec<i64> = tasks
        .children(id)
        .map(|c| c.id)
        .filter(|&c| open(c))
        .collect();
    if children.is_empty() {
        let (n, known) = length(id);
        out.push(Piece {
            id,
            sized: None,
            length: n,
            known,
        });
    }
    for c in children {
        find_pieces(tasks, c, length, out);
    }
}
