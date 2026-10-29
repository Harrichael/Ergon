//! Where tasks sit on `ergon kanban`'s board. Its columns are the statuses
//! but cancelled; todo splits into ready and blocked, and landing into how
//! far its PRs have got, as choros reports them.
//!
//! The cards are the tasks `ls` would list, found the same way across every
//! status: a landing or done task is one card, as is a blocked one, and an
//! open task splits into its children where blocked work is under it, and
//! also where landing or done work is, so that work shows in its own
//! column. Or, with `leaves`, every task with no subtasks. Done cards show
//! only if done since a cutoff.

use std::collections::{BTreeMap, HashMap};

use crate::derived::{self, Derived};
use crate::landings::{Landing, Seen};
use crate::task::{Status, Tasks};

/// A place on the board, in board order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Cell {
    Ready,
    Blocked,
    InProgress,
    MergeQueue,
    Draft,
    Waiting,
    Unanswered,
    Approved,
    /// Landing, but no open set carries it, or choros couldn't say.
    NoLanding,
    Done,
}

impl Cell {
    pub fn status(self) -> Status {
        match self {
            Cell::Ready | Cell::Blocked => Status::Todo,
            Cell::InProgress => Status::InProgress,
            Cell::MergeQueue
            | Cell::Draft
            | Cell::Waiting
            | Cell::Unanswered
            | Cell::Approved
            | Cell::NoLanding => Status::Landing,
            Cell::Done => Status::Done,
        }
    }

    /// The heading under its column; `None` for a column that doesn't split.
    pub fn label(self) -> Option<&'static str> {
        Some(match self {
            Cell::Ready => "ready",
            Cell::Blocked => "blocked",
            Cell::MergeQueue => "merge queue",
            Cell::Draft => "PR draft",
            Cell::Waiting => "waiting for comments",
            Cell::Unanswered => "unresponded comments",
            Cell::Approved => "approved",
            Cell::NoLanding => "no open landing",
            Cell::InProgress | Cell::Done => return None,
        })
    }
}

/// The cards in each cell, most urgent first.
pub struct Board {
    pub cells: BTreeMap<Cell, Vec<i64>>,
    /// Done cards left off for being done before the cutoff.
    pub older_done: usize,
}

/// Which done cards show: those done at or after `since`, by `done_at`
/// (when each task with a status of its own last became done).
pub struct DoneSince<'a> {
    pub since: Option<i64>,
    pub done_at: &'a HashMap<i64, i64>,
}

impl Board {
    pub fn compute(
        tasks: &Tasks,
        derived: &Derived,
        leaves: bool,
        landings: &Seen,
        done: DoneSince,
    ) -> Board {
        let mut cards = Vec::new();
        for c in tasks.children(tasks.root().id) {
            if leaves {
                visit_leaves(tasks, c.id, &mut cards);
            } else {
                visit(tasks, derived, c.id, &mut cards);
            }
        }
        let mut cells: BTreeMap<Cell, Vec<i64>> = BTreeMap::new();
        let mut older_done = 0;
        for id in cards {
            let cell = match tasks.effective_status(id) {
                Status::Todo if derived.is_blocked(id) => Cell::Blocked,
                Status::Todo => Cell::Ready,
                Status::InProgress => Cell::InProgress,
                Status::Landing => landing_cell(landings.of(id)),
                Status::Done if done.shows(tasks, id) => Cell::Done,
                Status::Done => {
                    older_done += 1;
                    continue;
                }
                Status::Cancelled => continue,
            };
            cells.entry(cell).or_default().push(id);
        }
        for ids in cells.values_mut() {
            ids.sort_by_key(|&id| derived::urgency(tasks, derived, id));
        }
        Board { cells, older_done }
    }
}

impl DoneSince<'_> {
    /// A task done without a status of its own was done with the nearest
    /// ancestor that has one.
    fn shows(&self, tasks: &Tasks, id: i64) -> bool {
        let Some(since) = self.since else {
            return true;
        };
        std::iter::once(id)
            .chain(tasks.ancestors(id))
            .find(|t| tasks.get(*t).status.is_some())
            .and_then(|t| self.done_at.get(&t))
            .is_some_and(|&at| at >= since)
    }
}

/// As `ls`'s walk, with closed and blocked work kept as cards.
fn visit(tasks: &Tasks, derived: &Derived, id: i64, out: &mut Vec<i64>) {
    let splits = tasks.effective_status(id).is_open()
        && tasks.descendants(id).into_iter().any(|d| match tasks.effective_status(d) {
            Status::Landing | Status::Done => true,
            s => s.is_open() && derived.is_blocked(d) && !derived.is_blocked(id),
        });
    if splits {
        for c in tasks.children(id) {
            visit(tasks, derived, c.id, out);
        }
    } else {
        out.push(id);
    }
}

fn visit_leaves(tasks: &Tasks, id: i64, out: &mut Vec<i64>) {
    let mut children = tasks.children(id).peekable();
    if children.peek().is_none() {
        out.push(id);
    }
    for c in children {
        visit_leaves(tasks, c.id, out);
    }
}

/// A task landing in several sets sits where the one that most needs its
/// author is: a reply due, then a draft to finish, then work still queued,
/// then review awaited; approved only once all its sets are.
fn landing_cell(sets: Option<Vec<&Landing>>) -> Cell {
    let rank = |c: &Cell| match c {
        Cell::Unanswered => 0,
        Cell::Draft => 1,
        Cell::MergeQueue => 2,
        Cell::Waiting => 3,
        _ => 4,
    };
    sets.unwrap_or_default()
        .into_iter()
        .filter(|l| l.is_open())
        .map(|l| match (l.state.as_str(), l.review) {
            ("queued", _) => Cell::MergeQueue,
            (_, Some(r)) if r.unanswered => Cell::Unanswered,
            (_, Some(r)) if r.draft => Cell::Draft,
            (_, Some(r)) if r.approved => Cell::Approved,
            _ => Cell::Waiting,
        })
        .min_by_key(rank)
        .unwrap_or(Cell::NoLanding)
}
