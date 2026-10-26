-- End-to-end test of the plugin against a scratch database. Run by
-- `cargo test` (tests/nvim.rs), or directly:
--
--   ERGON_BIN=target/debug/ergon nvim --headless -u NONE -l nvim/test.lua

local bin = assert(os.getenv("ERGON_BIN"), "set ERGON_BIN to the ergon binary")
local here = vim.fn.fnamemodify(debug.getinfo(1, "S").source:sub(2), ":p:h")
local tmp = vim.fn.tempname()
vim.fn.mkdir(tmp .. "/proj", "p")

vim.env.ERGON_DB = tmp .. "/ergon.db"
vim.env.ERGON_SEED = "1"
vim.env.ERGON_NOW = "1760000000"
vim.opt.rtp:prepend(here)
vim.cmd("syntax on")
vim.cmd.runtime("plugin/ergon.lua")
require("ergon").setup({ cmd = bin })
vim.cmd.cd(tmp .. "/proj")

local function ergon(...)
  local r = vim.system({ bin, ... }, { text = true }):wait()
  assert(r.code == 0, r.stderr)
  return vim.trim(r.stdout)
end

local function eq(actual, expected, what)
  if not vim.deep_equal(actual, expected) then
    error(("%s:\n  expected %s\n  actual   %s"):format(what, vim.inspect(expected), vim.inspect(actual)), 2)
  end
end

local function lines()
  return vim.api.nvim_buf_get_lines(0, 0, -1, false)
end

-- What `ergon outline` prints now, line by line.
local function outline()
  return vim.split(ergon("outline", "--project", "demo"), "\n", { plain = true })
end

-- The row for `slug`, and its line number.
local function row(slug)
  for i, line in ipairs(lines()) do
    if line:match("^(%S+)") == slug then
      return line, i
    end
  end
  error("no row " .. slug)
end

local function set_row(slug, text)
  local _, i = row(slug)
  vim.api.nvim_buf_set_lines(0, i - 1, i, false, { text })
end

-- `line` with cell `n` (1-9) replaced. The row shifts; its nesting doesn't.
local function set_cell(line, n, value)
  local pos = 1
  for i = 1, n do
    local s, e = line:find("%S+", pos)
    if i == n then
      return line:sub(1, s - 1) .. value .. line:sub(e + 1)
    end
    pos = e + 1
  end
end

-- The column a row's title starts at, `- ` included.
local function title_col(line)
  return line:find("%- [^-]*$")
end

local messages = {}
vim.notify = function(msg)
  table.insert(messages, msg)
end
local prompts = {}
local answer = 1
vim.fn.confirm = function(msg)
  table.insert(prompts, msg)
  return answer
end

ergon("init", "demo", "Build a demo", "--no-launch")
ergon("add", "Backend\n\nDetails.", "--priority", "p1") -- qxpj
ergon("add", "Write the API", "--parent", "qxpj") -- FTFE
ergon("add", "Frontend") -- nHcx
ergon("dep", "add", "nHcx", "FTFE")

-- Opening uses the directory's .ergon, and shows the table as printed.
vim.cmd("Ergon")
eq(vim.api.nvim_buf_get_name(0), "ergon://demo", "buffer name")
local header = lines()[1]
eq(lines(), outline(), "the outline")
eq(vim.bo.filetype, "ergon", "filetype")
local backend, b = row("qxpj")
local syn = function(lnum, col)
  return vim.fn.synIDattr(vim.fn.synID(lnum, col, 0), "name")
end
eq(syn(b, 1), "ErgonSlug", "slug highlight")
eq(syn(b, backend:find(" p1 ") + 1), "ErgonPriority", "priority highlight")
eq(syn(b, backend:find("~p1", 1, true)), "ErgonInferred", "calculated highlight")

-- Edit like text: rename, add a subtask, finish a task, delete one.
local api = row("FTFE")
local tests = set_cell(set_cell(set_cell(api, 4, "p2"), 2, "todo"), 1, "-")
tests = tests:gsub("Write the API$", "Write tests")
local _, n = row("nHcx")
vim.api.nvim_buf_set_lines(0, n - 1, n, false, {})
local _, f = row("FTFE")
vim.api.nvim_buf_set_lines(0, f - 1, f, false, { set_cell(api, 2, "done"), tests })
set_row("qxpj", (backend:gsub("Backend$", "Backend services")))
vim.cmd.write()
eq(#prompts, 1, "one confirmation")
for _, want in ipairs({
  "qxpj Backend: title -> Backend services",
  "FTFE Write the API: status unset -> done",
  'add "Write tests" under qxpj',
  "nHcx Frontend: delete",
}) do
  assert(prompts[1]:find(want, 1, true), "prompt lacks " .. want .. ":\n" .. prompts[1])
end
eq(lines(), outline(), "reloaded outline")
assert(row("pQyk"):find("- Write tests$"), "the new task, saved")
eq(vim.bo.modified, false, "saved")
eq(messages[#messages], "ergon: applied 4 changes", "summary")
eq(ergon("show", "qxpj", "--intent"), "Backend services\n\nDetails.", "body kept")

-- Undo can't bring back the applied edits.
vim.cmd("silent! undo")
eq(lines(), outline(), "after undo")

-- Declining the confirmation leaves the project and the edits alone.
answer = 2
set_row("pQyk", (row("pQyk"):gsub("Write tests$", "Write more tests")))
vim.cmd.write()
eq(vim.bo.modified, true, "still modified")
eq(ergon("tree", "pQyk"):match("Write tests$"), "Write tests", "not applied")
answer = 1

-- A mistake is marked in red on its cell, with the cursor there.
set_row("pQyk", set_cell(row("pQyk"), 8, "4444"))
local bad, p = row("pQyk")
vim.api.nvim_win_set_cursor(0, { 1, 0 })
vim.cmd.write()
eq(messages[#messages], ("ergon: line %d, deps: no task 4444 to depend on"):format(p), "error")
local marks = vim.diagnostic.get(0)
eq(#marks, 1, "one mark")
local s, e = bad:find("4444", 1, true)
eq({ marks[1].lnum + 1, marks[1].col, marks[1].end_col }, { p, s - 1, e }, "the deps cell marked")
eq(marks[1].severity, vim.diagnostic.severity.ERROR, "in red")
eq(vim.api.nvim_win_get_cursor(0), { p, s - 1 }, "cursor on the bad cell")

-- A refusal names tasks; their rows are marked.
vim.cmd("e!")
eq(#vim.diagnostic.get(0), 0, "reloading clears the marks")
set_row("FTFE", set_cell(row("FTFE"), 2, "todo"))
set_row("qxpj", set_cell(row("qxpj"), 2, "done"))
vim.cmd.write()
local marked = vim.tbl_map(function(m)
  return m.lnum + 1
end, vim.diagnostic.get(0))
table.sort(marked)
eq(marked, { select(2, row("qxpj")), select(2, row("FTFE")) }, "both tasks' rows marked")

-- A stale outline is refused, with a hint.
vim.cmd("e!")
ergon("edit", "qxpj", "--priority", "p0")
set_row("qxpj", (row("qxpj"):gsub("Backend services$", "Backend")))
vim.cmd.write()
assert(messages[#messages]:find("changed since this outline was printed", 1, true), messages[#messages])
assert(messages[#messages]:find(":e!", 1, true), "reload hint")
vim.cmd("e!")
eq(lines(), outline(), "reloaded with :e!")

-- >> and << nest a title, not the row, and never into the ~blocked column.
local before, w = row("pQyk")
vim.api.nvim_win_set_cursor(0, { w, 0 })
vim.cmd("normal >>")
eq(row("pQyk"), (before:gsub("%- Write tests$", "  - Write tests")), "nested a level deeper")
vim.cmd("normal <<")
vim.cmd("normal <<")
local top = (before:gsub("  %- Write tests$", "- Write tests"))
eq(row("pQyk"), top, "out a level")
vim.cmd("normal <<")
eq(row("pQyk"), top, "no further")

-- :ErgonFormat lines widened and hand-edited rows up again.
vim.cmd("e!")
set_row("FTFE", set_cell(row("FTFE"), 8, "pQyk"))
assert(title_col(row("FTFE")) ~= title_col(row("pQyk")), "widened row out of line")
vim.cmd("ErgonFormat")
eq(title_col(row("FTFE")), title_col(row("pQyk")), "siblings line up again")
eq(vim.bo.modified, true, "formatting is an edit")
vim.cmd("e!")

-- <CR> opens the full intent; writing it updates the task and the outline.
vim.api.nvim_win_set_cursor(0, { select(2, row("qxpj")), 0 })
vim.api.nvim_feedkeys(vim.keycode("<CR>"), "x", false)
eq(vim.api.nvim_buf_get_name(0), "ergon://demo/qxpj", "intent buffer")
eq(lines(), { "Backend services", "", "Details." }, "intent")
vim.api.nvim_buf_set_lines(0, 0, 1, false, { "Backend and database" })
vim.cmd.write()
eq(vim.bo.modified, false, "intent saved")
eq(ergon("show", "qxpj", "--intent"), "Backend and database\n\nDetails.", "intent written")
vim.cmd.close()
assert(row("qxpj"):find("- Backend and database$"), "outline refreshed")

-- Nothing changed: no prompt.
local count = #prompts
vim.cmd.write()
eq(#prompts, count, "no prompt")
eq(messages[#messages], "ergon: no changes", "no changes")

-- Reopening a loaded, unedited outline refreshes it.
ergon("add", "Docs")
vim.cmd("Ergon demo")
assert(lines()[#lines()]:find("- Docs$"), "refreshed on reopen")
assert(header ~= lines()[1], "version changed")

print("ok")
