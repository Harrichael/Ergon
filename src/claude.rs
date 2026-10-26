//! Launching Claude Code sessions attached to tasks.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::project::Project;
use crate::task::{Status, Tasks};

/// The planner's instructions, also printed by `ergon help planner`.
pub const PLANNER_DOC: &str = include_str!("../docs/prompts/planner.md");

/// A `claude` invocation, ready to run.
#[derive(Debug)]
pub struct Launch {
    pub program: PathBuf,
    pub cwd: PathBuf,
    pub args: Vec<String>,
    /// Set when the task is blocked; worth telling the user.
    pub warning: Option<String>,
}

impl Launch {
    /// Replaces this process with `claude` (on Unix), so the session owns the
    /// terminal. Only returns on failure.
    pub fn exec(self) -> Result<()> {
        let mut cmd = std::process::Command::new(&self.program);
        cmd.args(&self.args).current_dir(&self.cwd);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let err = cmd.exec();
            Err(err).with_context(|| format!("launching {}", self.program.display()))
        }
        #[cfg(not(unix))]
        {
            let status = cmd
                .status()
                .with_context(|| format!("launching {}", self.program.display()))?;
            std::process::exit(status.code().unwrap_or(1));
        }
    }
}

/// `$ERGON_CLAUDE`, else `claude`, resolved through `PATH`.
fn find_claude() -> Result<PathBuf> {
    let name = std::env::var_os("ERGON_CLAUDE")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| OsString::from("claude"));
    let path = PathBuf::from(&name);
    if path.components().count() > 1 {
        return Ok(path);
    }
    let dirs = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&dirs)
        .map(|d| d.join(&path))
        .find(|p| p.is_file())
        .with_context(|| format!("{} not found on PATH", path.display()))
}

/// Prepares `ergon work`: the first time, records a new session with `cwd`
/// and moves a `todo` task to `in_progress`; afterwards, resumes the session
/// in its recorded directory.
pub fn work(conn: &mut Connection, project: &Project, slug: &str, cwd: &Path) -> Result<Launch> {
    let program = find_claude()?;
    let tx = conn.transaction()?;
    let tasks = Tasks::load(&tx, project)?;
    let task = tasks.live(slug)?;
    let reference = tasks.reference(task.id);
    let status = tasks.effective_status(task.id);
    if status.is_closed() {
        bail!("{reference} is {status}");
    }
    let blocked_by = crate::derived::blocked_by(&tasks, task.id);
    let warning = (!blocked_by.is_empty()).then(|| {
        let refs: Vec<String> = blocked_by.iter().map(|&d| tasks.reference(d)).collect();
        format!("{reference} is blocked, waiting on {}", refs.join(", "))
    });

    if let (Some(session), Some(dir)) = (&task.claude_session, &task.claude_cwd) {
        let dir = PathBuf::from(dir);
        if !dir.is_dir() {
            bail!(
                "{reference}'s session directory {} no longer exists",
                dir.display()
            );
        }
        return Ok(Launch {
            program,
            cwd: dir,
            args: vec!["--resume".into(), session.clone()],
            warning,
        });
    }

    let session = crate::uuid(&mut crate::rng(task.id as u64));
    let dir = cwd
        .to_str()
        .with_context(|| format!("{} isn't valid UTF-8", cwd.display()))?;
    let new_status = if status == Status::Todo {
        Some(Status::InProgress)
    } else {
        task.status
    };
    tx.execute(
        "UPDATE tasks SET claude_session = ?1, claude_cwd = ?2, status = ?3, updated_at = ?4
         WHERE id = ?5",
        params![session, dir, new_status, crate::now(), task.id],
    )?;
    Tasks::load(&tx, project)?.validate().context("refused")?;
    tx.commit()?;

    let prompt = if task.is_root() {
        planner_prompt(&tasks)
    } else {
        work_prompt(&tasks, task.id)
    };
    Ok(Launch {
        program,
        cwd: cwd.to_path_buf(),
        args: vec!["--session-id".into(), session, prompt],
        warning,
    })
}

pub fn planner_prompt(tasks: &Tasks) -> String {
    let root = tasks.root();
    format!(
        "{PLANNER_DOC}\n\n## This project\n\nProject: {}\n\nRoot intent:\n\n{}\n\n\
         Ask the user how they'd like to start planning.\n",
        tasks.project.name, root.intent
    )
}

/// The work session's prompt; its wording and layout live in the template.
const WORK_TEMPLATE: &str = include_str!("../docs/prompts/work.md.tera");

/// An intent as the template reads it: its first line, and the rest.
#[derive(Serialize)]
struct Intent<'a> {
    title: &'a str,
    body: &'a str,
}

impl<'a> Intent<'a> {
    /// The whole first line, not the title capped for display: a prompt must
    /// not lose the end of a long one.
    fn of(intent: &'a str) -> Self {
        let intent = intent.trim();
        let (title, rest) = intent.split_once('\n').unwrap_or((intent, ""));
        // Skip blank lines, but keep the first body line's own indentation.
        let start = rest
            .find(|c: char| !c.is_whitespace())
            .map_or(rest.len(), |i| rest[..i].rfind('\n').map_or(0, |n| n + 1));
        Intent {
            title: title.trim_end(),
            body: &rest[start..],
        }
    }
}

#[derive(Serialize)]
struct Outlined<'a> {
    #[serde(flatten)]
    intent: Intent<'a>,
    /// Two spaces per level of its outline.
    pad: String,
    marker: Option<Status>,
}

pub fn work_prompt(tasks: &Tasks, id: i64) -> String {
    let ancestors: Vec<Outlined> = tasks
        .ancestors(id)
        .into_iter()
        .rev()
        .enumerate()
        .map(|(depth, a)| Outlined {
            intent: Intent::of(&tasks.get(a).intent),
            pad: "  ".repeat(depth),
            marker: None,
        })
        .collect();
    let mut subtasks = Vec::new();
    collect_subtasks(tasks, id, 0, &mut subtasks);

    let mut ctx = tera::Context::new();
    ctx.insert("ancestors", &ancestors);
    ctx.insert("reference", &tasks.reference(id));
    ctx.insert("task", &Intent::of(&tasks.get(id).intent));
    ctx.insert("subtasks", &subtasks);
    let mut tera = tera::Tera::default();
    tera.register_filter("indent", indent);
    tera.add_raw_template("work.md", WORK_TEMPLATE)
        .and_then(|_| tera.render("work.md", &ctx))
        .expect("the work template is built in and its context complete")
}

fn collect_subtasks<'a>(tasks: &'a Tasks, id: i64, depth: usize, out: &mut Vec<Outlined<'a>>) {
    for c in tasks.children(id) {
        let status = tasks.effective_status(c.id);
        if status == Status::Cancelled {
            continue;
        }
        out.push(Outlined {
            intent: Intent::of(&c.intent),
            pad: "  ".repeat(depth),
            marker: matches!(status, Status::Done | Status::Landing).then_some(status),
        });
        collect_subtasks(tasks, c.id, depth + 1, out);
    }
}

/// `text | indent(with=PREFIX)`: PREFIX before every line but blank ones.
fn indent(value: &tera::Value, args: &HashMap<String, tera::Value>) -> tera::Result<tera::Value> {
    let text = value.as_str().ok_or("indent takes text")?;
    let prefix = args.get("with").and_then(|p| p.as_str()).unwrap_or("  ");
    let lines: Vec<String> = text
        .lines()
        .map(|l| {
            if l.trim().is_empty() {
                String::new()
            } else {
                format!("{prefix}{l}")
            }
        })
        .collect();
    Ok(lines.join("\n").into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::{self, Changes};
    use crate::{db, project};

    #[test]
    fn work_prompt_layout() {
        let mut conn = db::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let p = project::init(
            &mut conn,
            "demo",
            "Build a demo\n\nProject context.",
            dir.path(),
        )
        .unwrap();
        let add = |conn: &mut Connection, parent: Option<&str>, intent: &str| {
            ops::add(conn, &p, parent, intent, None, None, None).unwrap()
        };
        let a = add(&mut conn, None, "Backend");
        let b = add(&mut conn, Some(&a), "Write the API\nUse REST.");
        let c = add(&mut conn, Some(&b), "Routes\n\nUnder /api.\n\nVersioned.");
        let d = add(&mut conn, Some(&b), "Auth");
        let e = add(&mut conn, Some(&b), "Dropped");
        for (slug, status) in [(&d, Status::Done), (&e, Status::Cancelled)] {
            let changes = Changes {
                status: Some(Some(status)),
                ..Default::default()
            };
            ops::edit(&mut conn, &p, slug, changes).unwrap();
        }
        let n = add(&mut conn, Some(&c), "Nested\nOne level down.");

        let tasks = Tasks::load(&conn, &p).unwrap();
        let prompt = work_prompt(&tasks, tasks.live(&b).unwrap().id);
        assert_eq!(
            prompt,
            format!(
                "Greater context that your work sits within:

- Build a demo
  - Backend

Task to complete here (demo/{b}):

Write the API

Use REST.

- Routes
  Under /api.

  Versioned.
  - Nested
    One level down.
- [done] Auth

Ask the user how they'd like to start.
"
            )
        );

        let leaf = work_prompt(&tasks, tasks.live(&n).unwrap().id);
        assert!(leaf.ends_with(&format!(
            "  - Backend
    - Write the API
      - Routes

Task to complete here (demo/{n}):

Nested

One level down.

Ask the user how they'd like to start.
"
        )));
    }
}
