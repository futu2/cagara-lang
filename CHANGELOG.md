# Changelog

## Unreleased

- Reject self- and mutually recursive definitions during type checking, even
  without an annotation. Previously an unannotated cycle could check as a
  fresh type variable and produce no SQL.
- Accept a leading UTF-8 BOM on input; `cagara fmt` preserves it.
- Render diagnostics with character columns and clip very long source lines.
- Write formatted files through a sibling temporary and rename.
- Publish LSP diagnostics for the edited document immediately and refresh other
  open documents after a short debounce, instead of recompiling every document
  on every keystroke.
- Fix the README example (`sum` is nullable), compile the README example in CI,
  and check the version references in `flake.nix`, `CAGARA.md`, and the VS Code
  README.
- Replace the stale "declare operators in prelude.cagara" guidance in
  diagnostics, comments, and editor syntax files with the fixed-table rule.
- Fix rustdoc links, add a rustdoc CI gate, and add Windows/macOS compile
  checks plus a scheduled RustSec audit.
