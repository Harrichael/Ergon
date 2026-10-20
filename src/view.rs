//! Plain-text renderings of tasks.

use std::fmt::Write;

use crate::fmt::timestamp;
use crate::task::Tasks;

fn priority(p: Option<u8>) -> String {
    p.map_or("p-".to_string(), |p| format!("p{p}"))
}

/// One task in full.
pub fn show(tasks: &Tasks, id: i64, now: i64) -> String {
    let t = tasks.get(id);
    let status = tasks.effective_status(id);
    let mut out = String::new();
    let _ = writeln!(out, "{}  {}", tasks.reference(id), t.title());
    let inherited = if t.status.is_none() {
        " (inherited)"
    } else {
        ""
    };
    let _ = writeln!(out, "status:    {status}{inherited}");
    let _ = writeln!(out, "priority:  {}", priority(t.priority));
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
    let _ = writeln!(out, "\n{}", t.intent);

    let children: Vec<_> = tasks.children(id).collect();
    if !children.is_empty() {
        let _ = writeln!(out, "\nSubtasks:");
        for c in children {
            let _ = writeln!(
                out,
                "  - {}  {:<11}  {}",
                c.slug,
                tasks.effective_status(c.id).as_str(),
                c.title()
            );
        }
    }
    out
}

/// The subtree under `id` as indented titles.
pub fn tree(tasks: &Tasks, id: i64) -> String {
    let mut out = String::new();
    write_tree(tasks, id, 0, &mut out);
    out
}

fn write_tree(tasks: &Tasks, id: i64, depth: usize, out: &mut String) {
    let t = tasks.get(id);
    let _ = writeln!(
        out,
        "{}{}  {:<11}  {}  {}",
        "  ".repeat(depth),
        t.slug,
        tasks.effective_status(id).as_str(),
        priority(t.priority),
        t.title()
    );
    for c in tasks.children(id) {
        write_tree(tasks, c.id, depth + 1, out);
    }
}
