use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, Subcommand};
use rusqlite::Connection;

use anstream::{print, println};
use ergon::ops::{self, Changes};
use ergon::project::{self, Project};
use ergon::refs::{self, TaskRef, TaskRefs};
use ergon::task::{self, Status, Tasks};

use ergon::derived::{Derived, Filter, Range, SortField};
use ergon::{claude, db, export, fmt, outline, picker, view};
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
        /// A Fibonacci number. A sized task is one unit of work: its subtasks
        /// are steps, and no task above or below it can have a size.
        #[arg(long, value_parser = parse_size)]
        size: Option<u32>,
        /// How much the task needs you: 1 (just execute), 5 (steering
        /// recommended) to 10 (your decision).
        #[arg(long, value_parser = parse_attention)]
        attention: Option<u8>,
    },
    /// Change a task's intent, status, priority, size or attention.
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
        /// A Fibonacci number, or `none` to add it up from the subtasks.
        #[arg(long, value_parser = parse_size_or_none)]
        size: Option<OrNone<u32>>,
        /// 1 to 10, or `none` for unknown.
        #[arg(long, value_parser = parse_attention_or_none)]
        attention: Option<OrNone<u8>>,
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
    Tree {
        task: Option<TaskRef>,
        /// Mark the tasks `ergon ls` would list.
        #[arg(long)]
        ls: bool,
    },
    /// List the coarsest open, unblocked work, most urgent first
    ///
    /// --sort orders by priority, attention, size (what's left), created or
    /// updated instead, ascending unless followed by desc. Tasks without a
    /// value go last either way, and ties stay most urgent, then oldest.
    Ls {
        /// A field, then optionally asc or desc: `--sort attention desc`.
        #[arg(long, num_args = 1..=2, value_names = ["FIELD", "ORDER"])]
        sort: Option<Vec<String>>,
        /// Show each task's subtasks under it, indented.
        #[arg(long)]
        subtasks: bool,
        /// Group tasks under the parents they share, shown only where two or
        /// more listed tasks meet.
        #[arg(long, conflicts_with = "ancestors")]
        groups: bool,
        /// Show tasks under all their ancestors, from the root down.
        #[arg(long)]
        ancestors: bool,
        /// Only tasks whose priority is in RANGE: `p0..p1`, `..p2`, `p3`, or
        /// `none`.
        #[arg(long, value_name = "RANGE", value_parser = parse_priority_range)]
        priority: Option<Range>,
        /// Only tasks whose attention is in RANGE: `..3`, `7..`, `4..6`, `5`,
        /// or `none` for unrated.
        #[arg(long, value_name = "RANGE", value_parser = parse_attention_range)]
        attention: Option<Range>,
        /// Only tasks whose size left is in RANGE: `..3`, `8..`, or `none`
        /// for unsized.
        #[arg(long, value_name = "RANGE", value_parser = parse_size_range)]
        size: Option<Range>,
        /// Only tasks with these statuses, comma-separated: `todo`,
        /// `in_progress`, or both (the only ones `ls` lists).
        #[arg(long, value_name = "STATUS,...", value_parser = parse_ls_statuses)]
        status: Option<LsStatuses>,
        /// Show each task's full intent: its body under its row.
        #[arg(long)]
        intent: bool,
    },
    /// Print the project as an editable table, or apply an edited one
    ///
    /// Each row is one task: eleven cells, then the title.
    ///
    ///     <slug> <status> ~<status> <priority> ~<priority> <size> ~<size> <attention> ~<attention> <deps> ~blocked <title>
    ///
    /// `-` is an empty cell. Cells starting with `~` are calculated: the
    /// actual status, the effective priority, the size added up from
    /// subtasks, the effective attention (empty when unknown), and whether
    /// the task is blocked. Editing one is refused. Status `-` follows the
    /// parent's; priority is `p0`..`p4`; size is a Fibonacci number;
    /// attention is `a1`..`a10`; deps are slugs, comma-separated.
    ///
    /// The title is the intent's first line; editing it keeps the rest of the
    /// intent. The root's comes first and bare; every other title starts with
    /// `- `, indented two spaces per level under the root. A row with slug `-`
    /// is a new task, and a task whose row is removed is deleted along with
    /// its subtasks, unless they were moved elsewhere. The order of siblings
    /// is kept, though it carries no meaning.
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
        /// Read an edited outline from stdin and print it lined up again,
        /// titles re-indented from their nesting. Applies nothing.
        #[arg(long, conflicts_with_all = ["apply", "json"])]
        format: bool,
        /// Print JSON for editors: the outline, plus each line's effective
        /// priority, size and blocked state.
        #[arg(long, conflicts_with = "apply")]
        json: bool,
    },
    /// Manage dependencies.
    #[command(subcommand)]
    Dep(DepCommand),
    /// Print projects in full as JSON, for backups or another database
    ///
    /// Every project by default, retired ones included; one with --project.
    /// The format doesn't depend on the database schema, and later versions
    /// of ergon can still import it.
    Export,
    /// Add the projects in an export (a file, or stdin) to this database
    ///
    /// They keep their UUIDs, so their .ergon files still find them. All or
    /// nothing: a project that's already in the database is refused.
    Import { file: Option<std::path::PathBuf> },
    /// Start or resume the Claude Code session for a task, or for several
    ///
    /// Tasks given as `slug,slug,...` are taken on together, in one session
    /// that every one of them records. Once they have it, `ergon work` on any
    /// of them resumes it.
    #[command(group(clap::ArgGroup::new("which").required(true).args(["tasks", "interactive"])))]
    Work {
        /// Comma-separated, or `-` to read them from stdin, as in
        /// `ergon ls --attention ..3 | ergon work -`.
        tasks: Option<WorkTasks>,
        /// Pick the tasks from `ergon ls` instead.
        #[arg(short, long)]
        interactive: bool,
    },
    /// Record the Claude Code session you're in on a task, or on several
    ///
    /// For a session ergon didn't start: from then on, `ergon work` resumes
    /// it, as if `ergon work` had started it. Run it from the session's Bash
    /// tool, where Claude Code sets $CLAUDE_CODE_SESSION_ID.
    Attach {
        tasks: TaskRefs,
        /// The session, instead of $CLAUDE_CODE_SESSION_ID.
        #[arg(long)]
        session: Option<String>,
    },
    /// Print the prompt `ergon work` starts a session with
    ///
    /// Rendered from the tasks as they are now, not as they were when the
    /// session started. Changes nothing.
    Prompt { tasks: TaskRefs },
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

/// The tasks `ergon work` was given, or `-` for the ones piped in.
#[derive(Clone)]
enum WorkTasks {
    Given(TaskRefs),
    Stdin,
}

impl std::str::FromStr for WorkTasks {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        if s == "-" {
            Ok(WorkTasks::Stdin)
        } else {
            s.parse().map(WorkTasks::Given)
        }
    }
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

fn parse_size(s: &str) -> Result<u32, String> {
    task::parse_size(s).map_err(|e| e.to_string())
}

fn parse_size_or_none(s: &str) -> Result<OrNone<u32>, String> {
    if s == "none" {
        Ok(OrNone(None))
    } else {
        parse_size(s).map(|v| OrNone(Some(v)))
    }
}

fn parse_attention(s: &str) -> Result<u8, String> {
    task::parse_attention(s).map_err(|e| e.to_string())
}

/// `ls --status`: open statuses only, since `ls` lists nothing else.
#[derive(Clone)]
struct LsStatuses(Vec<Status>);

fn parse_ls_statuses(s: &str) -> Result<LsStatuses, String> {
    s.split(',')
        .map(|part| {
            let status = Status::parse(part).map_err(|e| e.to_string())?;
            if !status.is_open() {
                return Err(format!(
                    "ergon ls lists only todo and in_progress tasks, never {}; see them in ergon tree",
                    status.as_str()
                ));
            }
            Ok(status)
        })
        .collect::<Result<_, _>>()
        .map(LsStatuses)
}

fn parse_priority_range(s: &str) -> Result<Range, String> {
    Range::parse(s, |b| task::parse_priority(b).map(i64::from)).map_err(|e| e.to_string())
}

fn parse_attention_range(s: &str) -> Result<Range, String> {
    Range::parse(s, |b| task::parse_attention(b).map(i64::from)).map_err(|e| e.to_string())
}

/// Bounds can be any count, not only Fibonacci: what's left is a sum.
fn parse_size_range(s: &str) -> Result<Range, String> {
    let count = |b: &str| {
        b.parse::<i64>()
            .ok()
            .filter(|&n| n >= 0)
            .with_context(|| format!("invalid size {b:?}: expected a whole number"))
    };
    Range::parse(s, count).map_err(|e| e.to_string())
}

fn parse_attention_or_none(s: &str) -> Result<OrNone<u8>, String> {
    if s == "none" {
        Ok(OrNone(None))
    } else {
        parse_attention(s).map(|v| OrNone(Some(v)))
    }
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
                let root = [project::ROOT_SLUG.to_string()];
                launch(claude::work(&mut conn, &p, &root, &cwd)?, None)?;
            }
        }
        Command::Project(cmd) => run_project(&conn, cmd)?,

        Command::Add {
            intent,
            parent,
            status,
            priority,
            size,
            attention,
        } => {
            let p = project_for(&conn, &parent.iter().collect::<Vec<_>>())?;
            let parent = parent.as_ref().map(|r| r.slug.as_str());
            println!(
                "{}",
                ops::add(
                    &mut conn,
                    &p,
                    parent,
                    &intent,
                    ops::Fields {
                        status,
                        priority,
                        size,
                        attention,
                    }
                )?
            );
        }
        Command::Edit {
            task,
            intent,
            status,
            priority,
            size,
            attention,
        } => {
            let p = project_for(&conn, &[&task])?;
            let changes = Changes {
                intent,
                status: status.map(|s| s.0),
                priority: priority.map(|p| p.0),
                size: size.map(|s| s.0),
                attention: attention.map(|a| a.0),
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
        Command::Tree { task, ls } => {
            let p = project_for(&conn, &task.iter().collect::<Vec<_>>())?;
            print!("{}", tree(&conn, &p, task.as_ref(), ls)?);
        }
        Command::Ls {
            sort,
            subtasks,
            groups,
            ancestors,
            priority,
            attention,
            size,
            status,
            intent,
        } => {
            let sort = match sort.as_deref() {
                None => Default::default(),
                Some([field, order @ ..]) => {
                    ergon::derived::Sort::parse(field, order.first().map(String::as_str))?
                }
                Some([]) => unreachable!("clap requires a field"),
            };
            let nest = match (groups, ancestors) {
                (true, _) => view::Nest::Groups,
                (_, true) => view::Nest::Ancestors,
                _ => view::Nest::Flat,
            };
            let filter = Filter {
                ranges: [
                    (SortField::Priority, priority),
                    (SortField::Attention, attention),
                    (SortField::Size, size),
                ]
                .into_iter()
                .filter_map(|(field, range)| Some((field, range?)))
                .collect(),
                statuses: status.map(|s| s.0),
            };
            let tasks = Tasks::load(&conn, &project_for(&conn, &[])?)?;
            print!(
                "{}",
                view::ls(
                    &tasks,
                    &Derived::compute(&tasks),
                    sort,
                    &filter,
                    view::Layout {
                        subtasks,
                        nest,
                        intent,
                        columns: columns(),
                    },
                )
            );
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
        Command::Outline { format: true, .. } => {
            let mut text = String::new();
            std::io::stdin().read_to_string(&mut text)?;
            print!("{}", outline::format(&text)?);
        }
        Command::Outline { json, .. } => {
            let tasks = Tasks::load(&conn, &project_for(&conn, &[])?)?;
            if json {
                print!("{}", outline::json(&tasks));
            } else {
                print!("{}", outline::render(&tasks).text);
            }
        }
        Command::Export => {
            let projects = match flag {
                Some(name) => vec![project::by_name(&conn, name)?],
                None => project::list(&conn, true)?,
            };
            print!("{}", export::export(&conn, &projects)?);
        }
        Command::Import { file } => {
            let json = match &file {
                Some(path) => std::fs::read_to_string(path)
                    .with_context(|| format!("reading {}", path.display()))?,
                None => {
                    let mut s = String::new();
                    std::io::stdin().read_to_string(&mut s)?;
                    s
                }
            };
            for (name, tasks) in export::import(&mut conn, &json)? {
                let s = if tasks == 1 { "" } else { "s" };
                println!("Imported {name} ({tasks} task{s})");
            }
        }
        Command::Work { tasks, .. } => {
            let (tasks, tty) = match tasks {
                Some(WorkTasks::Given(t)) => (Some(t), None),
                Some(WorkTasks::Stdin) => {
                    let text = std::io::read_to_string(std::io::stdin())?;
                    let t = TaskRefs::from_listing(&text)?;
                    if t.0.is_empty() {
                        bail!("no tasks on stdin");
                    }
                    (Some(t), Some(terminal()?))
                }
                None => (None, None),
            };
            let refs: Vec<&TaskRef> = tasks.iter().flat_map(|t| t.iter()).collect();
            let p = project_for(&conn, &refs)?;
            let l = match &tasks {
                Some(t) => claude::work(&mut conn, &p, &t.slugs(), &cwd)?,
                None => match picker::run(&mut conn, &p, &cwd)? {
                    Some(l) => l,
                    None => return Ok(()),
                },
            };
            launch(l, tty)?;
        }
        Command::Attach { tasks, session } => {
            let session = session
                .or_else(|| std::env::var("CLAUDE_CODE_SESSION_ID").ok())
                .filter(|s| !s.is_empty())
                .context("no session: run this inside Claude Code, or give --session")?;
            let p = project_for(&conn, &tasks.iter().collect::<Vec<_>>())?;
            let refs = claude::attach(&mut conn, &p, &tasks.slugs(), &session, &cwd)?;
            println!(
                "attached session {session} to {}; `ergon prompt {}` prints its instructions",
                refs.join(", "),
                refs.join(",")
            );
        }
        Command::Prompt { tasks } => {
            let p = project_for(&conn, &tasks.iter().collect::<Vec<_>>())?;
            let all = Tasks::load(&conn, &p)?;
            let ids = claude::selection(&all, &tasks.slugs())?;
            print!("{}", claude::prompt(&all, &ids));
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

/// The terminal, for claude's stdin when the pipe took ours: a copy of
/// stdout or stderr, which the shell opened read-write on it. Not
/// `/dev/tty`: macOS's kqueue refuses that device, and claude (on Bun)
/// watches its stdin with kqueue.
fn terminal() -> Result<std::fs::File> {
    use std::io::IsTerminal;
    use std::os::fd::AsFd;
    let fd = if std::io::stdout().is_terminal() {
        std::io::stdout().as_fd().try_clone_to_owned()?
    } else if std::io::stderr().is_terminal() {
        std::io::stderr().as_fd().try_clone_to_owned()?
    } else {
        bail!("`ergon work -` needs a terminal for the session");
    };
    Ok(fd.into())
}

/// The width to wrap to: `COLUMNS` when set, else the terminal's, else none.
fn columns() -> Option<usize> {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse().ok())
        .or_else(|| crossterm::terminal::size().ok().map(|(w, _)| w.into()))
}

fn launch(l: claude::Launch, stdin: Option<std::fs::File>) -> Result<()> {
    if let Some(w) = &l.warning {
        anstream::eprintln!("warning: {w}");
    }
    l.exec(stdin)
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

fn tree(conn: &Connection, p: &Project, task: Option<&TaskRef>, ls: bool) -> Result<String> {
    let tasks = Tasks::load(conn, p)?;
    let id = match task {
        Some(r) => tasks.live(&r.slug)?.id,
        None => tasks.root().id,
    };
    let derived = Derived::compute(&tasks);
    let mark = ls.then(|| {
        ergon::derived::ls(&tasks, &derived, Default::default())
            .into_iter()
            .collect::<std::collections::HashSet<_>>()
    });
    Ok(view::tree(&tasks, &derived, id, mark.as_ref()))
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
