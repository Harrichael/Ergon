if vim.g.loaded_ergon then
  return
end
vim.g.loaded_ergon = true

vim.api.nvim_create_user_command("Ergon", function(o)
  require("ergon").open(o.args)
end, {
  nargs = "?",
  complete = function()
    return require("ergon").projects()
  end,
  desc = "Open an ergon project's tasks (default: the current directory's)",
})

local group = vim.api.nvim_create_augroup("ergon", { clear = true })

vim.api.nvim_create_autocmd("BufReadCmd", {
  group = group,
  pattern = "ergon://*",
  callback = function(ev)
    require("ergon").read(ev.buf)
  end,
})

vim.api.nvim_create_autocmd("BufWriteCmd", {
  group = group,
  pattern = "ergon://*",
  callback = function(ev)
    require("ergon").write(ev.buf)
  end,
})

-- Colours match the CLI: stored values bold, calculated ones cyan.
local function highlights()
  local set = function(name, opts)
    vim.api.nvim_set_hl(0, name, vim.tbl_extend("force", { default = true }, opts))
  end
  set("ErgonHeader", { link = "Comment" })
  set("ErgonSlug", { link = "Comment" })
  set("ErgonEmpty", { link = "NonText" })
  set("ErgonMarker", { link = "Delimiter" })
  set("ErgonTodo", { link = "Delimiter" })
  set("ErgonInProgress", { link = "DiagnosticWarn" })
  set("ErgonLanding", { link = "DiagnosticInfo" })
  set("ErgonFinished", { link = "Comment" })
  set("ErgonPriority", { bold = true })
  set("ErgonInferred", { link = "DiagnosticHint" })
  set("ErgonBlocked", { link = "DiagnosticWarn" })
end
highlights()
vim.api.nvim_create_autocmd("ColorScheme", { group = group, callback = highlights })
