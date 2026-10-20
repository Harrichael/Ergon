use anyhow::Result;
use clap::{Parser, Subcommand};
use rusqlite::Connection;

use anstream::{print, println};
use ergon::ops::{self, Changes};
use ergon::project::{self, Project};
use ergon::refs::{self, TaskRef};
use ergon::task::{self, Status, Tasks};

use ergon::derived::Derived;
use ergon::{db, fmt, view};

/// Minimal task tracker for dispatching work to Claude Code sessions.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Project to act on, instead of the one named by the nearest .ergon file.
    #[arg(long, global = true)]
    project: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a project and its root task, and write .ergon here.
    Init { name: String, root_intent: String },
    /// Manage projects.
    #[command(subcommand)]
    Project(ProjectCommand),

    /// Create a task and print its slug.
    Add {
        intent: String,
        /// Parent task (default: the root).
        #[arg(long)]
        parent: Option<TaskRef>,
        #[arg(long, value_parser = parse_status)]
        status: Option<Status>,
        #[arg(long, value_parser = parse_priority)]
        priority: Option<u8>,
    },
    /// Change a task's intent, status or priority.
    Edit {
        task: TaskRef,
        #[arg(long)]
        intent: Option<String>,
        /// A status, or `none` to follow the parent's.
        #[arg(long, value_parser = parse_status_or_none)]
        status: Option<OrNone<Status>>,
        /// A priority, or `none` to infer it.
        #[arg(long, value_parser = parse_priority_or_none)]
        priority: Option<OrNone<u8>>,
    },
    /// Delete a task and its subtree. The counts must match `show`.
    Rm {
        task: TaskRef,
        /// Number of subtasks, at all depths, that will be deleted.
        #[arg(long)]
        subtasks: usize,
        /// Number of tasks outside the subtree that depend on it.
        #[arg(long)]
        dependents: usize,
    },
    /// Undo a deletion.
    Restore { task: TaskRef },
    /// Move a task, with its subtree, under a new parent.
    Mv { task: TaskRef, new_parent: TaskRef },
    /// Show one task in full.
    Show { task: TaskRef },
    /// Show the task tree (default: from the root).
    Tree { task: Option<TaskRef> },
    /// List the coarsest open, unblocked work, most urgent first.
    Ls,
    /// Manage dependencies.
    #[command(subcommand)]
    Dep(DepCommand),
}

#[derive(Subcommand)]
enum DepCommand {
    /// Record that a task depends on another.
    Add { task: TaskRef, depends_on: TaskRef },
    /// Remove a dependency.
    Rm { task: TaskRef, depends_on: TaskRef },
}

#[derive(Subcommand)]
enum ProjectCommand {
    /// List projects.
    Ls {
        /// Include retired projects.
        #[arg(long)]
        all: bool,
    },
    /// Rename a project.
    Rename { old: String, new: String },
    /// Hide a project from `project ls`.
    Retire { name: String },
    /// Undo `retire`.
    Unretire { name: String },
}

/// A value, or `none` to unset it.
#[derive(Clone)]
struct OrNone<T>(Option<T>);

fn parse_status(s: &str) -> Result<Status, String> {
    Status::parse(s).map_err(|e| e.to_string())
}

fn parse_priority(s: &str) -> Result<u8, String> {
    task::parse_priority(s).map_err(|e| e.to_string())
}

fn parse_status_or_none(s: &str) -> Result<OrNone<Status>, String> {
    if s == "none" {
        Ok(OrNone(None))
    } else {
        parse_status(s).map(|v| OrNone(Some(v)))
    }
}

fn parse_priority_or_none(s: &str) -> Result<OrNone<u8>, String> {
    if s == "none" {
        Ok(OrNone(None))
    } else {
        parse_priority(s).map(|v| OrNone(Some(v)))
    }
}

fn main() {
    if let Err(e) = run(Cli::parse()) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let mut conn = db::open(&db::default_path()?)?;
    let cwd = std::env::current_dir()?;
    let flag = cli.project.as_deref();
    let project_for =
        |conn: &Connection, refs: &[&TaskRef]| refs::project_for(conn, flag, &cwd, refs);

    match cli.command {
        Command::Init { name, root_intent } => {
            let p = project::init(&mut conn, &name, &root_intent, &cwd)?;
            println!("Created project {} in {}", p.name, cwd.display());
        }
        Command::Project(cmd) => run_project(&conn, cmd)?,

        Command::Add {
            intent,
            parent,
            status,
            priority,
        } => {
            let p = project_for(&conn, &parent.iter().collect::<Vec<_>>())?;
            let parent = parent.as_ref().map(|r| r.slug.as_str());
            println!(
                "{}",
                ops::add(&mut conn, &p, parent, &intent, status, priority)?
            );
        }
        Command::Edit {
            task,
            intent,
            status,
            priority,
        } => {
            let p = project_for(&conn, &[&task])?;
            let changes = Changes {
                intent,
                status: status.map(|s| s.0),
                priority: priority.map(|p| p.0),
            };
            for r in ops::edit(&mut conn, &p, &task.slug, changes)? {
                println!("Also cancelled {r}");
            }
        }
        Command::Mv { task, new_parent } => {
            let p = project_for(&conn, &[&task, &new_parent])?;
            ops::mv(&mut conn, &p, &task.slug, &new_parent.slug)?;
        }
        Command::Rm {
            task,
            subtasks,
            dependents,
        } => {
            let p = project_for(&conn, &[&task])?;
            ops::rm(&mut conn, &p, &task.slug, subtasks, dependents)?;
        }
        Command::Restore { task } => {
            let p = project_for(&conn, &[&task])?;
            for r in ops::restore(&mut conn, &p, &task.slug)? {
                println!("Restored {r}");
            }
        }
        Command::Show { task } => {
            let p = project_for(&conn, &[&task])?;
            let tasks = Tasks::load(&conn, &p)?;
            let id = tasks.live(&task.slug)?.id;
            let derived = Derived::compute(&tasks);
            print!("{}", view::show(&tasks, &derived, id, ergon::now()));
        }
        Command::Tree { task } => {
            let p = project_for(&conn, &task.iter().collect::<Vec<_>>())?;
            print!("{}", tree(&conn, &p, task.as_ref())?);
        }
        Command::Ls => {
            let tasks = Tasks::load(&conn, &project_for(&conn, &[])?)?;
            print!("{}", view::ls(&tasks, &Derived::compute(&tasks)));
        }
        Command::Dep(DepCommand::Add { task, depends_on }) => {
            let p = project_for(&conn, &[&task, &depends_on])?;
            ops::dep_add(&mut conn, &p, &task.slug, &depends_on.slug)?;
        }
        Command::Dep(DepCommand::Rm { task, depends_on }) => {
            let p = project_for(&conn, &[&task, &depends_on])?;
            ops::dep_rm(&mut conn, &p, &task.slug, &depends_on.slug)?;
        }
    }
    Ok(())
}

fn tree(conn: &Connection, p: &Project, task: Option<&TaskRef>) -> Result<String> {
    let tasks = Tasks::load(conn, p)?;
    let id = match task {
        Some(r) => tasks.live(&r.slug)?.id,
        None => tasks.root().id,
    };
    Ok(view::tree(&tasks, &Derived::compute(&tasks), id))
}

fn run_project(conn: &Connection, cmd: ProjectCommand) -> Result<()> {
    match cmd {
        ProjectCommand::Ls { all } => {
            let now = ergon::now();
            for p in project::list(conn, all)? {
                let retired = if p.retired_at.is_some() {
                    "  retired"
                } else {
                    ""
                };
                println!("{}  {}{retired}", p.name, fmt::timestamp(p.created_at, now));
            }
        }
        ProjectCommand::Rename { old, new } => project::rename(conn, &old, &new)?,
        ProjectCommand::Retire { name } => project::set_retired(conn, &name, true)?,
        ProjectCommand::Unretire { name } => project::set_retired(conn, &name, false)?,
    }
    Ok(())
}
