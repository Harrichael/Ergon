# Ergon data model

Ergon is a minimal task tracker for dispatching work to Claude Code sessions.
All state lives in one SQLite database. This document is the full
specification of the model, its rules, and the CLI. The planner session gets
a shorter, role-specific version instead: [planner.md](planner.md).

## Concepts

- **Project**: a named container of tasks. Every project has exactly one **root
  task**, created with the project. Task slugs are unique per project, and
  nothing crosses projects: no parenting, no dependencies, no moves.
- **Task**: a piece of work described by its **intent**: free-form text of any
  length. The **title** is the first line of the intent, capped at 100
  characters. Tasks form a strict tree under the root. A child is a subtask;
  there is no separate notion of a "bullet". Children have no order.
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
  claude_session TEXT,
  claude_cwd     TEXT,
  created_at     INTEGER NOT NULL,
  updated_at     INTEGER NOT NULL,
  deleted_at     INTEGER,                        -- soft delete
  UNIQUE (project_id, slug),
  CHECK ((claude_session IS NULL) = (claude_cwd IS NULL))
);

CREATE UNIQUE INDEX one_root_per_project ON tasks(project_id) WHERE parent_id IS NULL;

CREATE TABLE task_deps (
  task_id    INTEGER NOT NULL REFERENCES tasks(id),
  depends_on INTEGER NOT NULL REFERENCES tasks(id),
  PRIMARY KEY (task_id, depends_on)
);
```

The database is at `$ERGON_DB`, or `$XDG_DATA_HOME/ergon/ergon.db` (normally
`~/.local/share/ergon/ergon.db`). It runs in WAL mode with a busy timeout, so
concurrent `ergon` calls are safe. The schema version is tracked with
`PRAGMA user_version`.

## Status

| Status        | Meaning                                                              |
|---------------|----------------------------------------------------------------------|
| `todo`        | Not started.                                                         |
| `in_progress` | Being worked on.                                                     |
| `landing`     | Work is finished but not yet landed, e.g. a PR in review or merged but not released. |
| `done`        | Finished.                                                            |
| `cancelled`   | Won't be done.                                                       |

**Open** means `todo` or `in_progress`. **Closed** means `done` or `cancelled`.

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
   children have an upward priority, use their geometric mean. The mean is
   taken over ranks, where `pN` has rank `N+1`, and the result is rounded to the
   nearest rank, with exact halves rounding towards more urgent. Otherwise the
   task has no upward priority.
2. **Downward.** A task's base priority is its upward priority if it has one,
   otherwise its parent's base priority. If nothing up to the root has one, the
   task has no priority.
3. **Dependency boost.** The effective priority is the most urgent of the
   task's own base priority and the base priorities of every task that depends
   on it, directly or transitively, and isn't closed. A `p3` task that blocks a
   `p1` task is effectively `p1`. The boost doesn't feed back into passes 1 and 2.

Effective priority is shown in a different colour from stored priority. Tasks
with no priority display as `p-` and sort after `p4`.

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

Results are sorted by effective priority, then oldest first. Each row shows:
slug, effective priority, effective status, title, and a dimmed `(in: <parent
title>)`.

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
   dependency graph has no cycles. The root has no dependencies in either
   direction.
6. **Root.** The root can't be deleted, moved, cancelled, or given a NULL
   status.
7. **Sessions are permanent.** Once a task has a Claude session, it can't be
   changed or removed.

A task's updated time changes when the task's own intent, status, priority, or session
changes. Changes to its children or dependencies don't count.

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
it was started in.

`ergon work <ref>`:

- **Refused** if the task's effective status is closed. It works on blocked
  tasks, with a warning.
- **First time:** generates a session UUID and records it with the current
  directory. If the effective status is `todo`, sets it to `in_progress`. Then
  launches `claude --session-id <uuid> "<prompt>"`.
- **Afterwards:** changes to the recorded directory (an error if it no longer
  exists) and launches `claude --resume <uuid>`.

There are two prompts:

- **Planner** (the root task, launched by `ergon init`):
  [planner.md](planner.md), the project name, and the root intent. The planner is the only session that's
  expected to run `ergon`, to create, edit, and organise the project's tasks.
- **Work** (every other task):

  ```
  Greater context that your work sits within:
  <intents of the ancestors, root first>

  Task to complete here (<project>/<slug>):
  <the task's intent>
  <non-deleted, non-cancelled descendants' intents, indented by depth>

  Ask the user how they'd like to start.
  ```

  Work sessions aren't told about ergon. The user (or the planner) updates
  their status.

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

ergon add <intent> [--parent <ref>] [--status <status>] [--priority <pN>]
    Create a task (under the root by default) and print its slug.
ergon edit <ref> [--intent <text>] [--status <status>|none] [--priority <pN>|none]
ergon mv <ref> <new-parent-ref>
ergon rm <ref> --subtasks <N> --dependents <M>
ergon restore <ref>

ergon show <ref>
    Full intent; stored and effective status and priority; whether it's
    blocked; dependencies and dependents; session; timestamps; the
    children's titles as a bullet list; and the subtask and dependent
    counts that `rm` would require.
ergon tree [<ref>]
    Indented titles with slug, effective status, and effective priority.
ergon ls

ergon dep add <ref> <depends-on-ref>
ergon dep rm <ref> <depends-on-ref>

ergon work <ref>
ergon help planner
    Print planner.md, the planner's instructions.
```

Timestamps are stored as unix seconds and shown as local time with a relative
age, e.g. `2026-10-20 14:03 (3h ago)`.
