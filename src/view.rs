//! Plain-text renderings of tasks. Output may contain ANSI styles; print it
//! through `anstream`, which strips them when they aren't supported.

use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use anstyle::{AnsiColor, Style};

use crate::derived::{self, Derived, Filter, Sort};
use crate::fmt::timestamp;
use crate::task::Tasks;

const STORED: Style = Style::new().bold();
const INFERRED: Style = AnsiColor::Cyan.on_default();
const DIM: Style = Style::new().dimmed();
const GROUP: Style = Style::new().bold();

/// Effective priority, bold if stored and cyan if inferred.
fn priority(tasks: &Tasks, derived: &Derived, id: i64) -> String {
    match derived.priority(id) {
        None => format!("{DIM}p-{DIM:#}"),
        Some(p) => {
            let style = if tasks.get(id).priority == Some(p) {
                STORED
            } else {
                INFERRED
            };
            format!("{style}p{p}{style:#}")
        }
    }
}

/// The size column, padded to `width`: bold if stored, cyan if added up.
/// Views show it only once some task in the project has a size.
fn size(derived: &Derived, id: i64, width: usize) -> String {
    match derived.size(id) {
        None => format!("{DIM}{:<width$}{DIM:#}", "-"),
        Some(s) => {
            let style = if s.stored { STORED } else { INFERRED };
            format!("{style}{:<width$}{style:#}", s.label())
        }
    }
}

fn size_width(derived: &Derived, ids: impl IntoIterator<Item = i64>) -> usize {
    ids.into_iter()
        .filter_map(|id| derived.size(id))
        .map(|s| s.label().len())
        .max()
        .unwrap_or(1)
}

/// The attention column, `a7`: bold if stored, cyan if inherited or raised
/// by subtasks, and a dimmed `a?` when unknown. Views show it only once some
/// task in the project has an attention.
fn attention(tasks: &Tasks, derived: &Derived, id: i64) -> String {
    match derived.attention(id) {
        None => format!("{DIM}{:<3}{DIM:#}", "a?"),
        Some(a) => {
            let style = if tasks.get(id).attention == Some(a) {
                STORED
            } else {
                INFERRED
            };
            format!("{style}{:<3}{style:#}", format!("a{a}"))
        }
    }
}

fn attention_column(tasks: &Tasks, derived: &Derived, id: i64) -> String {
    if derived.rated() {
        format!("{}  ", attention(tasks, derived, id))
    } else {
        String::new()
    }
}

fn status(tasks: &Tasks, id: i64) -> String {
    format!("{:<11}", tasks.effective_status(id).as_str())
}

/// One task in full.
pub fn show(tasks: &Tasks, derived: &Derived, id: i64, now: i64) -> String {
    let t = tasks.get(id);
    let mut out = String::new();
    let _ = writeln!(out, "{}  {}", tasks.reference(id), t.title());

    let inherited = if t.status.is_none() {
        " (inherited)"
    } else {
        ""
    };
    let _ = writeln!(out, "status:    {}{inherited}", tasks.effective_status(id));

    let note = match (t.priority, derived.priority(id)) {
        (_, None) => String::new(),
        (Some(s), Some(e)) if s == e => String::new(),
        (Some(s), Some(_)) => format!(" (boosted from stored p{s})"),
        (None, Some(_)) => " (inferred)".to_string(),
    };
    let _ = writeln!(out, "priority:  {}{note}", priority(tasks, derived, id));
    if derived.sized() {
        let line = match derived.size(id) {
            Some(s) if s.stored && s.left == 0 => format!("{} (finished)", s.total),
            Some(s) if s.stored => s.total.to_string(),
            Some(s) => format!(
                "{} left of {}{} (added up from subtasks)",
                s.left,
                s.total,
                if s.partial { ", plus unsized work" } else { "" }
            ),
            None => match tasks
                .ancestors(id)
                .into_iter()
                .find(|&a| tasks.get(a).size.is_some())
            {
                Some(a) => format!("- (a step of {})", tasks.reference(a)),
                None => "-".to_string(),
            },
        };
        let _ = writeln!(out, "size:      {line}");
    }
    if derived.rated() {
        let line = match (t.attention, derived.attention(id)) {
            (_, None) => "? (unknown)".to_string(),
            (Some(s), Some(e)) if s == e => e.to_string(),
            (Some(s), Some(e)) => format!("{e} (raised from stored {s} by subtasks)"),
            (None, Some(e)) => format!("{e} (inferred)"),
        };
        let _ = writeln!(out, "attention: {line}");
    }

    let waiting_on = derived::blocked_by(tasks, id);
    if waiting_on.is_empty() {
        let _ = writeln!(out, "blocked:   no");
    } else {
        let refs: Vec<String> = waiting_on.iter().map(|&d| tasks.reference(d)).collect();
        let _ = writeln!(out, "blocked:   yes, waiting on {}", refs.join(", "));
    }

    if let Some(p) = t.parent_id {
        let _ = writeln!(
            out,
            "parent:    {}  {}",
            tasks.reference(p),
            tasks.get(p).title()
        );
    }
    let _ = writeln!(out, "created:   {}", timestamp(t.created_at, now));
    let _ = writeln!(out, "updated:   {}", timestamp(t.updated_at, now));
    if let (Some(session), Some(cwd)) = (&t.claude_session, &t.claude_cwd) {
        let _ = writeln!(out, "session:   {session} in {cwd}");
    }
    if !t.is_root() {
        let _ = writeln!(
            out,
            "delete:    ergon rm {} --subtasks {} --dependents {}",
            tasks.reference(id),
            tasks.descendants(id).len(),
            tasks.outside_dependents(id).len()
        );
    }
    let _ = writeln!(out, "\n{}", t.intent);

    let sections = [
        ("Subtasks", tasks.children(id).collect::<Vec<_>>()),
        ("Depends on", tasks.dependencies(id)),
        ("Dependents", tasks.dependents(id)),
    ];
    for (heading, list) in sections {
        if list.is_empty() {
            continue;
        }
        let _ = writeln!(out, "\n{heading}:");
        for t in list {
            let _ = writeln!(
                out,
                "  - {}  {}  {}",
                t.slug,
                status(tasks, t.id),
                t.title()
            );
        }
    }
    out
}

/// The subtree under `id` as indented titles. With `mark`, the tasks in it
/// get a `▸` gutter and a bold title.
pub fn tree(tasks: &Tasks, derived: &Derived, id: i64, mark: Option<&HashSet<i64>>) -> String {
    let width = derived
        .sized()
        .then(|| size_width(derived, std::iter::once(id).chain(tasks.descendants(id))));
    let mut out = String::new();
    let row = Row {
        tasks,
        derived,
        width,
        mark,
    };
    write_tree(&row, id, 0, &mut out);
    out
}

struct Row<'a> {
    tasks: &'a Tasks,
    derived: &'a Derived,
    width: Option<usize>,
    mark: Option<&'a HashSet<i64>>,
}

fn write_tree(row: &Row, id: i64, depth: usize, out: &mut String) {
    let Row {
        tasks,
        derived,
        width,
        mark,
    } = *row;
    let t = tasks.get(id);
    let blocked = if derived.is_blocked(id) {
        format!("  {DIM}blocked{DIM:#}")
    } else {
        String::new()
    };
    let size = width.map_or(String::new(), |w| format!("{}  ", size(derived, id, w)));
    let (gutter, title) = match mark {
        Some(m) if m.contains(&id) => ("▸ ", format!("{STORED}{}{STORED:#}", t.title())),
        Some(_) => ("  ", t.title().to_string()),
        None => ("", t.title().to_string()),
    };
    let _ = writeln!(
        out,
        "{gutter}{}{}  {}  {}  {size}{}{title}{blocked}",
        "  ".repeat(depth),
        t.slug,
        status(tasks, id),
        priority(tasks, derived, id),
        attention_column(tasks, derived, id),
    );
    for c in tasks.children(id) {
        write_tree(row, c.id, depth + 1, out);
    }
}

/// `ergon ls`: one row per task ready to start that `filter` keeps. With
/// `subtasks`, each is followed by its subtasks, indented. `nest` puts rows
/// under their ancestors. With `intent`, each task's body follows its row.
pub fn ls(
    tasks: &Tasks,
    derived: &Derived,
    sort: Sort,
    filter: &Filter,
    subtasks: bool,
    nest: Nest,
    intent: bool,
) -> String {
    let mut ids = derived::ls(tasks, derived, sort);
    ids.retain(|&id| filter.keeps(tasks, derived, id));
    let shown = |id: i64| {
        let below = if subtasks {
            tasks.descendants(id)
        } else {
            Vec::new()
        };
        std::iter::once(id).chain(below)
    };
    let width = derived
        .sized()
        .then(|| size_width(derived, ids.iter().flat_map(|&id| shown(id))));
    let members = match nest {
        Nest::Flat => HashMap::from([(None, ids)]),
        _ => group_members(tasks, &ids, nest),
    };
    let view = Ls {
        tasks,
        derived,
        width,
        subtasks,
        intent,
        members,
    };
    let mut out = String::new();
    view.write(None, 0, &mut out);
    out
}

/// Which ancestors `ergon ls` shows its rows under.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Nest {
    Flat,
    /// Only where two or more listed tasks meet: `--groups`.
    Groups,
    /// All of them, from the root down: `--ancestors`.
    Ancestors,
}

/// What each group holds, groups and tasks alike, in the order their first
/// task is listed; `None` is the top level. With `Nest::Groups` an ancestor
/// groups when listed tasks reach it through two or more of its children.
fn group_members(tasks: &Tasks, ids: &[i64], nest: Nest) -> HashMap<Option<i64>, Vec<i64>> {
    let mut branches: HashMap<i64, HashSet<i64>> = HashMap::new();
    for &id in ids {
        let mut below = id;
        for a in tasks.ancestors(id) {
            if tasks.get(a).is_root() {
                break;
            }
            branches.entry(a).or_default().insert(below);
            below = a;
        }
    }
    let shown = |a: &i64| match nest {
        Nest::Ancestors => true,
        _ => branches.get(a).is_some_and(|b| b.len() > 1),
    };
    let mut members: HashMap<Option<i64>, Vec<i64>> = HashMap::new();
    for &id in ids {
        let mut path: Vec<i64> = tasks.ancestors(id).into_iter().filter(shown).collect();
        path.reverse();
        path.push(id);
        let mut group = None;
        for node in path {
            let list = members.entry(group).or_default();
            if !list.contains(&node) {
                list.push(node);
            }
            group = Some(node);
        }
    }
    members
}

struct Ls<'a> {
    tasks: &'a Tasks,
    derived: &'a Derived,
    width: Option<usize>,
    subtasks: bool,
    intent: bool,
    members: HashMap<Option<i64>, Vec<i64>>,
}

impl Ls<'_> {
    fn write(&self, group: Option<i64>, depth: usize, out: &mut String) {
        let pad = "  ".repeat(depth);
        for &id in self.members.get(&group).into_iter().flatten() {
            if self.members.contains_key(&Some(id)) {
                let t = self.tasks.get(id);
                let _ = writeln!(out, "{pad}{}  {GROUP}{}{GROUP:#}", t.slug, t.title());
                self.write(Some(id), depth + 1, out);
                continue;
            }
            let row = ls_row(self.tasks, self.derived, id, self.width, group);
            let _ = writeln!(out, "{pad}{row}");
            self.write_body(id, depth + 1, out);
            if self.subtasks {
                for c in self.tasks.children(id) {
                    self.write_subtask(c.id, depth + 1, out);
                }
            }
        }
    }

    fn write_subtask(&self, id: i64, depth: usize, out: &mut String) {
        let (tasks, derived) = (self.tasks, self.derived);
        let t = tasks.get(id);
        let size = self
            .width
            .map_or(String::new(), |w| format!("{}  ", size(derived, id, w)));
        let blocked = if derived.is_blocked(id) {
            format!("  {DIM}blocked{DIM:#}")
        } else {
            String::new()
        };
        let _ = writeln!(
            out,
            "{}{}  {}  {}  {size}{}{}{blocked}",
            "  ".repeat(depth),
            t.slug,
            priority(tasks, derived, id),
            status(tasks, id),
            attention_column(tasks, derived, id),
            t.title()
        );
        self.write_body(id, depth + 1, out);
        for c in tasks.children(id) {
            self.write_subtask(c.id, depth + 1, out);
        }
    }

    /// The intent past its title, behind a `│` gutter. `ergon work -` skips
    /// gutter lines, so `ls --intent` still pipes into it.
    fn write_body(&self, id: i64, depth: usize, out: &mut String) {
        if !self.intent {
            return;
        }
        let intent = self.tasks.get(id).intent.trim();
        let Some((_, body)) = intent.split_once('\n') else {
            return;
        };
        let pad = "  ".repeat(depth);
        for line in body.trim_start_matches(['\n', '\r']).trim_end().lines() {
            let line = line.trim_end();
            if line.is_empty() {
                let _ = writeln!(out, "{pad}{DIM}│{DIM:#}");
            } else {
                let _ = writeln!(out, "{pad}{DIM}│{DIM:#} {line}");
            }
        }
    }
}

/// The rows of `ergon ls`, each with its task, without line breaks.
pub fn ls_rows(tasks: &Tasks, derived: &Derived, sort: Sort) -> Vec<(i64, String)> {
    let ids = derived::ls(tasks, derived, sort);
    let width = derived
        .sized()
        .then(|| size_width(derived, ids.iter().copied()));
    ids.into_iter()
        .map(|id| (id, ls_row(tasks, derived, id, width, None)))
        .collect()
}

/// One `ls` row; its parent is named unless it's the root or the `group`
/// the row is shown under.
fn ls_row(
    tasks: &Tasks,
    derived: &Derived,
    id: i64,
    width: Option<usize>,
    group: Option<i64>,
) -> String {
    let t = tasks.get(id);
    let context = match t.parent_id.map(|p| tasks.get(p)) {
        Some(p) if !p.is_root() && Some(p.id) != group => {
            format!("  {DIM}(in: {}){DIM:#}", p.title())
        }
        _ => String::new(),
    };
    let size = width.map_or(String::new(), |w| format!("{}  ", size(derived, id, w)));
    format!(
        "{}  {}  {}  {size}{}{}{context}",
        t.slug,
        priority(tasks, derived, id),
        status(tasks, id),
        attention_column(tasks, derived, id),
        t.title()
    )
}
