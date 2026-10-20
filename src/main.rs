use anyhow::{Result, bail};
use clap::{CommandFactory, Parser, Subcommand};
use rusqlite::Connection;

use anstream::{print, println};
use ergon::ops::{self, Changes};
use ergon::project::{self, Project};
use ergon::refs::{self, TaskRef};
use ergon::task::{self, Status, Tasks};

use ergon::derived::Derived;
use ergon::{claude, db, fmt, outline, view};
use std::io::Read;

/// Minimal task tracker for dispatching work to Claude Code sessions.
#[derive(Parser)]
#[command(version, disable_help_subcommand = true)]
struct Cli {
    /// Project to act on, instead of the one named by the nearest .ergon file.
    #[arg(long, global = true)]
    project: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a project and its root task, write .ergon here, and launch the
    /// planner session.
    Init {
        name: String,
        root_intent: String,
        /// Don't launch the planner; start it later with `ergon work root`.
        #[arg(long)]
        no_launch: bool,
    },
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
    Show {
        task: TaskRef,
        /// Print only the intent.
        #[arg(long)]
        intent: bool,
    },
    /// Show the task tree (default: from the root).
    Tree { task: Option<TaskRef> },
    /// List the coarsest open, unblocked work, most urgent first.
    Ls,
    /// Print the project as an editable outline, or apply an edited one
    ///
    /// Each line is one task, nested by indentation:
    ///
    ///     <slug> [<box>] <title> [!p<N>] [deps:<slug>,<slug>]
    ///
    /// The box is the status: `[ ]` todo, `[~]` in_progress, `[>]` landing,
    /// `[x]` done, `[-]` cancelled, or no box to follow the parent. The title
    /// is the intent's first line; editing it keeps the rest of the intent.
    /// A line without a slug is a new task, and a task whose line is removed
    /// is deleted along with its subtasks, unless they were moved elsewhere.
    /// Sibling order isn't stored.
    ///
    /// `--apply` reads the edited outline from stdin, applies every change in
    /// one transaction (or none, if any is refused) and prints them. It
    /// refuses an outline printed before the project last changed.
    #[command(verbatim_doc_comment)]
    Outline {
        /// Read an edited outline from stdin and apply it.
        #[arg(long)]
        apply: bool,
        /// With --apply: print the changes without making them.
        #[arg(long, requires = "apply")]
        dry_run: bool,
        /// Print JSON for editors: the outline, plus each line's effective
        /// priority and blocked state.
        #[arg(long, conflicts_with = "apply")]
        json: bool,
    },
    /// Manage dependencies.
    #[command(subcommand)]
    Dep(DepCommand),
    /// Start or resume the Claude Code session for a task.
    Work { task: TaskRef },
    /// Print help for ergon or a command; `ergon help planner` prints the
    /// planner's instructions.
    Help { topic: Option<String> },
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
    if let Command::Help { topic } = &cli.command {
        return help(topic.as_deref());
    }
    let mut conn = db::open(&db::default_path()?)?;
    let cwd = std::env::current_dir()?;
    let flag = cli.project.as_deref();
    let project_for =
        |conn: &Connection, refs: &[&TaskRef]| refs::project_for(conn, flag, &cwd, refs);

    match cli.command {
        Command::Init {
            name,
            root_intent,
            no_launch,
        } => {
            let p = project::init(&mut conn, &name, &root_intent, &cwd)?;
            println!("Created project {} in {}", p.name, cwd.display());
            if !no_launch {
                launch(claude::work(&mut conn, &p, project::ROOT_SLUG, &cwd)?)?;
            }
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
        Command::Show { task, intent } => {
            let p = project_for(&conn, &[&task])?;
            let tasks = Tasks::load(&conn, &p)?;
            let id = tasks.live(&task.slug)?.id;
            if intent {
                println!("{}", tasks.get(id).intent);
                return Ok(());
            }
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
        Command::Outline {
            apply: true,
            dry_run,
            ..
        } => {
            let mut text = String::new();
            std::io::stdin().read_to_string(&mut text)?;
            let (name, _) = outline::header(&text)?;
            if flag.is_some_and(|f| f != name) {
                bail!("the outline is for project {name}, not {}", flag.unwrap());
            }
            let p = project::by_name(&conn, &name)?;
            for change in outline::apply(&mut conn, &p, &text, dry_run)? {
                println!("{change}");
            }
        }
        Command::Outline { json, .. } => {
            let tasks = Tasks::load(&conn, &project_for(&conn, &[])?)?;
            if json {
                print!("{}", outline::json(&tasks, &Derived::compute(&tasks)));
            } else {
                print!("{}", outline::render(&tasks).text);
            }
        }
        Command::Work { task } => {
            let p = project_for(&conn, &[&task])?;
            launch(claude::work(&mut conn, &p, &task.slug, &cwd)?)?;
        }
        Command::Help { .. } => unreachable!("handled above"),
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

fn launch(l: claude::Launch) -> Result<()> {
    if let Some(w) = &l.warning {
        anstream::eprintln!("warning: {w}");
    }
    l.exec()
}

fn help(topic: Option<&str>) -> Result<()> {
    let mut cmd = Cli::command();
    match topic {
        None => cmd.print_help()?,
        Some("planner") => print!("{}", claude::PLANNER_DOC),
        Some(name) if cmd.find_subcommand(name).is_some() => {
            if let Err(e) = Cli::try_parse_from(["ergon", name, "--help"]) {
                e.print()?;
            }
        }
        Some(name) => bail!("no help topic {name:?}; try `ergon help`"),
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
