# ergon.nvim

Edit an ergon project's tasks in a neovim buffer, one task per row.

```
# ergon demo 1a2b3c4d
root  in_progress  ~in_progress  -   -    -  ~8/13+  -     -         Build a demo
qxpj  todo         ~todo         p1  ~p1  -  ~8/13+  -     -         - Backend
FTFE  in_progress  ~in_progress  -   ~p1  5  ~5      -     -           - Write the API
-     todo         -             p2  -    -  -       FTFE  -           - Write tests
pQyk  done         ~done         -   -    8  ~0/8    -     -         - Frontend
```

## Install

Needs neovim 0.10+ and `ergon` on `PATH`. With packer, point it at this
`nvim/` directory itself; packer's `rtp = 'nvim'` adds the directory too late
for `plugin/ergon.lua` to run, which leaves `:Ergon` undefined.

```lua
use {
  '~/.local/share/dev-setup/tools/Ergon/nvim',
  as = 'ergon.nvim',
  config = function()
    require('ergon').setup()  -- optional: { cmd = '/path/to/ergon' }
    vim.keymap.set('n', '<leader>eo', '<cmd>Ergon<cr>', { desc = 'ergon: open tasks' })
  end,
}
```

## Use

`:Ergon` opens the project named by the nearest `.ergon` file. `:Ergon <project>`
opens another one; it completes project names.

Each row is nine cells, then the title: slug, status, ~status, priority,
~priority, size, ~size, deps, ~blocked. `-` is an empty cell.

- **Stored cells** are yours to edit: status (`todo`, `in_progress`,
  `landing`, `done`, `cancelled`, or `-` to follow the parent's), priority
  (`p0`..`p4`), size (a Fibonacci number) and deps (slugs, comma-separated).
- **`~` cells** are calculated: the actual status, the effective priority, the
  size added up from subtasks, and `~blocked`. Editing one is refused.
- **Nesting** is the title's indentation under the root's. `>>` and `<<` (and
  `>`, `<` on a selection) move the title, not the row. `dd` then `p` moves a
  subtree.
- **The title** is the first line of the intent. Editing it keeps the rest.
  `<CR>` opens the whole intent in a split; `:w` there saves it.
- **New tasks** are rows with `-` for a slug; copying a row is the easy way.
- **Deleting a row** deletes the task, along with its subtasks unless you moved
  them elsewhere.
- **`:ErgonFormat`** lines the columns up again after edits. A cell edited
  wider pushes its row out of line, but never changes its nesting.

`:w` lists every change and asks before applying them. Changes are applied
together, or not at all if ergon refuses one. A failed `:w` or `:ErgonFormat`
marks what it's about in red, as a diagnostic: the cell at fault, or the rows
of the tasks a refusal names, with the cursor on the first. If the project changed since you opened the buffer, the write
is refused; `:e!` reloads it, discarding your edits.

Sibling order is kept, though it means nothing to ergon: new tasks from the
CLI go last.

The plugin is a thin layer over `ergon outline`, `ergon outline --apply
[--dry-run]`, `ergon outline --format` and `ergon show --intent`. See `ergon outline --help`.

## Test

`cargo test` runs `nvim/test.lua` headless when `nvim` is installed.
