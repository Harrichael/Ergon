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

-- Opening uses the directory's .ergon.
vim.cmd("Ergon")
eq(vim.api.nvim_buf_get_name(0), "ergon://demo", "buffer name")
local header = lines()[1]
eq(vim.list_slice(lines(), 2), {
  "qxpj [ ] Backend !p1",
  "  FTFE Write the API",
  "nHcx [ ] Frontend deps:FTFE",
}, "outline")
eq(vim.bo.filetype, "ergon", "filetype")
eq(vim.bo.shiftwidth, 2, "shiftwidth")
eq(vim.fn.synIDattr(vim.fn.synID(2, 1, 0), "name"), "ErgonSlug", "slug highlight")
eq(vim.fn.synIDattr(vim.fn.synID(2, 18, 0), "name"), "ErgonPriority", "priority highlight")

-- Inference shows as virtual text: FTFE inherits p1, nHcx is blocked.
local marks = vim.api.nvim_buf_get_extmarks(0, -1, 0, -1, { details = true })
local virt = {}
for _, m in ipairs(marks) do
  virt[m[2] + 1] = vim.trim(table.concat(vim.tbl_map(function(c)
    return c[1]
  end, m[4].virt_text), ""))
end
eq(virt, { [3] = "p1", [4] = "blocked" }, "annotations")

-- Edit like text: rename, add a subtask, finish a task, delete one.
vim.api.nvim_buf_set_lines(0, 1, -1, false, {
  "qxpj [ ] Backend services !p1",
  "  FTFE [x] Write the API",
  "  [ ] Write tests !p2",
})
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
eq(vim.list_slice(lines(), 2), {
  "qxpj [ ] Backend services !p1",
  "  FTFE [x] Write the API",
  "  pQyk [ ] Write tests !p2",
}, "reloaded outline")
eq(vim.bo.modified, false, "saved")
eq(messages[#messages], "ergon: applied 4 changes", "summary")
eq(ergon("show", "qxpj", "--intent"), "Backend services\n\nDetails.", "body kept")

-- Undo can't bring back the applied edits.
vim.cmd("silent! undo")
eq(lines()[2], "qxpj [ ] Backend services !p1", "after undo")

-- Declining the confirmation leaves the project and the edits alone.
answer = 2
vim.api.nvim_buf_set_lines(0, 3, 4, false, { "  pQyk [ ] Write more tests !p2" })
vim.cmd.write()
eq(vim.bo.modified, true, "still modified")
eq(ergon("tree", "pQyk"):match("Write tests$"), "Write tests", "not applied")
answer = 1

-- A mistake is reported, with the cursor on its line.
vim.api.nvim_buf_set_lines(0, 3, 4, false, { "  pQyk [ ] Write tests deps:4444" })
vim.api.nvim_win_set_cursor(0, { 1, 0 })
vim.cmd.write()
eq(messages[#messages], "ergon: line 4: no task 4444 to depend on", "error")
eq(vim.api.nvim_win_get_cursor(0)[1], 4, "cursor on the bad line")

-- A stale outline is refused, with a hint.
vim.cmd("e!")
ergon("edit", "qxpj", "--priority", "p0")
vim.api.nvim_buf_set_lines(0, 1, 2, false, { "qxpj [~] Backend services !p1" })
vim.cmd.write()
assert(messages[#messages]:find("changed since this outline was printed", 1, true), messages[#messages])
assert(messages[#messages]:find(":e!", 1, true), "reload hint")
vim.cmd("e!")
eq(lines()[2], "qxpj [ ] Backend services !p0", "reloaded with :e!")

-- <CR> opens the full intent; writing it updates the task and the outline.
vim.api.nvim_win_set_cursor(0, { 2, 0 })
vim.api.nvim_feedkeys(vim.keycode("<CR>"), "x", false)
eq(vim.api.nvim_buf_get_name(0), "ergon://demo/qxpj", "intent buffer")
eq(lines(), { "Backend services", "", "Details." }, "intent")
vim.api.nvim_buf_set_lines(0, 0, 1, false, { "Backend and database" })
vim.cmd.write()
eq(vim.bo.modified, false, "intent saved")
eq(ergon("show", "qxpj", "--intent"), "Backend and database\n\nDetails.", "intent written")
vim.cmd.close()
eq(lines()[2], "qxpj [ ] Backend and database !p0", "outline refreshed")

-- Reordering siblings is kept.
vim.api.nvim_buf_set_lines(0, 2, 4, false, {
  "  pQyk [ ] Write tests !p2",
  "  FTFE [x] Write the API",
})
vim.cmd.write()
assert(prompts[#prompts]:find("qxpj Backend and database: reorder subtasks", 1, true), prompts[#prompts])
vim.cmd("e!")
eq(vim.list_slice(lines(), 3), { "  pQyk [ ] Write tests !p2", "  FTFE [x] Write the API" }, "order kept")

-- Nothing changed: no prompt.
local before = #prompts
vim.cmd.write()
eq(#prompts, before, "no prompt")
eq(messages[#messages], "ergon: no changes", "no changes")

-- Reopening a loaded, unedited outline refreshes it.
ergon("add", "Docs")
vim.cmd("Ergon demo")
eq(lines()[#lines()], "9cDQ [ ] Docs", "refreshed on reopen")
assert(header ~= lines()[1], "version changed")

print("ok")
