# Changelog

## Unreleased

- Typing near the top of a file no longer re-checks and re-elaborates the rest
  of it. A character inserted anywhere but the end shifts every byte below it,
  and the incremental cache read that shift as an *edit*: a definition below the
  cursor parsed to a tree with different spans, its spans were part of both the
  facts that authorize reuse and the parse the checker is handed, and so one
  keystroke rebuilt the file. Over stdio LSP, a single character inserted per
  keystroke into a generated 1000-definition file (84 KB) took **154 ms** to
  report its own diagnostics and now takes **19 ms**; at 200 definitions, 29.5 ms
  to 2.5 ms. Nothing was added outside the pipeline: the same `check` runs the
  same per-definition checks, and the same `compile_reusing` runs the same
  elaborations. What changed is what counts as the same definition.

  Per definition, the workspace now decides whether an input is current by
  comparing its *text* (`Workspace::unchanged_defs`) rather than its parsed
  tree. A definition whose own text is byte-identical keeps its input — and with
  it its memoized check — even when everything above it moved. The spans that
  check recorded are then stale by exactly the distance it moved, one offset for
  the whole definition, and `module_check` applies that offset where a span
  becomes a position: rendered diagnostics, the module's typed-span index that
  hover searches, and the two places in normalization that build an origin from
  a typed span.

  In the compilation, a definition's offset is no longer one of its
  `DefinitionFacts`. A definition that only moved is not dirty, so its cached
  relation is reused; the locations it carries are offsets into the text it was
  compiled against, and `incremental::Moves` translates each one through the
  definition that wrote it, refusing reuse rather than guessing when a module's
  definition list changed and an index no longer names the same definition. The
  proof is still exact — a definition's text and the closed data its check
  produced, with no hash standing in for equality — it just no longer says that
  moving is changing.

  Two tests that asserted the old behaviour now assert the new one: `cached
  relations keep current source locations` still requires a reused relation to
  equal a fresh compile, and `an edit rebuilds its module` became `an edit
  rebuilds the definition it touched and keeps its siblings`. A comment-only
  insertion above every definition now re-checks nothing, re-elaborates nothing,
  and still produces a relation identical to a fresh compile.
