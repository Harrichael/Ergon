-- Edit an ergon project's tasks as a buffer, one task per row.
--
-- `:Ergon [project]` opens `ergon://<project>`, the table of `ergon outline`.
-- Edit it like any text; `:w` previews the changes with
-- `ergon outline --apply --dry-run`, asks, and applies them in one
-- transaction. `<CR>` on a task opens its full intent in `ergon://<project>/<slug>`.
-- Nesting is the title's indentation, so `>>` and `<<` move the title, not
-- the row, and `:ErgonFormat` lines the columns up again after edits.

local M = {}

-- Where a failed :w or :ErgonFormat marks what it's about.
local ns = vim.api.nvim_create_namespace("ergon")

M.config = {
  -- The ergon executable.
  cmd = "ergon",
}

-- Per outline buffer: the slugs it was loaded with, so a line's leading word
-- is only taken as a slug if it is one.
local slugs = {}

function M.setup(opts)
  M.config = vim.tbl_extend("force", M.config, opts or {})
end

local function notify(msg, level)
  vim.notify("ergon: " .. msg, level or vim.log.levels.INFO)
end

-- Runs ergon, returning its stdout, or nil and the error message.
local function run(args, stdin)
  local cmd = { M.config.cmd }
  vim.list_extend(cmd, args)
  local ok, proc = pcall(vim.system, cmd, { text = true, stdin = stdin })
  if not ok then
    return nil, ("can't run %s: %s"):format(M.config.cmd, proc)
  end
  local r = proc:wait()
  if r.code ~= 0 then
    local err = vim.trim(r.stderr ~= "" and r.stderr or r.stdout)
    return nil, (err:gsub("^error: ", ""))
  end
  return r.stdout
end

-- `ergon://<project>` or `ergon://<project>/<slug>`.
local function parse_name(buf)
  local name = vim.api.nvim_buf_get_name(buf)
  local project, slug = name:match("^ergon://([^/]+)/?(.*)$")
  return project, slug ~= "" and slug or nil
end

local function slug_at(buf, lnum)
  local line = vim.api.nvim_buf_get_lines(buf, lnum - 1, lnum, false)[1] or ""
  local word = line:match("^%s*(%S+)")
  return word and slugs[buf] and slugs[buf][word] and word or nil
end

local function to_lines(text)
  local lines = vim.split(text, "\n", { plain = true })
  if lines[#lines] == "" then
    table.remove(lines)
  end
  return lines
end

-- The columns of a row, in the order ergon names them in errors.
local CELLS = {
  slug = 1, status = 2, ["~status"] = 3, priority = 4, ["~priority"] = 5,
  size = 6, ["~size"] = 7, attention = 8, ["~attention"] = 9, deps = 10,
  ["~blocked"] = 11, title = 12,
}

-- Where a row's title starts, and how many of the spaces before it may go:
-- nesting counts from the end of the ~blocked column, the last cell, which
-- is padded to 8 wide. Nil for a row that isn't a task's.
local function title_at(line)
  local pos, last = 1, nil
  for _ = 1, CELLS["~blocked"] do
    local s, e = line:find("%S+", pos)
    if not s then
      return nil
    end
    last, pos = line:sub(s, e), e + 1
  end
  local start = line:find("%S", pos)
  if not start or line:sub(start, start + 1) ~= "- " then
    return nil
  end
  local spare = (start - pos) - (2 + 8 - math.min(#last, 8))
  return start, spare
end

-- Indents (`by` > 0) or outdents the titles on rows `first`..`last`.
local function shift_titles(buf, first, last, by)
  local lines = vim.api.nvim_buf_get_lines(buf, first - 1, last, false)
  for i, line in ipairs(lines) do
    local start, spare = title_at(line)
    if start then
      local n = by > 0 and by or -math.min(-by, spare)
      if n > 0 then
        lines[i] = line:sub(1, start - 1) .. (" "):rep(n) .. line:sub(start)
      elseif n < 0 then
        lines[i] = line:sub(1, start - 1 + n) .. line:sub(start)
      end
    end
  end
  vim.api.nvim_buf_set_lines(buf, first - 1, last, false, lines)
end

-- Lines the buffer's columns up again, as `ergon outline --format` does.
function M.format(buf)
  local text = table.concat(vim.api.nvim_buf_get_lines(buf, 0, -1, false), "\n") .. "\n"
  local out, err = run({ "outline", "--format" }, text)
  if not out then
    return M.fail(buf, err)
  end
  vim.diagnostic.reset(ns, buf)
  local win = vim.fn.bufwinid(buf)
  local cursor = win ~= -1 and vim.api.nvim_win_get_cursor(win)
  vim.api.nvim_buf_set_lines(buf, 0, -1, false, to_lines(out))
  if cursor then
    vim.api.nvim_win_set_cursor(win, { math.min(cursor[1], vim.api.nvim_buf_line_count(buf)), 0 })
  end
end

-- Replaces the buffer's text without leaving an undo step, so `u` can't
-- bring back edits that were already applied.
local function set_text(buf, lines)
  local levels = vim.bo[buf].undolevels
  vim.bo[buf].undolevels = -1
  vim.api.nvim_buf_set_lines(buf, 0, -1, false, lines)
  vim.bo[buf].undolevels = levels
  vim.bo[buf].modified = false
end

local function load_outline(buf)
  local project = parse_name(buf)
  local out, err = run({ "--project", project, "outline", "--json" })
  if not out then
    return notify(err, vim.log.levels.ERROR)
  end
  local data = vim.json.decode(out)

  local win = vim.fn.bufwinid(buf)
  local cursor = win ~= -1 and vim.api.nvim_win_get_cursor(win) or { 1, 0 }
  local cursor_slug = slug_at(buf, cursor[1])

  slugs[buf] = {}
  for _, t in ipairs(data.tasks) do
    slugs[buf][t.slug] = true
  end
  local lines = to_lines(data.text)
  set_text(buf, lines)
  vim.diagnostic.reset(ns, buf)

  if win ~= -1 then
    local row = math.min(cursor[1], #lines)
    for _, t in ipairs(data.tasks) do
      if t.slug == cursor_slug then
        row = t.line
      end
    end
    vim.api.nvim_win_set_cursor(win, { row, cursor[2] })
  end
end

local function setup_outline(buf)
  local bo = vim.bo[buf]
  bo.buftype = "acwrite"
  bo.swapfile = false
  bo.expandtab = true
  bo.shiftwidth = 2
  bo.softtabstop = 2
  bo.autoindent = true
  bo.filetype = "ergon"
  local function shift(by)
    return function()
      local first = vim.api.nvim_win_get_cursor(0)[1]
      shift_titles(buf, first, first + vim.v.count1 - 1, by)
    end
  end
  local function shift_visual(by)
    return function()
      local a, b = vim.fn.line("v"), vim.fn.line(".")
      shift_titles(buf, math.min(a, b), math.max(a, b), by * vim.v.count1)
      vim.api.nvim_feedkeys(vim.keycode("<Esc>"), "nx", false)
    end
  end
  local opts = function(desc)
    return { buffer = buf, desc = "ergon: " .. desc }
  end
  vim.keymap.set("n", ">>", shift(2), opts("nest the title one level deeper"))
  vim.keymap.set("n", "<<", shift(-2), opts("nest the title one level shallower"))
  vim.keymap.set("x", ">", shift_visual(2), opts("nest the titles one level deeper"))
  vim.keymap.set("x", "<", shift_visual(-2), opts("nest the titles one level shallower"))
  vim.api.nvim_buf_create_user_command(buf, "ErgonFormat", function()
    M.format(buf)
  end, { desc = "Line the outline's columns up again" })
  vim.keymap.set("n", "<CR>", function()
    local slug = slug_at(buf, vim.api.nvim_win_get_cursor(0)[1])
    if not slug then
      return notify("no saved task on this line")
    end
    M.open_intent(parse_name(buf), slug)
  end, { buffer = buf, desc = "ergon: open the task's intent" })
end

local function load_intent(buf)
  local project, slug = parse_name(buf)
  local out, err = run({ "--project", project, "show", slug, "--intent" })
  if not out then
    return notify(err, vim.log.levels.ERROR)
  end
  local bo = vim.bo[buf]
  bo.buftype = "acwrite"
  bo.swapfile = false
  bo.filetype = "markdown"
  set_text(buf, to_lines(out))
end

-- The 0-based column span of cell `n` (CELLS.title: the title) on `line`.
local function cell_span(line, n)
  local pos = 1
  for i = 1, CELLS["~blocked"] do
    local s, e = line:find("%S+", pos)
    if not s then
      return 0, #line
    end
    if i == n then
      return s - 1, e
    end
    pos = e + 1
  end
  local s = line:find("%S", pos) or #line + 1
  return s - 1, #line
end

-- Where an error points: `line N, <cell>:` is that cell, `line N:` the row,
-- and a refusal names tasks (`demo/FTFE`), whose rows it marks.
local function diagnostics(buf, err)
  local lines = vim.api.nvim_buf_get_lines(buf, 0, -1, false)
  local mark = function(lnum, col, end_col)
    return {
      lnum = lnum - 1, col = col, end_col = end_col,
      severity = vim.diagnostic.severity.ERROR, message = err, source = "ergon",
    }
  end
  local lnum, cell = err:match("^line (%d+), ([^:]+):")
  lnum = tonumber(lnum or err:match("^line (%d+):"))
  if lnum and lines[lnum] then
    if CELLS[cell] then
      return { mark(lnum, cell_span(lines[lnum], CELLS[cell])) }
    end
    return { mark(lnum, 0, #lines[lnum]) }
  end
  local out = {}
  for slug in err:gmatch("[%w_-]+/(%w+)") do
    for i, line in ipairs(lines) do
      if line:match("^(%S+)") == slug then
        table.insert(out, mark(i, 0, #line))
      end
    end
  end
  return out
end

-- Reports a failed command: marks the cells or rows it's about in red, and
-- moves the cursor to the first.
function M.fail(buf, err)
  local marks = diagnostics(buf, err)
  vim.diagnostic.set(ns, buf, marks)
  local win = vim.fn.bufwinid(buf)
  if marks[1] and win ~= -1 then
    vim.api.nvim_win_set_cursor(win, { marks[1].lnum + 1, marks[1].col })
  end
  if err:find("changed since this outline was printed", 1, true) then
    err = err .. "\nTo reload, discarding your edits: :e!"
  end
  notify(err, vim.log.levels.ERROR)
end

local function write_outline(buf)
  local project = parse_name(buf)
  local text = table.concat(vim.api.nvim_buf_get_lines(buf, 0, -1, false), "\n") .. "\n"
  local args = { "--project", project, "outline", "--apply" }

  local plan, err = run(vim.list_extend(vim.deepcopy(args), { "--dry-run" }), text)
  if not plan then
    return M.fail(buf, err)
  end
  plan = vim.trim(plan)
  if plan == "" then
    load_outline(buf)
    return notify("no changes")
  end
  local prompt = ("Apply these changes to %s?\n\n%s\n"):format(project, plan)
  local choice = vim.fn.confirm(prompt, "&Apply\n&Cancel", 2)
  -- Clear the prompt, so the result fits without a hit-enter prompt.
  vim.cmd.redraw()
  if choice ~= 1 then
    return
  end

  local out, apply_err = run(args, text)
  if not out then
    return M.fail(buf, apply_err)
  end
  load_outline(buf)
  local n = #to_lines(vim.trim(out))
  notify(("applied %d change%s"):format(n, n == 1 and "" or "s"))
end

local function write_intent(buf)
  local project, slug = parse_name(buf)
  local text = table.concat(vim.api.nvim_buf_get_lines(buf, 0, -1, false), "\n")
  local _, err = run({ "--project", project, "edit", slug, "--intent", text })
  if err then
    return notify(err, vim.log.levels.ERROR)
  end
  vim.bo[buf].modified = false
  -- The title may have changed; refresh an unedited outline to match.
  local outline = vim.fn.bufnr("ergon://" .. project)
  if outline ~= -1 and vim.api.nvim_buf_is_loaded(outline) and not vim.bo[outline].modified then
    load_outline(outline)
  end
end

-- BufReadCmd for ergon:// buffers.
function M.read(buf)
  local _, slug = parse_name(buf)
  if slug then
    load_intent(buf)
  else
    setup_outline(buf)
    load_outline(buf)
  end
end

-- BufWriteCmd for ergon:// buffers.
function M.write(buf)
  local _, slug = parse_name(buf)
  if slug then
    write_intent(buf)
  else
    write_outline(buf)
  end
end

-- Opens a project's outline: `project`, or else the one the current
-- directory's .ergon file names.
function M.open(project)
  if not project or project == "" then
    local out, err = run({ "outline", "--json" })
    if not out then
      return notify(err, vim.log.levels.ERROR)
    end
    project = vim.json.decode(out).project
  end
  local name = "ergon://" .. project
  local buf = vim.fn.bufnr(name)
  vim.cmd.edit(vim.fn.fnameescape(name))
  -- `:edit` doesn't reread a loaded buffer; refresh it unless it has edits.
  if buf ~= -1 and not vim.bo[buf].modified then
    load_outline(buf)
  end
end

function M.open_intent(project, slug)
  vim.cmd.split(vim.fn.fnameescape(("ergon://%s/%s"):format(project, slug)))
end

-- Project names, for completing `:Ergon`.
function M.projects()
  local out = run({ "project", "ls" }) or ""
  local names = {}
  for _, line in ipairs(to_lines(out)) do
    table.insert(names, line:match("^(%S+)"))
  end
  return names
end

return M
