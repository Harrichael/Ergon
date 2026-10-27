//! The outline: a project's tasks as an editable table, one row per task,
//! and applying an edited outline back to the project in one transaction.
//!
//! ```text
//! # ergon demo 1a2b3c4d
//! root  in_progress  ~in_progress  -   -    -  ~8/13+  -   ~a7  -     -         Root
//! qxpj  todo         ~todo         p1  ~p1  -  ~8/13+  -   ~a7  -     -         - Backend
//! FTFE  -            ~todo         -   ~p1  5  ~5      a7  ~a7  -     -           - Write the API
//! pQyk  todo         ~todo         -   -    -  -       a2  ~a2  FTFE  ~blocked  - Frontend
//! ```
//!
//! A row is eleven cells, then the title: slug, status, ~status, priority,
//! ~priority, size, ~size, attention, ~attention, deps (comma-separated slugs)
//! and ~blocked. `-` is an empty cell, so a row splits by position whatever it
//! holds; an empty ~attention is unknown. Cells starting
//! with `~` are calculated, and editing one is refused. A new task's slug is
//! `-`, and its calculated cells are ignored.
//!
//! The title is the intent's first line. The root's comes first and bare;
//! every other task's starts with `- `, indented two spaces per level of
//! nesting under the root. A task nests under the nearest row above with a
//! less indented title. Indentation counts from the end of the ~blocked
//! column, which has a fixed width: a cell edited wider shifts its whole row,
//! and its nesting with it, rather than re-nesting the task.
//!
//! Errors about a row start `line N, <cell>:` when one cell is at fault, the
//! cell named by its column (`status`, `~size`, `title`, ...), so an editor
//! can point at it.
//!
//! The header names the project and a version: a hash of the rest of the
//! text as rendered. An outline edited while the project changed underneath
//! it is refused, since applying it would undo those changes.

use std::collections::{BTreeSet, HashMap, HashSet};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::derived::Derived;
use crate::project::Project;
use crate::slug;
use crate::task::{Status, Tasks, parse_attention, parse_priority, parse_size};

const BLOCKED: &str = "~blocked";

/// Cells before the title: slug, status, ~status, priority, ~priority, size,
/// ~size, attention, ~attention, deps, ~blocked.
const CELLS: usize = 11;

/// Which cells are calculated, with their names for errors.
const CALCULATED: [(usize, &str, &str); 5] = [
    (2, "~status", "status"),
    (4, "~priority", "priority"),
    (6, "~size", "size"),
    (8, "~attention", "attention"),
    (10, "~blocked", "deps"),
];

/// The intent's first line, uncapped: the part of the intent an outline row
/// shows and edits.
fn first_line(intent: &str) -> &str {
    intent.trim_start().lines().next().unwrap_or("").trim_end()
}

/// `intent` with its first line replaced by `title`.
fn replace_first_line(intent: &str, title: &str) -> String {
    match intent.trim_start().split_once('\n') {
        Some((_, rest)) => format!("{title}\n{rest}"),
        None => title.to_string(),
    }
}

/// FNV-1a, folded to 32 bits. Stable across builds, unlike `DefaultHasher`.
fn version(body: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in body.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x0100_0000_01b3);
    }
    format!("{:08x}", (h ^ (h >> 32)) as u32)
}

fn or_dash(s: Option<String>) -> String {
    s.unwrap_or_else(|| "-".into())
}

/// A task's cells, as rendered.
fn cells(tasks: &Tasks, derived: &Derived, id: i64) -> [String; CELLS] {
    let t = tasks.get(id);
    let deps: Vec<&str> = tasks
        .dependencies(id)
        .iter()
        .map(|d| d.slug.as_str())
        .collect();
    [
        t.slug.clone(),
        or_dash(t.status.map(|s| s.to_string())),
        format!("~{}", tasks.effective_status(id)),
        or_dash(t.priority.map(|p| format!("p{p}"))),
        or_dash(derived.priority(id).map(|p| format!("~p{p}"))),
        or_dash(t.size.map(|n| n.to_string())),
        or_dash(derived.size(id).map(|s| format!("~{}", s.label()))),
        or_dash(t.attention.map(|a| format!("a{a}"))),
        or_dash(derived.attention(id).map(|a| format!("~a{a}"))),
        or_dash((!deps.is_empty()).then(|| deps.join(","))),
        if derived.is_blocked(id) { BLOCKED } else { "-" }.to_string(),
    ]
}

pub struct Rendered {
    pub text: String,
    pub version: String,
    /// The task on each row after the header, in order: the root first.
    pub ids: Vec<i64>,
}

pub fn render(tasks: &Tasks) -> Rendered {
    let derived = Derived::compute(tasks);
    let root = tasks.root().id;
    let ids: Vec<i64> = std::iter::once(root)
        .chain(tasks.descendants(root))
        .collect();
    let rows: Vec<[String; CELLS]> = ids.iter().map(|&id| cells(tasks, &derived, id)).collect();

    let laid: Vec<(Vec<&str>, usize, &str)> = rows
        .iter()
        .zip(&ids)
        .map(|(row, &id)| {
            let cells = row.iter().map(String::as_str).collect();
            (
                cells,
                tasks.ancestors(id).len(),
                first_line(&tasks.get(id).intent),
            )
        })
        .collect();
    let body = lay_out(&laid);
    let version = version(&body);
    Rendered {
        text: format!("# ergon {} {version}\n{body}", tasks.project.name),
        version,
        ids,
    }
}

/// Rows of cells, depth and title, with every column lined up.
fn lay_out(rows: &[(Vec<&str>, usize, &str)]) -> String {
    let mut widths = [0; CELLS];
    for (cells, ..) in rows {
        for (w, c) in widths.iter_mut().zip(cells) {
            *w = (*w).max(c.chars().count());
        }
    }
    widths[CELLS - 1] = BLOCKED.len();
    let mut out = String::new();
    for (cells, depth, title) in rows {
        for (c, w) in cells.iter().zip(widths) {
            out.push_str(&format!("{c:<w$}  "));
        }
        if *depth > 0 {
            out.push_str(&"  ".repeat(depth - 1));
            out.push_str("- ");
        }
        out.push_str(title);
        out.push('\n');
    }
    out
}

/// An edited outline lined up again, as written: nothing is checked against
/// the project or applied. Titles are re-indented from the nesting they
/// show, and blank lines are dropped.
pub fn format(text: &str) -> Result<String> {
    let header = text.lines().next().unwrap_or("");
    let mut rows: Vec<(Vec<&str>, usize, &str)> = Vec::new();
    let mut levels: Vec<usize> = Vec::new();
    for (i, raw) in text.lines().enumerate().skip(1) {
        if raw.trim().is_empty() {
            continue;
        }
        let split = split_row(i + 1, raw)?;
        let depth = if split.level == 0 {
            levels.clear();
            0
        } else {
            while levels.last().is_some_and(|&l| l >= split.level) {
                levels.pop();
            }
            levels.push(split.level);
            levels.len()
        };
        rows.push((split.cells, depth, split.title));
    }
    Ok(format!("{header}\n{}", lay_out(&rows)))
}

#[derive(Serialize)]
struct Json<'a> {
    project: &'a str,
    version: &'a str,
    text: &'a str,
    tasks: Vec<JsonTask<'a>>,
}

#[derive(Serialize)]
struct JsonTask<'a> {
    /// 1-based line number in `text`.
    line: usize,
    slug: &'a str,
}

/// The outline as JSON, with which task is on each row, for editors.
pub fn json(tasks: &Tasks) -> String {
    let r = render(tasks);
    let json = Json {
        project: &tasks.project.name,
        version: &r.version,
        text: &r.text,
        tasks: r
            .ids
            .iter()
            .enumerate()
            .map(|(i, &id)| JsonTask {
                line: i + 2,
                slug: &tasks.get(id).slug,
            })
            .collect(),
    };
    serde_json::to_string(&json).expect("serializable") + "\n"
}

/// The project name and version from an outline's header.
pub fn header(text: &str) -> Result<(String, String)> {
    let first = text.lines().next().unwrap_or("");
    let words: Vec<&str> = first.split_whitespace().collect();
    match words.as_slice() {
        ["#", "ergon", name, version] => Ok((name.to_string(), version.to_string())),
        _ => bail!("line 1: expected the header `# ergon <project> <version>`"),
    }
}

#[derive(Debug, PartialEq)]
struct Line {
    no: usize,
    /// 0 for the root's bare title; otherwise one more than the spaces
    /// before its `- `. Only the order matters.
    level: usize,
    /// `None` for a new task.
    slug: Option<String>,
    status: Option<Status>,
    priority: Option<u8>,
    size: Option<u32>,
    attention: Option<u8>,
    deps: Vec<String>,
    /// The calculated cells as written, by cell index.
    calculated: Vec<(usize, String)>,
    title: String,
}

fn cell<T>(no: usize, name: &str, c: &str, parse: impl Fn(&str) -> Result<T>) -> Result<Option<T>> {
    if c == "-" {
        return Ok(None);
    }
    parse(c)
        .map(Some)
        .with_context(|| format!("line {no}, {name}"))
}

/// A row split into its cells, its title, and the title's level: 0 for
/// a bare title (the root's), otherwise one more than its indentation.
struct Split<'a> {
    cells: Vec<&'a str>,
    level: usize,
    title: &'a str,
}

fn split_row(no: usize, raw: &str) -> Result<Split<'_>> {
    let mut cells: Vec<&str> = Vec::with_capacity(CELLS);
    let mut rest = raw;
    for _ in 0..CELLS {
        let trimmed = rest.trim_start();
        let Some(end) = trimmed.find(char::is_whitespace) else {
            bail!("line {no}: expected {CELLS} cells, then the title; use - for an empty cell");
        };
        cells.push(&trimmed[..end]);
        rest = &trimmed[end..];
    }

    let gap = rest.len() - rest.trim_start().len();
    let base = 2 + BLOCKED.len() - cells[CELLS - 1].len().min(BLOCKED.len());
    let title = rest.trim();
    let marked = title == "-" || title.starts_with("- ");
    let (level, title) = if marked {
        let Some(indent) = gap.checked_sub(base) else {
            bail!("line {no}, title: it starts inside the ~blocked column");
        };
        (indent + 1, title[1..].trim_start())
    } else {
        (0, title)
    };
    if title.is_empty() {
        bail!("line {no}, title: missing");
    }
    Ok(Split {
        cells,
        level,
        title,
    })
}

/// `a1`..`a10`, as the outline writes an attention.
fn parse_outline_attention(s: &str) -> Result<u8> {
    match s.strip_prefix('a') {
        Some(n) => parse_attention(n),
        None => bail!("invalid attention {s:?}: expected a1 to a10"),
    }
}

/// Parses one row: its cells split on whitespace, then the title.
fn parse_line(no: usize, raw: &str) -> Result<Line> {
    let Split {
        cells,
        level,
        title,
    } = split_row(no, raw)?;
    let slug = match cells[0] {
        "-" => None,
        s if s == slug::ROOT || slug::is_valid(s) => Some(s.to_string()),
        s => bail!("line {no}, slug: invalid slug {s:?}; use - for a new task"),
    };
    let deps = match cells[9] {
        "-" => Vec::new(),
        list => {
            let list: Vec<String> = list
                .split(',')
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect();
            if let Some(bad) = list.iter().find(|s| !slug::is_valid(s)) {
                bail!("line {no}, deps: invalid slug {bad:?}");
            }
            list
        }
    };
    Ok(Line {
        no,
        level,
        slug,
        status: cell(no, "status", cells[1], Status::parse)?,
        priority: cell(no, "priority", cells[3], parse_priority)?,
        size: cell(no, "size", cells[5], parse_size)?,
        attention: cell(no, "attention", cells[7], parse_outline_attention)?,
        deps,
        calculated: CALCULATED
            .iter()
            .map(|&(i, ..)| (i, cells[i].to_string()))
            .collect(),
        title: title.to_string(),
    })
}

/// One outline row, resolved against the project.
struct Node {
    line: Line,
    /// The existing task, or `None` for a new one.
    id: Option<i64>,
    /// Index of the parent node, or `None` at the top level.
    parent: Option<usize>,
    deps: BTreeSet<i64>,
}

/// Applies an edited outline to `project`, returning a description of each
/// change. With `dry_run`, checks the changes but doesn't make them.
pub fn apply(
    conn: &mut Connection,
    project: &Project,
    text: &str,
    dry_run: bool,
) -> Result<Vec<String>> {
    let tx = conn.transaction()?;
    let tasks = Tasks::load(&tx, project)?;
    let (_, version) = header(text)?;
    if version != render(&tasks).version {
        bail!(
            "{} changed since this outline was printed, and applying it would undo \
             those changes; print it again and redo your edits",
            project.name
        );
    }
    let (root, nodes) = resolve(&tasks, text)?;
    let mut out = Vec::new();
    let now = crate::now();
    let r = tasks.root();
    let label = format!("{} {}", r.slug, first_line(&r.intent));
    edit_fields(&tx, r, &root, root.status, now, &label, &mut out)?;
    let ids = apply_nodes(&tx, &tasks, &nodes, &mut out)?;
    describe_reorders(&tasks, &nodes, &ids, &mut out);
    cascade_cancel(&tx, &tasks, &nodes, &ids, &mut out)?;
    delete_absent(&tx, &tasks, &nodes, &ids, &mut out)?;
    Tasks::load(&tx, project)?.validate().context("refused")?;
    if !dry_run {
        tx.commit()?;
    }
    Ok(out)
}

/// The root's row, and a node for every other row.
fn resolve(tasks: &Tasks, text: &str) -> Result<(Line, Vec<Node>)> {
    let derived = Derived::compute(tasks);
    let mut root: Option<Line> = None;
    let mut nodes: Vec<Node> = Vec::new();
    let mut seen: HashMap<i64, usize> = HashMap::new();
    let mut stack: Vec<usize> = Vec::new();
    for (i, raw) in text.lines().enumerate().skip(1) {
        if raw.trim().is_empty() {
            continue;
        }
        let line = parse_line(i + 1, raw)?;
        let no = line.no;

        let id = match &line.slug {
            None => None,
            Some(s) => {
                let t = tasks.find(s).with_context(|| format!("line {no}, slug"))?;
                if t.is_deleted() {
                    bail!(
                        "line {no}, slug: {s} is deleted; restore it with `ergon restore {s}` first"
                    );
                }
                if let Some(other) = seen.insert(t.id, no) {
                    bail!("line {no}, slug: {s} is already on line {other}");
                }
                Some(t.id)
            }
        };
        if let Some(id) = id {
            let rendered = cells(tasks, &derived, id);
            for (i, written) in &line.calculated {
                if *written != rendered[*i] {
                    let (_, name, stored) = CALCULATED.iter().find(|c| c.0 == *i).unwrap();
                    bail!(
                        "line {no}, {name}: it's calculated, so {written} can't be edited; \
                         change the {stored} cell instead"
                    );
                }
            }
        }

        let is_root = id == Some(tasks.root().id);
        match (is_root, root.is_some(), line.level) {
            (true, _, 0) => {
                if !line.deps.is_empty() {
                    bail!("line {no}, deps: the root task can't have dependencies");
                }
                root = Some(line);
                continue;
            }
            (true, ..) => {
                bail!("line {no}, title: the root's title has no `- `, since nothing is above it")
            }
            (false, false, _) => bail!("line {no}: the first row must be the root's"),
            (false, true, 0) => {
                bail!("line {no}, title: it must start with `- `, since the task is under the root")
            }
            (false, true, _) => {}
        }

        let mut deps = BTreeSet::new();
        for d in &line.deps {
            let t = match tasks.find(d) {
                Ok(t) if !t.is_root() => t,
                _ => bail!("line {no}, deps: no task {d} to depend on"),
            };
            if t.is_deleted() {
                bail!("line {no}, deps: can't depend on {d}, which is deleted");
            }
            deps.insert(t.id);
        }

        while stack
            .last()
            .is_some_and(|&p| nodes[p].line.level >= line.level)
        {
            stack.pop();
        }
        stack.push(nodes.len());
        nodes.push(Node {
            parent: stack.iter().rev().nth(1).copied(),
            line,
            id,
            deps,
        });
    }
    let root = root.context("the outline has no rows; the first must be the root's")?;
    Ok((root, nodes))
}

fn status_label(s: Option<Status>) -> String {
    s.map_or("unset".into(), |s| s.to_string())
}

fn priority_label(p: Option<u8>) -> String {
    p.map_or("unset".into(), |p| format!("p{p}"))
}

fn size_label(n: Option<u32>) -> String {
    n.map_or("unset".into(), |n| n.to_string())
}

fn attention_label(a: Option<u8>) -> String {
    a.map_or("unset".into(), |a| format!("a{a}"))
}

/// Applies a row's title, status, priority, size and attention to an
/// existing task,
/// describing each change under `label`.
fn edit_fields(
    conn: &Connection,
    t: &crate::task::Task,
    line: &Line,
    status: Option<Status>,
    now: i64,
    label: &str,
    out: &mut Vec<String>,
) -> Result<()> {
    let mut intent = t.intent.clone();
    if line.title != first_line(&t.intent) {
        intent = replace_first_line(&t.intent, &line.title);
        out.push(format!("{label}: title -> {}", line.title));
    }
    if status != t.status {
        out.push(format!(
            "{label}: status {} -> {}",
            status_label(t.status),
            status_label(status)
        ));
    }
    if line.priority != t.priority {
        out.push(format!(
            "{label}: priority {} -> {}",
            priority_label(t.priority),
            priority_label(line.priority)
        ));
    }
    if line.size != t.size {
        out.push(format!(
            "{label}: size {} -> {}",
            size_label(t.size),
            size_label(line.size)
        ));
    }
    if line.attention != t.attention {
        out.push(format!(
            "{label}: attention {} -> {}",
            attention_label(t.attention),
            attention_label(line.attention)
        ));
    }
    if (&intent, status, line.priority, line.size, line.attention)
        != (&t.intent, t.status, t.priority, t.size, t.attention)
    {
        conn.execute(
            "UPDATE tasks SET intent = ?1, status = ?2, priority = ?3, size = ?4, attention = ?5,
             updated_at = ?6 WHERE id = ?7",
            params![
                intent,
                status,
                line.priority,
                line.size,
                line.attention,
                now,
                t.id
            ],
        )?;
    }
    Ok(())
}

/// Adds and edits every node in order, parents first, returning the task id
/// of each node.
fn apply_nodes(
    conn: &Connection,
    tasks: &Tasks,
    nodes: &[Node],
    out: &mut Vec<String>,
) -> Result<Vec<i64>> {
    let root = tasks.root().id;
    let now = crate::now();
    let mut rng = crate::rng(0);
    let mut ids: Vec<i64> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    let mut new_slugs: HashSet<String> = HashSet::new();
    // Each node's index among its siblings in the outline.
    let mut counts: HashMap<Option<usize>, i64> = HashMap::new();
    let positions: Vec<i64> = nodes
        .iter()
        .map(|n| {
            let c = counts.entry(n.parent).or_default();
            *c += 1;
            *c - 1
        })
        .collect();
    for (i, n) in nodes.iter().enumerate() {
        let line = &n.line;
        let parent = n.parent.map_or(root, |p| ids[p]);
        let place = match n.parent {
            Some(p) => format!("under {}", names[p]),
            None => "at the top level".to_string(),
        };
        // Top-level tasks must have a status, so an empty one there is todo.
        let status = line.status.or(n.parent.is_none().then_some(Status::Todo));

        // `name` refers to the task as a parent; `label` heads its changes.
        let (id, name, label) = match n.id {
            None => {
                let slug = (0..1000)
                    .map(|_| slug::random(&mut rng))
                    .find(|s| tasks.find(s).is_err() && !new_slugs.contains(s))
                    .context("couldn't find an unused slug")?;
                new_slugs.insert(slug.clone());
                conn.execute(
                    "INSERT INTO tasks (project_id, slug, parent_id, intent, status, priority, size, attention, created_at, updated_at, position)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, ?10)",
                    params![tasks.project.id, slug, parent, line.title, status, line.priority, line.size, line.attention, now, positions[i]],
                )?;
                let name = format!("\"{}\"", line.title);
                out.push(format!("add {name} {place}"));
                (conn.last_insert_rowid(), name.clone(), name)
            }
            Some(id) => {
                let t = tasks.get(id);
                let name = t.slug.clone();
                let label = format!("{} {}", t.slug, first_line(&t.intent));
                if t.parent_id != Some(parent) {
                    conn.execute(
                        "UPDATE tasks SET parent_id = ?1 WHERE id = ?2",
                        params![parent, id],
                    )?;
                    let to = match n.parent {
                        Some(p) => format!("under {}", names[p]),
                        None => "to the top level".to_string(),
                    };
                    out.push(format!("{label}: move {to}"));
                }
                if t.position != positions[i] {
                    conn.execute(
                        "UPDATE tasks SET position = ?1 WHERE id = ?2",
                        params![positions[i], id],
                    )?;
                }
                edit_fields(conn, t, line, status, now, &label, out)?;
                (id, name, label)
            }
        };

        let current: BTreeSet<i64> = match n.id {
            Some(id) => tasks.dependencies(id).iter().map(|d| d.id).collect(),
            None => BTreeSet::new(),
        };
        let mut changes = Vec::new();
        for &d in n.deps.difference(&current) {
            conn.execute(
                "INSERT OR IGNORE INTO task_deps (task_id, depends_on) VALUES (?1, ?2)",
                params![id, d],
            )?;
            changes.push(format!("+{}", tasks.get(d).slug));
        }
        for &d in current.difference(&n.deps) {
            conn.execute(
                "DELETE FROM task_deps WHERE task_id = ?1 AND depends_on = ?2",
                params![id, d],
            )?;
            changes.push(format!("-{}", tasks.get(d).slug));
        }
        if !changes.is_empty() {
            out.push(format!("{label}: deps {}", changes.join(" ")));
        }

        ids.push(id);
        names.push(name);
    }
    Ok(ids)
}

/// Notes each parent whose subtasks the outline put in a new order, among
/// the ones that stayed under it. Moved and new tasks are already described.
fn describe_reorders(tasks: &Tasks, nodes: &[Node], ids: &[i64], out: &mut Vec<String>) {
    let root = tasks.root().id;
    let mut groups: Vec<(Option<usize>, Vec<i64>)> = Vec::new();
    for n in nodes {
        let Some(id) = n.id else { continue };
        let parent = n.parent.map_or(root, |p| ids[p]);
        if tasks.get(id).parent_id != Some(parent) {
            continue;
        }
        match groups.iter_mut().find(|(p, _)| *p == n.parent) {
            Some((_, kept)) => kept.push(id),
            None => groups.push((n.parent, vec![id])),
        }
    }
    for (p, kept) in groups {
        let parent = p.map_or(root, |p| ids[p]);
        let before: Vec<i64> = tasks
            .children(parent)
            .map(|t| t.id)
            .filter(|id| kept.contains(id))
            .collect();
        if before != kept {
            out.push(match p {
                Some(_) => {
                    let t = tasks.get(parent);
                    format!("{} {}: reorder subtasks", t.slug, first_line(&t.intent))
                }
                None => "reorder top-level tasks".to_string(),
            });
        }
    }
}

/// Like `ergon edit`, cancelling a task cancels its subtasks that have an
/// unfinished status of their own, unless the outline changed that status.
fn cascade_cancel(
    conn: &Connection,
    tasks: &Tasks,
    nodes: &[Node],
    ids: &[i64],
    out: &mut Vec<String>,
) -> Result<()> {
    let cancelled = |i: usize| {
        let n = &nodes[i];
        n.line.status == Some(Status::Cancelled)
            && n.id
                .is_some_and(|id| tasks.get(id).status != Some(Status::Cancelled))
    };
    for (i, n) in nodes.iter().enumerate() {
        let Some(id) = n.id else { continue };
        let t = tasks.get(id);
        let unchanged_open = n.line.status == t.status
            && matches!(
                t.status,
                Some(Status::Todo | Status::InProgress | Status::Landing)
            );
        if !unchanged_open {
            continue;
        }
        let mut cur = n.parent;
        while let Some(p) = cur {
            if cancelled(p) {
                conn.execute(
                    "UPDATE tasks SET status = 'cancelled', updated_at = ?1 WHERE id = ?2",
                    params![crate::now(), ids[i]],
                )?;
                out.push(format!(
                    "{} {}: cancelled along with {}",
                    t.slug,
                    first_line(&t.intent),
                    tasks.get(ids[p]).slug
                ));
                break;
            }
            cur = nodes[p].parent;
        }
    }
    Ok(())
}

/// Deletes the live tasks missing from the outline. A deleted task's
/// subtasks are deleted with it, unless the outline moved them elsewhere.
fn delete_absent(
    conn: &Connection,
    tasks: &Tasks,
    nodes: &[Node],
    ids: &[i64],
    out: &mut Vec<String>,
) -> Result<()> {
    let present: HashSet<i64> = ids.iter().copied().collect();
    let absent: Vec<i64> = tasks
        .descendants(tasks.root().id)
        .into_iter()
        .filter(|id| !present.contains(id))
        .collect();
    let absent_set: HashSet<i64> = absent.iter().copied().collect();

    // Group each deleted task under the highest deleted ancestor it has
    // through deleted tasks only.
    let mut groups: Vec<(i64, Vec<i64>)> = Vec::new();
    for &a in &absent {
        let mut top = a;
        while let Some(p) = tasks.get(top).parent_id.filter(|p| absent_set.contains(p)) {
            top = p;
        }
        match groups.iter_mut().find(|(t, _)| *t == top) {
            Some((_, members)) => members.push(a),
            None => groups.push((top, vec![a])),
        }
    }

    let now = crate::now();
    for (top, members) in groups {
        for &m in &members {
            conn.execute(
                "UPDATE tasks SET deleted_at = ?1 WHERE id = ?2",
                params![now, m],
            )?;
        }
        let dependents = nodes
            .iter()
            .filter(|n| n.deps.iter().any(|d| members.contains(d)))
            .count();
        let t = tasks.get(top);
        let mut with = Vec::new();
        match members.len() - 1 {
            0 => {}
            1 => with.push("its 1 subtask".to_string()),
            n => with.push(format!("its {n} subtasks")),
        }
        match dependents {
            0 => {}
            1 => with.push("1 dependent".to_string()),
            n => with.push(format!("{n} dependents")),
        }
        let with = if with.is_empty() {
            String::new()
        } else {
            format!(", with {}", with.join(" and "))
        };
        out.push(format!(
            "{} {}: delete{with}",
            t.slug,
            first_line(&t.intent)
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Line {
        parse_line(2, raw).unwrap()
    }

    /// Cells split by position, so an empty cell is `-` and a title may
    /// hold anything. Nesting counts from the end of the fixed-width
    /// ~blocked column, so a widened cell to its left doesn't re-nest.
    #[test]
    fn parses_rows() {
        let l = parse(
            "FTFE  in_progress  ~in_progress  p1  ~p1  5  ~5  a7  ~a9  FTFE,nHcx  -           - 3 bugs ~p1 - left",
        );
        assert_eq!(l.slug.as_deref(), Some("FTFE"));
        assert_eq!(l.status, Some(Status::InProgress));
        assert_eq!(
            (l.priority, l.size, l.attention),
            (Some(1), Some(5), Some(7))
        );
        assert_eq!(l.deps, ["FTFE", "nHcx"]);
        assert_eq!(l.title, "3 bugs ~p1 - left");
        assert_eq!(l.level, 3);

        let widened = parse(
            "FTFE  in_progress  ~in_progress  p1  ~p1  5  ~5  a7  ~a9  FTFE,nHcx,pQyk  -           - Title",
        );
        assert_eq!(widened.level, 3);
        let blocked = parse("FTFE  -  ~todo  -  -  -  -  -  -  -  ~blocked    - Title");
        assert_eq!(blocked.level, 3);
        assert_eq!(blocked.status, None);

        let root = parse("root  in_progress  ~in_progress  -  -  -  -  -  -  -  -         Root");
        assert_eq!((root.level, root.title.as_str()), (0, "Root"));
        let new = parse("-  todo  -  -  -  -  -  -  -  -  -         - Write tests");
        assert_eq!((new.slug, new.level), (None, 1));

        for bad in [
            "FTFE  todo  ~todo  -  -  -  -  -  -  -  -  - Title",
            "FTFE  todo  ~todo  -  -  4  -  -  -  -  -         - Title",
            "FTFE  todo  ~todo  p9  -  -  -  -  -  -  -         - Title",
            "FTFE  open  ~todo  -  -  -  -  -  -  -  -         - Title",
            "FTFE  todo  ~todo  -  -  -  -  -  -  oops!  -         - Title",
            "FTFE  todo  ~todo  -  -  -  -  -  -  -  -         - ",
            "FTFE  todo  ~todo  -  -  -  -  7  -  -  -         - Title",
            "FTFE  todo  ~todo  -  -  -  -  a11  -  -  -         - Title",
        ] {
            assert!(parse_line(3, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn replaces_only_the_first_line() {
        assert_eq!(replace_first_line("Old\n\nBody", "New"), "New\n\nBody");
        assert_eq!(replace_first_line("\n  Old", "New"), "New");
        assert_eq!(first_line("\n  Old title  \nBody"), "Old title");
    }
}
