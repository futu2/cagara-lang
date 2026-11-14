#!/bin/bash
# A Trino server for the differential tests, driven the way `sqlite3` and
# `duckdb` are: a batch on stdin, rows on stdout, one per line, columns joined
# by `|`, NULL as `NULL`.
#
# The batch is `setup \x1e query \x1e query…` (0x1e is a record separator, which
# no SQL the compiler emits contains) — the same protocol `pglite.mjs` speaks, so
# the Rust side drives one process instead of one per query.
#
#   trino.sh --check   exit 0 if a server answers, 3 if not
#   trino.sh < batch
#
# The server is not started here: `nix develop` does not provide one and a Trino
# distribution is 680 MiB of jars. See `docs/ENGINE-TESTS.md`.
set -u
URL="${TRINO_URL:-http://localhost:18080}"
SEP=$'\x1e'

if [ "${1:-}" = "--check" ]; then
  if curl -sS -o /dev/null --max-time 5 "$URL/v1/info" 2>/dev/null; then exit 0; fi
  exit 3
fi

# One statement: rows printed, following `nextUri` until the query is done.
run() {
  local resp
  resp=$(curl -sS -X POST "$URL/v1/statement" \
    -H "X-Trino-User: cagara" -H "X-Trino-Catalog: memory" -H "X-Trino-Schema: main" \
    --data-binary "$(printf '%s' "$1" | sed -e 's/[[:space:]]*$//' -e 's/;$//')") \
    || { echo "ERROR: cannot reach $URL" >&2; exit 4; }
  while :; do
    printf '%s' "$resp" | python3 -c '
import sys, json
d = json.load(sys.stdin)
e = d.get("error")
if e:
    # One line, so the block is still one block and the test can report *which*
    # query the server refused instead of losing the rest of the batch.
    print("ERROR|" + e.get("message", "unknown").replace("\n", " "))
    sys.exit(0)
for row in d.get("data") or []:
    print("|".join("NULL" if v is None else str(v) for v in row))
' || exit 5
    local next
    next=$(printf '%s' "$resp" | python3 -c 'import sys,json;print(json.load(sys.stdin).get("nextUri",""))')
    [ -z "$next" ] && return 0
    resp=$(curl -sS "$next") || exit 4
  done
}

# The first block is the setup and Trino takes one statement per request, so each
# of its lines runs in turn and its output is discarded (a `DROP` reports nothing,
# an `INSERT` reports a row count; neither is a result set). Every later block is
# one query, and the result sets are joined by the separator — never preceded by
# one, which is the shape `pglite.mjs` writes and `pg_batch` reads.
setup=1
queries=0
while IFS= read -r -d "$SEP" block || [ -n "$block" ]; do
  if [ $setup -eq 1 ]; then
    setup=0
    while IFS= read -r line; do
      [ -n "${line// /}" ] && run "$line" > /dev/null
    done <<< "$block"
  else
    queries=$((queries + 1))
    # On its own line: the Rust side splits the output by line, so a separator
    # glued to the next row would be read as part of it.
    [ $queries -gt 1 ] && printf '\x1e\n'
    run "$block" | sed '/^$/d'
  fi
done
