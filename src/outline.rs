//! The outline: a project's tasks as editable text, one line per task, and
//! applying an edited outline back to the project in one transaction.
//!
//! ```text
//! # ergon demo 1a2b3c4d
//! qxpj [ ] Backend !p1
//!   FTFE [~] Write the API deps:nHcx
//!     pQyk Document it
//! nHcx [x] Frontend
//! ```
//!
//! A line is its indentation (it nests under the nearest less indented line
//! above), the slug (absent on a new task), the status box (absent when the
//! status follows the parent), the first line of the intent, then optional
//! `!pN` and `deps:<slug>,<slug>` tokens. The header names the project and a
//! version: a hash of the rest of the text as rendered. An outline edited
//! while the project changed underneath it is refused, since applying it
//! would undo those changes.

use std::collections::{BTreeSet, HashMap, HashSet};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::derived::Derived;
use crate::project::Project;
use crate::slug;
use crate::task::{Status, Tasks, parse_priority};

const INDENT: &str = "  ";

const BOXES: [(char, Status); 5] = [
    (' ', Status::Todo),
    ('~', Status::InProgress),
    ('>', Status::Landing),
    ('x', Status::Done),
    ('-', Status::Cancelled),
];

fn box_char(status: Status) -> char {
    BOXES.iter().find(|(_, s)| *s == status).unwrap().0
}

/// The intent's first line, uncapped: the part of the intent an outline line
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

pub struct Rendered {
    pub text: String,
    pub version: String,
    /// The task on each line after the header, in order.
    pub ids: Vec<i64>,
}

pub fn render(tasks: &Tasks) -> Rendered {
    let mut body = String::new();
    let mut ids = Vec::new();
    for c in tasks.children(tasks.root().id) {
        write_line(tasks, c.id, 0, &mut body, &mut ids);
    }
    let version = version(&body);
    Rendered {
        text: format!("# ergon {} {version}\n{body}", tasks.project.name),
        version,
        ids,
    }
}

fn write_line(tasks: &Tasks, id: i64, depth: usize, out: &mut String, ids: &mut Vec<i64>) {
    let t = tasks.get(id);
    out.push_str(&INDENT.repeat(depth));
    out.push_str(&t.slug);
    if let Some(s) = t.status {
        out.push_str(&format!(" [{}]", box_char(s)));
    }
    out.push(' ');
    out.push_str(first_line(&t.intent));
    if let Some(p) = t.priority {
        out.push_str(&format!(" !p{p}"));
    }
    let deps: Vec<&str> = tasks
        .dependencies(id)
        .iter()
        .map(|d| d.slug.as_str())
        .collect();
    if !deps.is_empty() {
        out.push_str(&format!(" deps:{}", deps.join(",")));
    }
    out.push('\n');
    ids.push(id);
    for c in tasks.children(id) {
        write_line(tasks, c.id, depth + 1, out, ids);
    }
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
    /// Effective priority: stored, inferred or boosted.
    priority: Option<u8>,
    stored_priority: Option<u8>,
    blocked: bool,
}

/// The outline as JSON, with what each line's task looks like after
/// inference, for editors to annotate it with.
pub fn json(tasks: &Tasks, derived: &Derived) -> String {
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
                priority: derived.priority(id),
                stored_priority: tasks.get(id).priority,
                blocked: derived.is_blocked(id),
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
    indent: usize,
    slug: Option<String>,
    status: Option<Status>,
    title: String,
    priority: Option<u8>,
    deps: Vec<String>,
}

/// Parses one task line. `is_slug` says whether a leading word names a task;
/// any other leading word is part of a new task's title.
fn parse_line(no: usize, raw: &str, is_slug: impl Fn(&str) -> bool) -> Result<Line> {
    let mut indent = 0;
    for c in raw.chars() {
        match c {
            ' ' => indent += 1,
            '\t' => indent += 8 - indent % 8,
            _ => break,
        }
    }
    let mut rest = raw.trim();

    let mut slug = None;
    if let Some(word) = rest.split_whitespace().next().filter(|w| is_slug(w)) {
        slug = Some(word.to_string());
        rest = rest[word.len()..].trim_start();
    }

    let mut status = None;
    let mut chars = rest.chars();
    if let (Some('['), Some(c), Some(']')) = (chars.next(), chars.next(), chars.next()) {
        let c = c.to_ascii_lowercase();
        if let Some((_, s)) = BOXES.iter().find(|(b, _)| *b == c) {
            status = Some(*s);
            rest = rest[1 + c.len_utf8() + 1..].trim_start();
        }
    } else if let Some(r) = rest.strip_prefix("[]") {
        status = Some(Status::Todo);
        rest = r.trim_start();
    }

    let mut priority = None;
    let mut deps = None;
    while let Some((head, word)) = rest.rsplit_once(char::is_whitespace) {
        if let (None, Some(p)) = (priority, word.strip_prefix('!')) {
            match parse_priority(p) {
                Ok(p) => priority = Some(p),
                Err(_) => break,
            }
        } else if let (None, Some(list)) = (&deps, word.strip_prefix("deps:")) {
            let list: Vec<String> = list
                .split(',')
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect();
            if let Some(bad) = list.iter().find(|s| !slug::is_valid(s)) {
                bail!("line {no}: invalid slug {bad:?} in deps");
            }
            deps = Some(list);
        } else {
            break;
        }
        rest = head.trim_end();
    }

    if rest.is_empty() {
        bail!("line {no}: missing a title");
    }
    Ok(Line {
        no,
        indent,
        slug,
        status,
        title: rest.to_string(),
        priority,
        deps: deps.unwrap_or_default(),
    })
}

/// One outline line, resolved against the project.
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
    let nodes = resolve(&tasks, text)?;
    let mut out = Vec::new();
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

fn resolve(tasks: &Tasks, text: &str) -> Result<Vec<Node>> {
    let is_slug = |w: &str| w != slug::ROOT && slug::is_valid(w) && tasks.find(w).is_ok();
    let mut nodes: Vec<Node> = Vec::new();
    let mut seen: HashMap<i64, usize> = HashMap::new();
    let mut stack: Vec<usize> = Vec::new();
    for (i, raw) in text.lines().enumerate().skip(1) {
        if raw.trim().is_empty() {
            continue;
        }
        let line = parse_line(i + 1, raw, is_slug)?;
        let no = line.no;

        let id = match &line.slug {
            None => None,
            Some(s) => {
                let t = tasks.find(s)?;
                if t.is_deleted() {
                    bail!("line {no}: {s} is deleted; restore it with `ergon restore {s}` first");
                }
                if let Some(other) = seen.insert(t.id, no) {
                    bail!("line {no}: {s} is already on line {other}");
                }
                Some(t.id)
            }
        };

        let mut deps = BTreeSet::new();
        for d in &line.deps {
            let t = match tasks.find(d) {
                Ok(t) if !t.is_root() => t,
                _ => bail!("line {no}: no task {d} to depend on"),
            };
            if t.is_deleted() {
                bail!("line {no}: can't depend on {d}, which is deleted");
            }
            deps.insert(t.id);
        }

        while stack
            .last()
            .is_some_and(|&p| nodes[p].line.indent >= line.indent)
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
    Ok(nodes)
}

fn status_label(s: Option<Status>) -> String {
    s.map_or("unset".into(), |s| s.to_string())
}

fn priority_label(p: Option<u8>) -> String {
    p.map_or("unset".into(), |p| format!("p{p}"))
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
        // Top-level tasks must have a status, so a missing box there is todo.
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
                    "INSERT INTO tasks (project_id, slug, parent_id, intent, status, priority, created_at, updated_at, position)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8)",
                    params![tasks.project.id, slug, parent, line.title, status, line.priority, now, positions[i]],
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
                if (&intent, status, line.priority) != (&t.intent, t.status, t.priority) {
                    conn.execute(
                        "UPDATE tasks SET intent = ?1, status = ?2, priority = ?3, updated_at = ?4
                         WHERE id = ?5",
                        params![intent, status, line.priority, now, id],
                    )?;
                }
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
        parse_line(2, raw, |w| w == "qxpj").unwrap()
    }

    #[test]
    fn parses_lines() {
        let l = parse("    qxpj [~] Write the API !p1 deps:FTFE,nHcx");
        assert_eq!(l.indent, 4);
        assert_eq!(l.slug.as_deref(), Some("qxpj"));
        assert_eq!(l.status, Some(Status::InProgress));
        assert_eq!(l.title, "Write the API");
        assert_eq!(l.priority, Some(1));
        assert_eq!(l.deps, ["FTFE", "nHcx"]);

        // A new task: no slug, a box, tokens in either order.
        let l = parse("\t[] Make tests pass deps: !P0");
        assert_eq!((l.indent, l.slug, l.status), (8, None, Some(Status::Todo)));
        assert_eq!((l.title.as_str(), l.priority), ("Make tests pass", Some(0)));
        assert!(l.deps.is_empty());

        // Things that only look like a box or tokens stay in the title.
        let l = parse("qxpj [WIP] use !p9");
        assert_eq!(l.status, None);
        assert_eq!(l.title, "[WIP] use !p9");

        assert!(parse_line(3, "qxpj [x]", |w| w == "qxpj").is_err());
        assert!(parse_line(3, "Do it deps:oops", |_| false).is_err());
    }

    #[test]
    fn replaces_only_the_first_line() {
        assert_eq!(replace_first_line("Old\n\nBody", "New"), "New\n\nBody");
        assert_eq!(replace_first_line("\n  Old", "New"), "New");
        assert_eq!(first_line("\n  Old title  \nBody"), "Old title");
    }
}
