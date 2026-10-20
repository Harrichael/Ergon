# ergon.nvim

Edit an ergon project's tasks in a neovim buffer, one task per line.

```
# ergon demo 1a2b3c4d
qxpj [ ] Backend !p1
  FTFE [~] Write the API                  p1
    nHcx Document the API !p3
  [ ] Write tests deps:FTFE
pQyk [x] Frontend
```

## Install

Needs neovim 0.10+ and `ergon` on `PATH`. With packer:

```lua
use {
  '~/.local/share/dev-setup/tools/Ergon',
  rtp = 'nvim',
  config = function()
    require('ergon').setup()  -- optional: { cmd = '/path/to/ergon' }
    vim.keymap.set('n', '<leader>eo', '<cmd>Ergon<cr>', { desc = 'ergon: open tasks' })
  end,
}
```

## Use

`:Ergon` opens the project named by the nearest `.ergon` file. `:Ergon <project>`
opens another one; it completes project names.

Each line is `<slug> [<box>] <title> [!p<N>] [deps:<slug>,<slug>]`:

- **Nesting** is indentation (`>>`, `<<`), so `dd` then `p` moves a subtree.
- **The box** is the status: `[ ]` todo, `[~]` in_progress, `[>]` landing,
  `[x]` done, `[-]` cancelled. No box means the status follows the parent's.
- **The title** is the first line of the intent. Editing it keeps the rest.
  `<CR>` opens the whole intent in a split; `:w` there saves it.
- **New tasks** are lines without a slug, like `[ ] Write tests`.
- **Deleting a line** deletes the task, along with its subtasks unless you moved
  them elsewhere.
- **Virtual text** at the end of a line is ergon's inference, not part of the
  text: an inferred or boosted priority, and `blocked`.

`:w` lists every change and asks before applying them. Changes are applied
together, or not at all if ergon refuses one, and an error naming a line moves
the cursor there. If the project changed since you opened the buffer, the write
is refused; `:e!` reloads it, discarding your edits.

Sibling order is kept, though it means nothing to ergon: new tasks from the
CLI go last.

The plugin is a thin layer over `ergon outline`, `ergon outline --apply
[--dry-run]` and `ergon show --intent`. See `ergon outline --help`.

## Test

`cargo test` runs `nvim/test.lua` headless when `nvim` is installed.
