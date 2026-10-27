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
    /// terminal; `stdin` stands in for ours when that was a pipe. Only
    /// returns on failure.
    pub fn exec(self, stdin: Option<std::fs::File>) -> Result<()> {
        let mut cmd = std::process::Command::new(&self.program);
        cmd.args(&self.args).current_dir(&self.cwd);
        if let Some(f) = stdin {
            cmd.stdin(f);
        }
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
    on_path(&path).with_context(|| format!("{} not found on PATH", path.display()))
}

fn on_path(program: &Path) -> Option<PathBuf> {
    let dirs = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&dirs)
        .map(|d| d.join(program))
        .find(|p| p.is_file())
}

/// The tasks one session works on, in tree order: one task, or several
/// taken on together. Refuses closed tasks, the root among others (planning
/// isn't work), and a task alongside its own subtask (it includes it).
pub fn selection(tasks: &Tasks, slugs: &[String]) -> Result<Vec<i64>> {
    let mut ids = Vec::new();
    for slug in slugs {
        let id = tasks.live(slug)?.id;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    for &id in &ids {
        let status = tasks.effective_status(id);
        if status.is_closed() {
            bail!("{} is {status}", tasks.reference(id));
        }
        if ids.len() > 1 && tasks.get(id).is_root() {
            bail!("the root's planner session can't take other tasks too");
        }
        if let Some(&a) = ids.iter().find(|&&a| tasks.is_ancestor(a, id)) {
            bail!(
                "{} is a subtask of {}, which already includes it",
                tasks.reference(id),
                tasks.reference(a)
            );
        }
    }
    let root = tasks.root().id;
    let order: Vec<i64> = std::iter::once(root)
        .chain(tasks.descendants(root))
        .collect();
    ids.sort_by_key(|id| order.iter().position(|o| o == id));
    Ok(ids)
}

/// Prepares `ergon work` for one task or several taken on together. The
/// first time, records one new session with `cwd` on all of them and moves
/// `todo` ones to `in_progress`; afterwards, resumes that session in its
/// recorded directory. Tasks that have different sessions, or where only
/// some have one, are refused: a session's conversation is already under
/// way, and taking on more work would be a change of plan for the user to
/// make.
pub fn work(
    conn: &mut Connection,
    project: &Project,
    slugs: &[String],
    cwd: &Path,
) -> Result<Launch> {
    let program = find_claude()?;
    let tx = conn.transaction()?;
    let tasks = Tasks::load(&tx, project)?;
    let ids = selection(&tasks, slugs)?;
    let warnings: Vec<String> = ids
        .iter()
        .filter_map(|&id| {
            let blocked_by = crate::derived::blocked_by(&tasks, id);
            (!blocked_by.is_empty()).then(|| {
                let refs: Vec<String> = blocked_by.iter().map(|&d| tasks.reference(d)).collect();
                format!(
                    "{} is blocked, waiting on {}",
                    tasks.reference(id),
                    refs.join(", ")
                )
            })
        })
        .collect();
    let warning = (!warnings.is_empty()).then(|| warnings.join("\nwarning: "));

    let sessions: Vec<(&str, &str)> = ids
        .iter()
        .filter_map(|&id| {
            let t = tasks.get(id);
            Some((t.claude_session.as_deref()?, t.claude_cwd.as_deref()?))
        })
        .collect();
    if let Some(&(session, dir)) = sessions.first() {
        if sessions.len() < ids.len() || sessions.iter().any(|s| s.0 != session) {
            let list: Vec<String> = ids
                .iter()
                .map(|&id| {
                    let t = tasks.get(id);
                    let s = t.claude_session.as_deref().unwrap_or("no session");
                    format!("{} ({s})", tasks.reference(id))
                })
                .collect();
            bail!(
                "these tasks don't share one session, so they can't be worked on together: {}",
                list.join(", ")
            );
        }
        let reference = tasks.reference(ids[0]);
        let dir = PathBuf::from(dir);
        if !dir.is_dir() {
            bail!(
                "{reference}'s session directory {} no longer exists",
                dir.display()
            );
        }
        if conversation_saved(session) {
            return Ok(Launch {
                program,
                cwd: dir,
                args: vec!["--resume".into(), session.to_string()],
                warning,
            });
        }
        // claude was launched but never saved the conversation (it crashed
        // or quit at once): there is nothing to resume, so start it again.
        let fresh = format!("session {session} was never saved; starting it again");
        let warning = Some(warning.map_or(fresh.clone(), |w| format!("{w}\nwarning: {fresh}")));
        return Ok(Launch {
            program,
            cwd: dir,
            args: vec![
                "--session-id".into(),
                session.to_string(),
                prompt(&tasks, &ids),
            ],
            warning,
        });
    }

    let session = crate::uuid(&mut crate::rng(ids[0] as u64));
    record(&tx, &tasks, &ids, &session, cwd)?;
    Tasks::load(&tx, project)?.validate().context("refused")?;
    tx.commit()?;

    let prompt = prompt(&tasks, &ids);
    Ok(Launch {
        program,
        cwd: cwd.to_path_buf(),
        args: vec!["--session-id".into(), session, prompt],
        warning,
    })
}

/// Records an already running session on tasks, as `ergon work` does when
/// it starts one, so that `ergon work` resumes it from then on. Returns
/// the tasks' references. A task can hold only one session, so one with
/// another session is refused.
pub fn attach(
    conn: &mut Connection,
    project: &Project,
    slugs: &[String],
    session: &str,
    cwd: &Path,
) -> Result<Vec<String>> {
    let tx = conn.transaction()?;
    let tasks = Tasks::load(&tx, project)?;
    let ids = selection(&tasks, slugs)?;
    for &id in &ids {
        if let Some(other) = tasks.get(id).claude_session.as_deref().filter(|s| *s != session) {
            let r = tasks.reference(id);
            bail!("{r} already has session {other}; `ergon work {r}` resumes it");
        }
    }
    // Claude Code resumes a conversation only from the directory it began
    // in, which may not be where the session's shell is now.
    let dir = conversation_file(session)
        .and_then(|f| began_in(&f))
        .unwrap_or_else(|| cwd.to_path_buf());
    record(&tx, &tasks, &ids, session, &dir)?;
    Tasks::load(&tx, project)?.validate().context("refused")?;
    tx.commit()?;
    Ok(ids.iter().map(|&id| tasks.reference(id)).collect())
}

/// Records `session` and its directory on the tasks, and starts the `todo`
/// ones.
fn record(tx: &Connection, tasks: &Tasks, ids: &[i64], session: &str, dir: &Path) -> Result<()> {
    let dir = dir
        .to_str()
        .with_context(|| format!("{} isn't valid UTF-8", dir.display()))?;
    for &id in ids {
        let task = tasks.get(id);
        let new_status = if tasks.effective_status(id) == Status::Todo {
            Some(Status::InProgress)
        } else {
            task.status
        };
        tx.execute(
            "UPDATE tasks SET claude_session = ?1, claude_cwd = ?2, status = ?3, updated_at = ?4
             WHERE id = ?5",
            params![session, dir, new_status, crate::now(), id],
        )?;
    }
    Ok(())
}

/// The directory a saved conversation began in: the first `cwd` it records.
fn began_in(conversation: &Path) -> Option<PathBuf> {
    use std::io::BufRead;
    let file = std::io::BufReader::new(std::fs::File::open(conversation).ok()?);
    file.lines().map_while(Result::ok).find_map(|line| {
        let entry: serde_json::Value = serde_json::from_str(&line).ok()?;
        Some(PathBuf::from(entry.get("cwd")?.as_str()?))
    })
}

/// Whether Claude Code saved `session`'s conversation: it keeps each as
/// `projects/<dir>/<session>.jsonl` under `$CLAUDE_CONFIG_DIR`, else
/// `~/.claude`. When there's no projects folder to look in, assume it did.
fn conversation_saved(session: &str) -> bool {
    projects_dir().is_none_or(|_| conversation_file(session).is_some())
}

fn conversation_file(session: &str) -> Option<PathBuf> {
    let file = format!("{session}.jsonl");
    std::fs::read_dir(projects_dir()?)
        .ok()?
        .flatten()
        .map(|d| d.path().join(&file))
        .find(|f| f.is_file())
}

fn projects_dir() -> Option<PathBuf> {
    let config = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude")))?;
    Some(config.join("projects")).filter(|p| p.is_dir())
}

/// The prompt a session starts with: the planner's for the root, otherwise
/// the work session's for the tasks, as `selection` returns them. Rendered
/// from the tasks as they are now, so a session that lost it, to compaction
/// say, gets back an up-to-date copy.
pub fn prompt(tasks: &Tasks, ids: &[i64]) -> String {
    match ids {
        [id] if tasks.get(*id).is_root() => planner_prompt(tasks),
        _ => work_prompt(tasks, ids, on_path(Path::new("choros")).is_some()),
    }
}

fn planner_prompt(tasks: &Tasks) -> String {
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

/// One task the session is to complete, with its subtree.
#[derive(Serialize)]
struct Assigned<'a> {
    reference: String,
    #[serde(flatten)]
    intent: Intent<'a>,
    subtasks: Vec<Outlined<'a>>,
}

fn work_prompt(tasks: &Tasks, ids: &[i64], choros: bool) -> String {
    // Every task's ancestors, merged into one outline in tree order, so
    // tasks that share a parent share its context.
    let wanted: std::collections::HashSet<i64> =
        ids.iter().flat_map(|&id| tasks.ancestors(id)).collect();
    let root = tasks.root().id;
    let ancestors: Vec<Outlined> = std::iter::once(root)
        .chain(tasks.descendants(root))
        .filter(|a| wanted.contains(a))
        .map(|a| Outlined {
            intent: Intent::of(&tasks.get(a).intent),
            pad: "  ".repeat(tasks.ancestors(a).len()),
            marker: None,
        })
        .collect();
    let assigned: Vec<Assigned> = ids
        .iter()
        .map(|&id| {
            let mut subtasks = Vec::new();
            collect_subtasks(tasks, id, 0, &mut subtasks);
            Assigned {
                reference: tasks.reference(id),
                intent: Intent::of(&tasks.get(id).intent),
                subtasks,
            }
        })
        .collect();

    let mut ctx = tera::Context::new();
    ctx.insert("ancestors", &ancestors);
    ctx.insert("tasks", &assigned);
    ctx.insert("choros", &choros);
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
            ops::add(conn, &p, parent, intent, Default::default()).unwrap()
        };
        let a = add(&mut conn, None, "Backend\nRust services.");
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
        let prompt = work_prompt(&tasks, &[tasks.live(&b).unwrap().id], false);
        assert_eq!(
            prompt,
            format!(
                "Greater context that your work sits within:

- Build a demo
  Project context.
  - Backend
    Rust services.

Task to complete here (demo/{b}):

Write the API

Use REST.

- Routes
  Under /api.

  Versioned.
  - Nested
    One level down.
- [done] Auth

These are ergon tasks; change them with the `ergon` CLI (`ergon help` lists
its commands, `ergon tree` shows each task's reference). As the work goes:

- Once all of a task's work is in PRs, set it to landing:
  `ergon edit REF --status landing`.
- Until then, leave it in_progress, and set each subtask whose work is all in
  PRs to landing.
- Name the task references (like demo/{b}) in each PR's title or body.
- `ergon work` resumes this session only from the folder it began in, so
  don't move or archive that folder.

Ask the user how they'd like to start.
"
            )
        );

        // With choros installed, the session is pointed at it.
        let leaf = work_prompt(&tasks, &[tasks.live(&n).unwrap().id], true);
        assert!(leaf.ends_with(&format!(
            "  - Backend
    Rust services.
    - Write the API
      Use REST.
      - Routes
        Under /api.

        Versioned.

Task to complete here (demo/{n}):

Nested

One level down.

These are ergon tasks; change them with the `ergon` CLI (`ergon help` lists
its commands, `ergon tree` shows each task's reference). As the work goes:

- Once all of a task's work is in PRs, set it to landing:
  `ergon edit REF --status landing`.
- Until then, leave it in_progress, and set each subtask whose work is all in
  PRs to landing.
- Name the task references (like demo/{n}) in each PR's title or body.
- `ergon work` resumes this session only from the folder it began in, so
  don't move or archive that folder.

choros is installed. If this folder is in a choros space, `choros here` says
what it is and where a land from it goes, and `choros help agent` says how to
work in it. If this session began in a choros workspace, don't run
`choros land archive` or `choros archive` on it.

Ask the user how they'd like to start.
"
        )));
    }
}
