# Cagara for Neovim

Filetype detection, syntax highlighting, and `cagara-lsp` for `.cagara`
files. Needs Neovim 0.10 or later.

## Install

Build the server from the repository root:

```sh
cargo build --release -p cagara-lsp
```

Add this directory to your runtime path. With lazy.nvim:

```lua
{
  dir = "/path/to/cagara-lang/editors/nvim",
  ft = "cagara",
  opts = {},
}
```

Or by hand, in `init.lua`:

```lua
vim.opt.runtimepath:append("/path/to/cagara-lang/editors/nvim")
require("cagara").setup()
```

## Options

```lua
require("cagara").setup({
  cmd = { "/path/to/cagara-lsp" },   -- default: see below
  root_markers = { "Cargo.toml", ".git" },
  on_attach = function(client, bufnr) end,
  capabilities = nil,                -- e.g. require("cmp_nvim_lsp").default_capabilities()
})
```

Without `cmd`, the server is `target/release/cagara-lsp` or
`target/debug/cagara-lsp` found upward from the file (a checkout of this
repository), else `cagara-lsp` on `PATH`.

Neovim 0.11+ maps the usual keys by default (`K` hover, `grr` references,
`gO` symbols, `<C-x><C-o>` completion); `gd` needs a mapping to
`vim.lsp.buf.definition` on older versions.
