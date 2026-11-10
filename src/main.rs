use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, Subcommand};
use rusqlite::Connection;

use anstream::{print, println};
use ergon::ops::{self, Changes};
use ergon::project::{self, Project};
use ergon::refs::{self, TaskRef, TaskRefs};
use ergon::task::{self, Status, Tasks};

use ergon::derived::{Derived, Filter, Range, SortField};
use ergon::{claude, db, export, fmt, gantt, kanban, land, landings, outline, picker, view};
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
    /// Change a task's intent, status, priority, size or attention
    ///
    /// Several tasks, as `slug,slug,...`, all take the same change, or none
    /// does if one is refused.
    Edit {
        tasks: TaskRefs,
        /// The new intent, or `-` to read it from stdin.
        #[arg(long)]
        intent: Option<String>,
        /// Text to add to the intent as a new paragraph, or `-` to read it
        /// from stdin.
        #[arg(long, conflicts_with = "intent")]
        append: Option<String>,
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
    /// Show a task in full, with the choros landing sets that carry it
    ///
    /// Several tasks, as `slug,slug,...`, are shown one after another.
    Show {
        #[arg(required_unless_present = "pr")]
        tasks: Option<TaskRefs>,
        /// Print only the intent.
        #[arg(long)]
        intent: bool,
        /// List the landing sets that have merged too, not just their number.
        #[arg(long)]
        merged: bool,
        /// Instead, show the landing sets of this PR and the tasks they carry.
        #[arg(long, conflicts_with_all = ["tasks", "intent", "merged"])]
        pr: Option<u64>,
    },
    /// Show the task tree (default: from the root).
    Tree {
        task: Option<TaskRef>,
        /// Mark the tasks `ergon ls` would list.
        #[arg(long)]
        ls: bool,
    },
    /// Chart the order open work can run in
    ///
    /// One bar per open child of TASK (default: the root). A bar runs from
    /// when the first work under it can start to when the last ends, each
    /// piece starting once what it waits on is done, with no limit on how
    /// much runs at once. There are no dates: the axis counts size, and a
    /// task of size 5 runs for 5. Unsized work runs for 1. The steps of a
    /// sized task run one after another, sharing its size, and work waiting
    /// on a step waits only on that step. Landing, done and cancelled tasks
    /// don't show, and hold nothing back.
    Gantt {
        task: Option<TaskRef>,
        /// Measure work by attention instead of size.
        #[arg(long)]
        attention: bool,
        /// Mark the work `ergon ls` would list.
        #[arg(long)]
        ls: bool,
    },
    /// Show the project as a board, a column per status
    ///
    /// Todo splits into ready and blocked. Landing splits by how far each
    /// task's landings have got, as choros reports them: in the merge
    /// queue, a draft PR, a PR waiting for comments, or one with comments
    /// its author hasn't answered. A task with several sets sits where a
    /// reply is due, else a draft, else the queue. The cards are what
    /// `ergon ls` would list, found the same way across every status: a
    /// landing, done or blocked task is one card, and an open task splits
    /// into its subtasks only where blocked work is under it. Cancelled
    /// tasks don't show, nor do done ones older than --done. Each cell is
    /// ordered as `ergon ls --sort` orders its rows.
    Kanban {
        /// A field, then optionally asc or desc: `--sort blocks desc`.
        #[arg(long, num_args = 1..=2, value_names = ["FIELD", "ORDER"])]
        sort: Option<Vec<String>>,
        /// Show every task with no subtasks instead.
        #[arg(long)]
        leaves: bool,
        /// Show tasks done in the last DAYS days, or `all`.
        #[arg(long, value_name = "DAYS|all", default_value = "14", value_parser = parse_done_days)]
        done: DoneDays,
    },
    /// List the coarsest open, unblocked work, most urgent first
    ///
    /// --sort orders by priority, attention, size (what's left), blocks (the
    /// size of open work waiting on it), created or updated instead,
    /// ascending unless followed by desc. Tasks without a value go last
    /// either way, and ties stay most urgent, then oldest.
    ///
    /// Given a task, only the work at or under it: the task itself if it's
    /// what ls would list, else what it splits into.
    Ls {
        task: Option<TaskRef>,
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
    ///
    /// Where choros makes workspaces (a trunk or a branch-space of its),
    /// a session that starts gets a workspace of its own, bound to the
    /// tasks: `choros land` from it then moves them (see `ergon help
    /// land-hook`). Its repos are --repos, else the project's default
    /// (`ergon project repos`); with neither, starting is refused. Inside
    /// a workspace, or away from choros, the session starts in this folder.
    ///
    /// The session resumes only from the folder it began in. Once that is
    /// gone, as after `choros land archive`, this opens the landing folder
    /// of the open set that carries the tasks instead (`choros landing
    /// open`).
    Work {
        /// Comma-separated, or `-` to read them from stdin, as in
        /// `ergon ls --attention ..3 | ergon work -`.
        tasks: Option<WorkTasks>,
        /// Ask for what isn't given: the tasks, from `ergon ls`; the new
        /// workspace's repos; which landing set to open when several
        /// carry the tasks.
        #[arg(short, long)]
        interactive: bool,
        /// The repos to make the workspace with, as `REPO,REPO`.
        #[arg(long, value_delimiter = ',', conflicts_with = "here")]
        repos: Option<Vec<String>>,
        /// The workspace's name (default: from the task's title).
        #[arg(long, conflicts_with = "here")]
        workspace: Option<String>,
        /// Start the session in this folder, with no workspace.
        #[arg(long)]
        here: bool,
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
    /// What `choros land` runs before and after a land, to move its tasks
    ///
    /// `ergon work` sets it as the land hook of the workspace it makes
    /// (`choros hook land "ergon land-hook --task REFS"`), which binds the
    /// workspace to those tasks.
    ///
    /// A land carries the tasks its `--label`s name, or with none named,
    /// the ones the workspace is bound to. Choros is told them all as
    /// labels, the most urgent priority among them, and the PR body with
    /// them named under it.
    ///
    /// After `choros land continue`, the bound tasks stay as they are, and
    /// a subtask of one that the land carries is set to landing, unless
    /// work under it is still open. `choros land archive` takes the
    /// workspace, so it carries the bound tasks whatever it names, and sets
    /// all it carries to landing; it is refused while a subtask of one is
    /// still todo or in progress.
    #[command(verbatim_doc_comment)]
    LandHook {
        /// The tasks the workspace is bound to, as `project/slug,...`.
        #[arg(long)]
        task: TaskRefs,
        stage: Stage,
    },
    /// Print the prompt `ergon work` starts a session with
    ///
    /// Rendered from the tasks as they are now, not as they were when the
    /// session started. Changes nothing.
    Prompt { tasks: TaskRefs },
    /// Print help for ergon or a command; `ergon help planner` prints the
    /// planner's instructions.
    Help { topic: Vec<String> },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum Stage {
    Before,
    After,
}

#[derive(Subcommand)]
enum DepCommand {
    /// Record that a task depends on another
    ///
    /// Each side can be several, as `slug,slug,...`: every task then
    /// depends on every one named, or none does if one is refused.
    Add { tasks: TaskRefs, depends_on: TaskRefs },
    /// Remove a dependency
    ///
    /// Each side can be several, as `add` takes them.
    Rm { tasks: TaskRefs, depends_on: TaskRefs },
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
    /// Show or set the choros repos `ergon work` makes a workspace with
    ///
    /// The default for the project this folder is in, kept in
    /// `.ergon/config.toml`. Given `REPO,REPO`, sets it; `none` clears it.
    Repos { repos: Option<String> },
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

/// `--sort FIELD [ORDER]`, for `ls` and `kanban`.
fn parse_sort(sort: Option<Vec<String>>) -> Result<ergon::derived::Sort> {
    match sort.as_deref() {
        None => Ok(Default::default()),
        Some([field, order @ ..]) => {
            ergon::derived::Sort::parse(field, order.first().map(String::as_str))
        }
        Some([]) => unreachable!("clap requires a field"),
    }
}

/// `kanban --done`: how many days back done tasks show; `None` for all.
#[derive(Clone)]
struct DoneDays(Option<u32>);

fn parse_done_days(s: &str) -> Result<DoneDays, String> {
    if s == "all" {
        return Ok(DoneDays(None));
    }
    s.parse()
        .map(|d| DoneDays(Some(d)))
        .map_err(|_| format!("expected a number of days or `all`, not {s:?}"))
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
        return help(topic);
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
                launch(claude::work(&mut conn, &p, &root, &cwd, false)?, None)?;
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
            tasks,
            intent,
            append,
            status,
            priority,
            size,
            attention,
        } => {
            let p = project_for(&conn, &tasks.iter().collect::<Vec<_>>())?;
            if intent.is_some() && tasks.0.len() > 1 {
                bail!("--intent replaces one task's intent; to add the same text to several, use --append");
            }
            let changes = Changes {
                intent: intent.map(text_or_stdin).transpose()?,
                append: append.map(text_or_stdin).transpose()?,
                status: status.map(|s| s.0),
                priority: priority.map(|p| p.0),
                size: size.map(|s| s.0),
                attention: attention.map(|a| a.0),
            };
            for r in ops::edit_all(&mut conn, &p, &tasks.slugs(), changes)? {
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
        Command::Show {
            tasks: None,
            pr: Some(pr),
            ..
        } => {
            let Some(all) = landings::read(&cwd)? else {
                bail!("choros isn't on PATH, so there are no landing sets to look in");
            };
            let sets: Vec<_> = all.iter().filter(|l| l.pr == Some(pr)).collect();
            if sets.is_empty() {
                bail!("no landing set has PR #{pr}");
            }
            let projects = project::list(&conn, true)?
                .iter()
                .map(|p| Tasks::load(&conn, p))
                .collect::<Result<Vec<_>>>()?;
            print!("{}", view::pr_tasks(&sets, &projects));
        }
        Command::Show {
            tasks: given,
            intent,
            merged,
            ..
        } => {
            let given = given.expect("clap requires a task without --pr");
            let p = project_for(&conn, &given.iter().collect::<Vec<_>>())?;
            if !intent {
                finish_merged(&mut conn, &p, &cwd)?;
            }
            let tasks = Tasks::load(&conn, &p)?;
            let ids = given
                .iter()
                .map(|r| Ok(tasks.live(&r.slug)?.id))
                .collect::<Result<Vec<i64>>>()?;
            let derived = Derived::compute(&tasks);
            // Tasks worked in one folder share what choros says there.
            let mut read = std::collections::HashMap::new();
            for (i, &id) in ids.iter().enumerate() {
                if i > 0 {
                    println!();
                }
                if intent {
                    if ids.len() > 1 {
                        println!("{}", tasks.reference(id));
                    }
                    println!("{}", tasks.get(id).intent);
                    continue;
                }
                let dir = landings::dir_for(&tasks, id).unwrap_or(&cwd);
                let found = read
                    .entry(dir)
                    .or_insert_with(|| landings::read(dir).transpose());
                print!(
                    "{}",
                    view::show(&tasks, &derived, id, ergon::now(), found.as_ref(), merged)
                );
            }
        }
        Command::Tree { task, ls } => {
            let p = project_for(&conn, &task.iter().collect::<Vec<_>>())?;
            print!("{}", tree(&conn, &p, task.as_ref(), ls)?);
        }
        Command::Gantt {
            task,
            attention,
            ls,
        } => {
            let p = project_for(&conn, &task.iter().collect::<Vec<_>>())?;
            let tasks = Tasks::load(&conn, &p)?;
            let id = match &task {
                Some(r) => tasks.live(&r.slug)?.id,
                None => tasks.root().id,
            };
            let length = if attention {
                gantt::Length::Attention
            } else {
                gantt::Length::Size
            };
            let derived = Derived::compute(&tasks);
            let schedule = gantt::Schedule::compute(&tasks, &derived, length);
            let mark = ls.then(|| {
                ergon::derived::ls(&tasks, &derived, tasks.root().id, Default::default())
                    .into_iter()
                    .collect::<std::collections::HashSet<_>>()
            });
            print!(
                "{}",
                view::gantt(&tasks, &schedule, id, length, mark.as_ref(), columns())
            );
        }
        Command::Kanban { sort, leaves, done } => {
            let sort = parse_sort(sort)?;
            let p = project_for(&conn, &[])?;
            finish_merged(&mut conn, &p, &cwd)?;
            let tasks = Tasks::load(&conn, &p)?;
            let derived = Derived::compute(&tasks);
            let landing: Vec<i64> = tasks
                .all()
                .into_iter()
                .filter(|t| !t.is_deleted() && tasks.effective_status(t.id) == Status::Landing)
                .map(|t| t.id)
                .collect();
            let seen = landings::Seen::read(&tasks, &landing, &cwd);
            let done_at = task::done_at(&conn, &p)?;
            let done = kanban::DoneSince {
                since: done.0.map(|d| ergon::now() - i64::from(d) * 86_400),
                done_at: &done_at,
            };
            let board = kanban::Board::compute(&tasks, &derived, leaves, sort, &seen, done);
            print!("{}", view::kanban(&tasks, &derived, &board, columns()));
        }
        Command::Ls {
            task,
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
            let sort = parse_sort(sort)?;
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
            let p = project_for(&conn, &task.iter().collect::<Vec<_>>())?;
            finish_merged(&mut conn, &p, &cwd)?;
            let tasks = Tasks::load(&conn, &p)?;
            let from = match &task {
                Some(t) => tasks.live(&t.slug)?.id,
                None => tasks.root().id,
            };
            print!(
                "{}",
                view::ls(
                    &tasks,
                    &Derived::compute(&tasks),
                    from,
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
        Command::Work {
            tasks,
            interactive,
            repos,
            workspace,
            here,
        } => {
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
                None if interactive => (None, None),
                None => bail!("which tasks? name them, or pick them with -i"),
            };
            let refs: Vec<&TaskRef> = tasks.iter().flat_map(|t| t.iter()).collect();
            let p = project_for(&conn, &refs)?;
            let slugs = match &tasks {
                Some(t) => t.slugs(),
                None => match picker::run(&mut conn, &p)? {
                    Some(slugs) => slugs,
                    None => return Ok(()),
                },
            };
            let all = Tasks::load(&conn, &p)?;
            let made = match claude::unstarted(&conn, &p, &slugs)? {
                // The root's session plans; it has no work to land.
                Some(ids) if !here && !ids.contains(&all.root().id) => {
                    let repos = match repos {
                        Some(given) => ergon::workspace::Repos::Given(given),
                        None if interactive => ergon::workspace::Repos::Pick,
                        None => {
                            // This folder's project may not be the tasks'.
                            let here = project::current(&conn, None, &cwd).ok();
                            let config = match project::find_marker(&cwd) {
                                Some(marker) if here.is_some_and(|h| h.id == p.id) => {
                                    project::Config::read(&marker)?
                                }
                                _ => Default::default(),
                            };
                            ergon::workspace::Repos::Default(config.choros.repos)
                        }
                    };
                    ergon::workspace::make(&all, &ids, &cwd, workspace.as_deref(), repos)?
                }
                _ => None,
            };
            let dir = made.as_deref().unwrap_or(&cwd);
            let l = claude::work(&mut conn, &p, &slugs, dir, interactive);
            let l = match &made {
                Some(ws) => l.with_context(|| {
                    format!(
                        "the workspace {} was made and bound to the tasks, but their session \
                         didn't start; `ergon work` again from inside it starts it there",
                        ws.display()
                    )
                })?,
                None => l?,
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
            print!("{}", claude::prompt(&all, &ids, &cwd));
        }
        Command::Help { .. } => unreachable!("handled above"),
        Command::LandHook { task, stage } => {
            let told: land::Told = serde_json::from_reader(std::io::stdin())
                .context("reading the land from stdin, as choros's land hook gives it")?;
            match stage {
                Stage::Before => {
                    if let Some(reply) = land::before(&mut conn, &told, &task.0)? {
                        println!("{}", serde_json::to_string(&reply)?);
                    }
                }
                Stage::After => {
                    for line in land::after(&mut conn, &told, &task.0)? {
                        eprintln!("{line}");
                    }
                }
            }
        }
        Command::Dep(DepCommand::Add { tasks, depends_on }) => {
            let p = project_for(&conn, &tasks.iter().chain(depends_on.iter()).collect::<Vec<_>>())?;
            ops::dep_add_all(&mut conn, &p, &tasks.slugs(), &depends_on.slugs())?;
        }
        Command::Dep(DepCommand::Rm { tasks, depends_on }) => {
            let p = project_for(&conn, &tasks.iter().chain(depends_on.iter()).collect::<Vec<_>>())?;
            ops::dep_rm_all(&mut conn, &p, &tasks.slugs(), &depends_on.slugs())?;
        }
    }
    Ok(())
}

/// Marks done each landing task whose landings have all merged. Only ever
/// from landing: one session's sets can carry tasks that have more to land.
fn finish_merged(conn: &mut Connection, p: &Project, cwd: &std::path::Path) -> Result<()> {
    let tasks = Tasks::load(conn, p)?;
    for id in landings::merged(&tasks, cwd) {
        let r = tasks.reference(id);
        let done = Changes {
            status: Some(Some(Status::Done)),
            ..Default::default()
        };
        // A side effect of reading, so it never stops the read.
        match ops::edit(conn, p, &tasks.get(id).slug, done) {
            Ok(_) => eprintln!("{r} done: its landings merged"),
            Err(e) => eprintln!("couldn't mark {r} done: {e:#}"),
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

/// `ergon help`, `ergon help planner`, or a command's help by its words:
/// `ergon help dep add`.
fn help(topic: &[String]) -> Result<()> {
    let mut cmd = Cli::command();
    let words: Vec<&str> = topic.iter().map(String::as_str).collect();
    match words.as_slice() {
        [] => cmd.print_help()?,
        ["planner"] => print!("{}", claude::PLANNER_DOC),
        _ => {
            let mut sub = &cmd;
            for word in &words {
                sub = sub
                    .find_subcommand(word)
                    .with_context(|| format!("no help topic {:?}; try `ergon help`", words.join(" ")))?;
            }
            let args = ["ergon"].into_iter().chain(words.iter().copied()).chain(["--help"]);
            if let Err(e) = Cli::try_parse_from(args) {
                e.print()?;
            }
        }
    }
    Ok(())
}

/// `text`, or what stdin holds when it is `-`.
fn text_or_stdin(text: String) -> Result<String> {
    if text != "-" {
        return Ok(text);
    }
    Ok(std::io::read_to_string(std::io::stdin())?)
}

fn tree(conn: &Connection, p: &Project, task: Option<&TaskRef>, ls: bool) -> Result<String> {
    let tasks = Tasks::load(conn, p)?;
    let id = match task {
        Some(r) => tasks.live(&r.slug)?.id,
        None => tasks.root().id,
    };
    let derived = Derived::compute(&tasks);
    let mark = ls.then(|| {
        ergon::derived::ls(&tasks, &derived, tasks.root().id, Default::default())
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
        ProjectCommand::Repos { repos } => {
            let cwd = std::env::current_dir()?;
            let marker = project::find_marker(&cwd)
                .context("not in an ergon project (no .ergon found above here)")?;
            let mut config = project::Config::read(&marker)?;
            match repos {
                None if config.choros.repos.is_empty() => println!("no default repos"),
                None => println!("{}", config.choros.repos.join(",")),
                Some(given) if given == "none" && marker.is_file() => {}
                Some(given) => {
                    config.choros.repos = given
                        .split(',')
                        .map(str::trim)
                        .filter(|r| !r.is_empty() && *r != "none")
                        .map(str::to_string)
                        .collect();
                    config.write(&marker)?;
                }
            }
        }
    }
    Ok(())
}
