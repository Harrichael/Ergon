//! Launching Claude Code sessions attached to tasks.

use std::ffi::OsString;
use std::fmt::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};

use crate::project::Project;
use crate::task::{Status, Tasks};

/// The planner's instructions, also printed by `ergon help planner`.
pub const PLANNER_DOC: &str = include_str!("../docs/planner.md");

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

pub fn work_prompt(tasks: &Tasks, id: i64) -> String {
    let mut out = String::from("Greater context that your work sits within:\n\n");
    for a in tasks.ancestors(id).into_iter().rev() {
        bullet(&mut out, &tasks.get(a).intent, 0, None);
        out.push('\n');
    }
    let _ = writeln!(out, "Task to complete here ({}):\n", tasks.reference(id));
    let _ = writeln!(out, "{}\n", tasks.get(id).intent);
    let before = out.len();
    write_subtasks(tasks, id, 0, &mut out);
    if out.len() > before {
        out.push('\n');
    }
    out.push_str("Ask the user how they'd like to start.\n");
    out
}

fn write_subtasks(tasks: &Tasks, id: i64, depth: usize, out: &mut String) {
    for c in tasks.children(id) {
        let status = tasks.effective_status(c.id);
        if status == Status::Cancelled {
            continue;
        }
        let marker = matches!(status, Status::Done | Status::Landing).then_some(status);
        bullet(out, &c.intent, depth, marker);
        write_subtasks(tasks, c.id, depth + 1, out);
    }
}

/// Writes `intent` as a bullet at `depth`, with continuation lines aligned.
fn bullet(out: &mut String, intent: &str, depth: usize, marker: Option<Status>) {
    let indent = "  ".repeat(depth);
    let marker = marker.map(|s| format!("[{s}] ")).unwrap_or_default();
    for (i, line) in intent.lines().enumerate() {
        match (i, line.is_empty()) {
            (0, _) => {
                let _ = writeln!(out, "{indent}- {marker}{line}");
            }
            (_, true) => out.push('\n'),
            (_, false) => {
                let _ = writeln!(out, "{indent}  {line}");
            }
        }
    }
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
            ops::add(conn, &p, parent, intent, None, None).unwrap()
        };
        let a = add(&mut conn, None, "Backend");
        let b = add(&mut conn, Some(&a), "Write the API\nUse REST.");
        let c = add(&mut conn, Some(&b), "Routes");
        let d = add(&mut conn, Some(&b), "Auth");
        let e = add(&mut conn, Some(&b), "Dropped");
        for (slug, status) in [(&d, Status::Done), (&e, Status::Cancelled)] {
            let changes = Changes {
                status: Some(Some(status)),
                ..Default::default()
            };
            ops::edit(&mut conn, &p, slug, changes).unwrap();
        }
        add(&mut conn, Some(&c), "Nested");

        let tasks = Tasks::load(&conn, &p).unwrap();
        let prompt = work_prompt(&tasks, tasks.live(&b).unwrap().id);
        assert_eq!(
            prompt,
            format!(
                "Greater context that your work sits within:

- Build a demo

  Project context.

- Backend

Task to complete here (demo/{b}):

Write the API
Use REST.

- Routes
  - Nested
- [done] Auth

Ask the user how they'd like to start.
"
            )
        );
    }
}
