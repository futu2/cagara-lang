#!/bin/bash
# A MySQL-family server for the differential tests, driven the way `sqlite3` and
# `duckdb` are: a batch on stdin, rows on stdout, one per line, columns joined by
# `|`, NULL as `NULL`.
#
# The batch is `setup \x1e query \x1e query…` (0x1e is a record separator, which no
# SQL the compiler emits contains) — the same protocol `pglite.mjs` and `trino.sh`
# speak, so the Rust side drives one process instead of one per query.
#
#   mysql.sh --check   exit 0 if a server answers, 3 if not
#   mysql.sh < batch
#
# The server is not started here: neither MariaDB nor MySQL ships with this
# repository. See `docs/ENGINE-TESTS.md`.
set -u
SOCK="${MYSQL_SOCKET:-}"
# The shared differential cases name their tables `main.<name>`, and in the
# MySQL family a schema *is* a database — so the server needs one called `main`,
# not the `cagara` the rest of the examples use.
DB="${MYSQL_DB:-main}"
SEP=$'\x1e'

if [ "${1:-}" = "--check" ]; then
  [ -n "$SOCK" ] && [ -S "$SOCK" ] && [ -x "${MYSQL:-}" ] && exit 0
  exit 3
fi

# One statement: the client prints tab-separated with `-N -B` (no header, batch),
# so the tabs become the `|` the other harnesses write.
#
# An error is reported as a row, not swallowed: an empty block and a broken server
# look the same to the test, and a setup that silently failed would report every
# case as a wrong row instead of one message saying why.
run() {
  local out err
  # `-r` (raw) as well: batch mode escapes a backslash as `\\`, so a value
  # holding one would come back doubled and differ from the other engines.
  out=$("$MYSQL" --no-defaults --socket="$SOCK" -u root "$DB" -N -B -r \
    -e "$(printf '%s' "$1" | sed -e 's/[[:space:]]*$//' -e 's/;$//')" 2>/tmp/cagara-mysql-err.$$)
  err=$(head -1 /tmp/cagara-mysql-err.$$ 2>/dev/null)
  rm -f /tmp/cagara-mysql-err.$$
  if [ -n "$err" ]; then
    printf 'ERROR|%s\n' "$err"
  else
    printf '%s\n' "$out" | tr '\t' '|' | sed '/^$/d'
  fi
}

# All of stdin first, then the work. The parent writes the whole batch before it
# reads anything, so a harness that answered as it read would deadlock on a full
# pipe: `run_script` writes, then calls `wait_with_output`. `pglite.mjs` reads
# first for the same reason.
batch=$(cat)
first=1
queries=0
while IFS= read -r -d "$SEP" block || [ -n "$block" ]; do
  if [ $first -eq 1 ]; then
    # The setup: one statement per line, output discarded (a `DROP` reports
    # nothing and an `INSERT` reports a row count; neither is a result set).
    first=0
    while IFS= read -r line; do
      [ -n "${line// /}" ] && "$MYSQL" --no-defaults --socket="$SOCK" -u root "$DB" \
        -e "$(printf '%s' "$line" | sed 's/;$//')" > /dev/null 2>&1
    done <<< "$block"
  else
    queries=$((queries + 1))
    [ $queries -gt 1 ] && printf '\x1e\n'
    run "$block"
  fi
done <<< "$batch"
