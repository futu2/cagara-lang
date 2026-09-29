-- Cagara for Neovim: starts the language server (`cagara lsp`) for `cagara` buffers.
--
--   require("cagara").setup({
--     cmd = nil,          -- server command; default: see M.server_cmd
--     root_markers = { "Cargo.toml", ".git" },
--     on_attach = nil,    -- function(client, bufnr)
--     capabilities = nil, -- e.g. from a completion plugin
--   })

local M = {}

local defaults = {
  cmd = nil,
  root_markers = { "Cargo.toml", ".git" },
  on_attach = nil,
  capabilities = nil,
}

M.config = vim.deepcopy(defaults)

local exe = vim.fn.has("win32") == 1 and "cagara.exe" or "cagara"

--- The server command for a buffer in `dir`: `cagara lsp`, with
--- `target/release/cagara` or `target/debug/cagara` in `dir` or a parent (a
--- checkout of this repository), else `cagara` on PATH.
---@param dir string?
---@return string[]
function M.server_cmd(dir)
  if dir then
    for _, profile in ipairs({ "release", "debug" }) do
      local found = vim.fs.find("target/" .. profile .. "/" .. exe, { path = dir, upward = true, type = "file" })[1]
      if found then
        return { found, "lsp" }
      end
    end
  end
  return { exe, "lsp" }
end

---@param bufnr integer
local function start(bufnr)
  local file = vim.api.nvim_buf_get_name(bufnr)
  if file == "" then
    return
  end
  local dir = vim.fs.dirname(file)
  local marker = vim.fs.find(M.config.root_markers, { path = dir, upward = true })[1]
  local root = marker and vim.fs.dirname(marker) or dir
  local cmd = M.config.cmd or M.server_cmd(dir)
  if vim.fn.executable(cmd[1]) ~= 1 then
    vim.notify_once(
      ("cagara: `%s` not found; build it with `cargo build --release` or pass `cmd` to setup()"):format(cmd[1]),
      vim.log.levels.WARN
    )
    return
  end
  vim.lsp.start({
    name = "cagara",
    cmd = cmd,
    root_dir = root,
    on_attach = M.config.on_attach,
    capabilities = M.config.capabilities,
  }, { bufnr = bufnr })
end

---@param opts table?
function M.setup(opts)
  M.config = vim.tbl_deep_extend("force", vim.deepcopy(defaults), opts or {})
  -- Also in ftdetect/, which is skipped when this directory joins the
  -- runtime path after startup.
  vim.filetype.add({ extension = { cagara = "cagara" } })
  local group = vim.api.nvim_create_augroup("cagara_lsp", { clear = true })
  vim.api.nvim_create_autocmd("FileType", {
    group = group,
    pattern = "cagara",
    callback = function(args)
      start(args.buf)
    end,
  })
  -- Buffers opened before setup().
  for _, buf in ipairs(vim.api.nvim_list_bufs()) do
    if vim.api.nvim_buf_is_loaded(buf) and vim.bo[buf].filetype == "cagara" then
      start(buf)
    end
  end
end

return M
