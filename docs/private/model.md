# Ergon data model

Ergon is a minimal task tracker for dispatching work to Claude Code sessions.
All state lives in one SQLite database. This document is the full
specification of the model, its rules, and the CLI. The planner session gets
a shorter, role-specific version instead: [planner.md](../prompts/planner.md).

## Concepts

- **Project**: a named container of tasks. Every project has exactly one **root
  task**, created with the project. Task slugs are unique per project, and
  nothing crosses projects: no parenting, no dependencies, no moves.
- **Task**: a piece of work described by its **intent**: free-form text of any
  length. The **title** is the first line of the intent, capped at 100
  characters. Tasks form a strict tree under the root. A child is a subtask;
  there is no separate notion of a "bullet". Children have no order in
  meaning, but keep a stable display order: new and moved tasks go last, and
  the outline can rearrange them.
- **Dependency**: "task A depends on task B". Dependencies are informational:
  they never prevent a status change. They decide what is **blocked**, which
  decides what `ergon ls` offers as work to do next.

## Identifiers

- **Project name**: one or more of `A-Z a-z 0-9 _ -`. Renamable: `.ergon`
  files hold the project's UUID, not its name, so a rename breaks nothing.
- **Task slug**: 4 random characters from this 41-character alphabet, which
  omits look-alikes (`0 O o 1 l I 5 S s 2 Z z 8 B`, and the capitals
  `C K P U V W X`):

  ```
  3 4 6 7 9
  A D E F G H J L M N Q R T Y
  a b c d e f g h i j k m n p q r t u v w x y
  ```

  Slugs are case-sensitive and never reused, not even after deletion. The root
  task's slug is always `root` (which can't collide, since `o` isn't in the
  alphabet).
- **Task reference**: `E6dn` (resolved in the current project) or `ergon/E6dn`
  (project name, then slug).

**Current project**: the `--project <name>` flag if given. Otherwise ergon
walks up from the working directory to the nearest `.ergon` file, which
contains the project UUID on a single line. Otherwise it's an error. A
`project/slug` reference names its own project explicitly.

## Schema

```sql
CREATE TABLE projects (
  id         INTEGER PRIMARY KEY,
  uuid       TEXT    NOT NULL UNIQUE,    -- what .ergon contains
  name       TEXT    NOT NULL UNIQUE,
  created_at INTEGER NOT NULL,           -- unix seconds
  retired_at INTEGER                     -- NULL = active
);

CREATE TABLE tasks (
  id             INTEGER PRIMARY KEY,
  project_id     INTEGER NOT NULL REFERENCES projects(id),
  slug           TEXT    NOT NULL,
  parent_id      INTEGER REFERENCES tasks(id),   -- NULL only for the root
  intent         TEXT    NOT NULL,
  status         TEXT CHECK (status IN ('todo','in_progress','landing','done','cancelled')),
  priority       INTEGER CHECK (priority BETWEEN 0 AND 4),
  size           INTEGER CHECK (size > 0),               -- a Fibonacci number
  attention      INTEGER CHECK (attention BETWEEN 1 AND 10),
  claude_session TEXT,
  claude_cwd     TEXT,
  created_at     INTEGER NOT NULL,
  updated_at     INTEGER NOT NULL,
  deleted_at     INTEGER,                        -- soft delete
  position       INTEGER NOT NULL DEFAULT 0,     -- display order among siblings
  UNIQUE (project_id, slug),
  CHECK ((claude_session IS NULL) = (claude_cwd IS NULL))
);

CREATE UNIQUE INDEX one_root_per_project ON tasks(project_id) WHERE parent_id IS NULL;

CREATE TABLE task_deps (
  task_id    INTEGER NOT NULL REFERENCES tasks(id),
  depends_on INTEGER NOT NULL REFERENCES tasks(id),
  PRIMARY KEY (task_id, depends_on)
);

CREATE TABLE task_history (                      -- written by triggers on tasks
  id         INTEGER PRIMARY KEY,
  task_id    INTEGER NOT NULL REFERENCES tasks(id),
  at         INTEGER NOT NULL,                   -- unix seconds, from ergon_now()
  parent_id  INTEGER,
  status     TEXT,
  priority   INTEGER,
  size       INTEGER,
  attention  INTEGER,
  deleted_at INTEGER
);
```

The database is at `$ERGON_DB`, or `$XDG_DATA_HOME/ergon/ergon.db` (normally
`~/.local/share/ergon/ergon.db`). It runs in WAL mode with a busy timeout, so
concurrent `ergon` calls are safe. The schema version is tracked with
`PRAGMA user_version`.

**Upgrades.** Any `ergon` command upgrades an older database when it opens
it. It takes the write lock, so of several processes only one upgrades, then
copies the database to `<db>.v<N>.bak` (the newest three are kept) and runs
every pending step in one transaction. An ergon older than the database
refuses to touch it. Released steps never change: a test pins their hashes,
and another upgrades data saved from each released schema
(`tests/fixtures/`).

**Export.** `ergon export` writes projects as JSON in a format of its own,
versioned apart from the schema, and `ergon import` reads it into any
database. Projects keep their UUIDs, so `.ergon` files keep working. Use it
for backups, to move between machines, or to recover if an upgrade ever
goes wrong.

## Status

| Status        | Meaning                                                              |
|---------------|----------------------------------------------------------------------|
| `todo`        | Not started.                                                         |
| `in_progress` | Being worked on.                                                     |
| `landing`     | Work is finished but not yet merged, e.g. a PR in review or waiting in a landing queue. |
| `done`        | Finished.                                                            |
| `cancelled`   | Won't be done.                                                       |

**Open** means `todo` or `in_progress`. **Closed** means `done` or `cancelled`.

`ergon ls` and `ergon show` turn a `landing` task `done` once choros reports
every landing set that carries it, or a task under it, as merged, and at
least one does; the `landing` tasks under it go `done` with it. Nothing else
moves a status on its own: a session's sets can carry tasks with more to
land, so only a task already in `landing` is said to be finished. `tree`
and the pickers don't check, so they can show a task as `landing` until the
next `ls` or `show`.

A task's stored status may be NULL. Its **effective status** is its own status
if set, otherwise its nearest ancestor's. The root's status can't be NULL; it
starts as `in_progress`. Top-level tasks (direct children of the root) always
have a stored status, so they don't inherit `in_progress`: a new one gets
`todo` unless another status is given, a task moved to the top level without a
status gets `todo`, and unsetting a top-level task's status is refused.

There are no transition rules: any status may change to any other, subject to
the invariants below.

## Priority

Priorities are `p0` (most urgent) to `p4`. A task's stored priority may be
NULL; its **effective priority** is computed in three passes. Each pass reads
only the results of the previous one, so the outcome doesn't depend on
evaluation order. Deleted tasks are ignored throughout.

1. **Upward.** If the task has a stored priority, use it. Otherwise, if any
   children that aren't closed have an upward priority, use their geometric
   mean. (The root never infers one this way: it stands for the whole
   project, and averaging it would push a project-wide mean onto every
   unprioritized top-level task.) The mean is
   taken over ranks, where `pN` has rank `N+1`, and the result is rounded to the
   nearest rank, with exact halves rounding towards more urgent. Otherwise the
   task has no upward priority.
2. **Downward.** A task's base priority is its upward priority if it has one,
   otherwise its parent's base priority. If nothing up to the root has one, the
   task has no priority.
3. **Dependency boost.** The effective priority is the most urgent of the
   task's own base priority and the base priorities of every task that isn't
   closed and waits on it: tasks that depend on it, their subtasks (which
   inherit the block), and so on transitively. A `p3` task that blocks a `p1`
   task is effectively `p1`. The boost doesn't feed back into passes 1 and 2.

Effective priority is shown bold when it equals the stored priority, and cyan
when it's inferred or boosted. Tasks with no priority display as a dimmed `p-`
and sort after `p4`.

## Size

A size is a unitless Fibonacci number (1, 2, 3, 5, 8, 13, ...) stored on a
task, or NULL. A sized task is one **unit of work**; its subtasks are steps
of it. So no sized task has a sized ancestor (invariant 8), and a sized
task's size is its own: nothing under it counts.

Every other task with sized work under it has a computed size, summed over
its children that aren't cancelled:

- **total**: the sum of their totals. A sized task's total is its size.
- **left**: the sum of what's left. A sized task has its size left while its
  effective status is open, and nothing once it's landing or done.
- **partial**: some child has no size and nothing sized under it, so the
  sums are a floor.

A task with no size and nothing sized under it, and every step of a sized
task, has no size.

Views show sizes only once some task in the project has one: `5` for a stored
size, in bold, and for a computed one, in cyan, the total when nothing is
done (`13`) or `left/total` (`8/13`), with `+` when partial. `show` spells
it out, or names the sized task a step belongs to.

## Attention

Attention is how much a task needs the user, from 1 to 10, stored on a task
or NULL: 1 is "just execute", 5 is "steering recommended", and 10 is "human
decision". It blends every reason for involving the user into one number, and
the gradient lets the user see where agents' choices held up and where they
didn't. It's informative only: nothing sorts or filters by it, and work
sessions don't see it.

A task's effective attention is its own, else its parent's effective own
(passed down), raised to the highest effective attention among its children
that aren't closed. A task with no attention of its own or from above is
**unknown** if it has no children that aren't closed, or if any of them is
unknown. So unknown spreads up through unrated tasks and stops at a rated
one. Closed tasks keep their own attention but don't raise their parent's.

Views show attention only once some task in the project has one: `a7`, bold
when stored and cyan when inherited or raised, and a dimmed `a?` when
unknown. `show` says which.

## Blocked

A task is **blocked** if any task it depends on has an effective status that
isn't closed, or if any of its ancestors is blocked. Deleted tasks count as
satisfied. Blocking is inherited downward, never upward.

## `ergon ls`

`ergon ls` lists the coarsest unblocked open work. It starts from the root's
children and visits each task like this:

- Skip the task (and its subtree) if it's deleted, isn't open, or is blocked.
- If any open descendant of the task is blocked, visit its children instead.
- Otherwise, list the task and don't descend further.

For example: P has children A and B. If both are unblocked, `ls` shows P. If B
is blocked, `ls` shows A.

Results are sorted by effective priority, then oldest first. `--sort FIELD
[asc|desc]` sorts by `priority`, `attention`, `size` (what's left), `created`
or `updated` instead, ascending by default; tasks without a value go last
either way, and ties fall back to the default order. Each row shows:
slug, effective priority, effective status, size and attention (once any
task has one), title, and, below the top level, a dimmed `(in: <parent title>)`.

## Invariants

ergon refuses any change (add, edit, move, restore) that would break these.

1. **Done needs closed children.** A task whose effective status is `done` has
   only closed descendants. This means you can't add an open subtask under a
   `done` task, or reopen one.
2. **Landing needs landed children.** A task whose effective status is
   `landing` has only `landing` or closed descendants.
3. **Cancel cascades.** Setting a task to `cancelled` also sets every
   descendant with a stored `todo`, `in_progress` or `landing` status to
   `cancelled`. `done` descendants stay `done`.
4. **Strict tree.** Every task except the root has a parent in the same
   project. Moving a task under itself or one of its descendants is refused.
5. **Dependencies.** Both tasks are in the same project. Neither is an
   ancestor of the other, and this is checked again when a task is moved. The
   root has no dependencies in either direction. No tasks may wait on each
   other in a cycle, where a task waits on its dependencies, its subtasks
   inherit those waits, and a task waits on its subtasks (it can't be done
   before them). For example, if `a` depends on `b` and `b` depends on `a1`, a
   subtask of `a`, then `a1` waits on `b` through `a`: a cycle.
6. **Root.** The root can't be deleted, moved, cancelled, or given a NULL
   status.
7. **Sessions are permanent.** Once a task has a Claude session, it can't be
   changed or removed.
8. **Sizes don't nest.** No sized task has a sized ancestor among live tasks.
   The refusal names both tasks and the command to unsize either.

A task's updated time changes when the task's own intent, status, priority, size, attention, or session
changes. Changes to its children or dependencies don't count.

## History

Every task's planning state is kept over time, for charts like a burndown:
`task_history` gets a row with the task's parent, status, priority, size,
attention and deletion when the task is added and whenever any of those
changes, dated by ergon's clock. Triggers on `tasks` write it, so every
command that changes a task is covered, `outline --apply` and `import`
included; intent and session changes aren't recorded. Nothing reads it yet.

History began with schema 5: upgrading gave each existing task one row, its
state then, dated to its last change. Exports carry it (format 4); importing
an older export starts each task's history the same way.

## Deletion

`ergon rm <ref> --subtasks N --dependents M` soft-deletes the task and every
non-deleted descendant, all with the same deletion time.

- `N` must equal the number of non-deleted descendants.
- `M` must equal the number of distinct non-deleted tasks, outside the deleted
  subtree, that depend on any task inside it.
- If either count is wrong, nothing is deleted and the real counts are printed.

Deleted tasks are hidden everywhere, and dependencies on them count as
satisfied. Their slugs stay reserved.

`ergon restore <ref>` restores the task, its deleted ancestors, and the
descendants that were deleted in the same `rm`. Restoring is subject to the invariants.

## Projects

Retiring a project only marks it as retired. It's hidden from `ergon project ls`
(unless `--all` is given), but every command still works on it, and retiring
changes nothing about its tasks.

## Claude sessions

Each task may hold one Claude Code session: its session UUID and the directory
it was started in. Several tasks may hold the same session, when they were
taken on together.

`ergon work <ref>[,<ref>...]`, or `ergon work -i` to pick the tasks from the
`ls` list (Space checks, Enter starts the checked tasks or else the
highlighted one; a refusal stays in the picker), or `ergon work -` to read
them from stdin, so `ergon ls --attention ..3 | ergon work -` composes. From
stdin it takes the first word of each line, split on commas, so `ls` rows
read as their tasks; the session's stdin is then the terminal ergon's
stdout or stderr is on (not `/dev/tty`, which macOS's kqueue refuses),
refused if there is none, and an empty stdin starts nothing:

- **Refused** if any task's effective status is closed, if the root is
  given with other tasks (planning isn't work), or if a task is given with
  one of its own subtasks (it already includes them). It works on blocked
  tasks, with a warning.
- **First time** (no task has a session): generates one session UUID and
  records it, with the current directory, on every task. Each one whose
  effective status is `todo` is set to `in_progress`. Then launches
  `claude --session-id <uuid> "<prompt>"`.
- **Afterwards** (every task has the same session): changes to the recorded
  directory (an error if it no longer exists) and launches
  `claude --resume <uuid>`. If Claude Code never saved that conversation
  (no `projects/*/<uuid>.jsonl` under `$CLAUDE_CONFIG_DIR` or `~/.claude`;
  claude crashed or quit at once), it launches the first-time command
  again instead, with a warning.
- **Refused** if the tasks have different sessions, or only some have one:
  that session's conversation is already under way, and adding work to it is
  a change of plan for the user to make.

ergon runs the `claude` found on `PATH`, or `$ERGON_CLAUDE` if set, and checks
it exists before recording a session.

There are two prompts:

- **Planner** (the root task, launched by `ergon init`):
  [planner.md](../prompts/planner.md), the project name, and the root intent. The planner is the only session that's
  expected to run `ergon`, to create, edit, and organise the project's tasks.
- **Work** (every other task), from
  [work.md.tera](../prompts/work.md.tera):

  ```
  ## Context
  These are the ancestor tasks which may help frame your work:
  <the ancestors, root first: titles, with bodies indented below>

  ## Your Task (<project>/<slug>)
  <the task's intent>
  <non-deleted, non-cancelled descendants' intents, indented by depth>

  ## Ergon
  <how to move the tasks to landing, and to name them in PRs>

  ## Choros
  <what `choros here` says of the session's folder, and to pass each
  task to `choros land` as --label>

  ## Start
  Ask the user how they'd like to start.
  ```

  For several tasks, the ancestors of all of them are merged into one
  outline in tree order, and each task follows in tree order as
  `## Your Task N of M (...)`, with its own subtasks.

  The Choros section appears only when `choros here` succeeds in the
  session's folder: the one its tasks recorded, else the current one.

## CLI

All commands accept `--project <name>`.

```
ergon init <name> <root-intent> [--no-launch]
    Create the project and its root task, write .ergon in the current
    directory, and launch the planner session.

ergon project ls [--all]
ergon project rename <old> <new>
ergon project retire <name>
ergon project unretire <name>

ergon add <intent> [--parent <ref>] [--status <status>] [--priority <pN>] [--size <N>] [--attention <1-10>]
    Create a task (under the root by default) and print its slug.
ergon edit <ref> [--intent <text>] [--status <status>|none] [--priority <pN>|none] [--size <N>|none] [--attention <1-10>|none]
ergon mv <ref> <new-parent-ref>
ergon rm <ref> --subtasks <N> --dependents <M>
ergon restore <ref>

ergon show <ref> [--intent]
    With --intent, just the intent. Otherwise: full intent; stored and effective status and priority; size; attention; whether it's
    blocked; dependencies and dependents; session; timestamps; the
    children's titles as a bullet list; and the exact `ergon rm` command,
    with the subtask and dependent counts it requires.
ergon tree [<ref>] [--ls]
    Indented titles with slug, effective status, effective priority, size, and
    attention. --ls marks the tasks `ergon ls` lists with a ▸ gutter and a
    bold title.
ergon ls [--sort FIELD [asc|desc]] [--subtasks] [--groups | --ancestors]
         [--priority RANGE] [--attention RANGE] [--size RANGE]
         [--status STATUS,...] [--intent]
    --subtasks follows each row with its subtasks, indented, in tree order.
    --groups nests rows under the ancestors they share: an ancestor is shown
    (slug and bold title) only where listed tasks reach it through two or
    more of its children, and groups follow the sort order of their first
    task. --ancestors nests the same way under every ancestor, the root
    included.
    --priority, --attention and --size RANGE keep only the rows whose value
    (as shown: effective priority and attention, size left) is in RANGE:
    `a..b`, `..b`, `a..` or `a`, both ends included, or `none` for no value.
    --status todo|in_progress (comma-separated) keeps tasks whose effective
    status is one of them; closed statuses and landing are refused, since ls
    never lists them. Flags combine; the filter runs before nesting.
    --intent follows each task row with the rest of its intent behind a dim
    `│` gutter; `ergon work -` skips gutter lines, so it still pipes. Bodies
    wrap between words to `COLUMNS`, else the terminal's width, with list
    items' continuation lines under their text.
ergon outline [--json]
ergon outline --apply [--dry-run]
ergon outline --format
    The project as an editable table, one row per task; applying an
    edited copy back in one transaction; and lining an edited copy up
    again. See `ergon outline --help`.

ergon dep add <ref> <depends-on-ref>
ergon dep rm <ref> <depends-on-ref>

ergon export
ergon import [<file>]

ergon work <ref>[,<ref>...]
ergon work -i
ergon work -
    Start or resume the session for one task or several taken on together;
    with -i, pick them from the `ls` list; with -, read them from stdin, as
    piped from `ergon ls`.
ergon prompt <ref>[,<ref>...]
    Print the prompt `ergon work` starts the session with, rendered
    from the tasks as they are now. A session whose context was compacted
    can get its instructions back this way.
ergon help planner
    Print planner.md, the planner's instructions.
```

Timestamps are stored as unix seconds and shown as local time with a relative
age, e.g. `2026-10-20 14:03 (3h ago)`.
