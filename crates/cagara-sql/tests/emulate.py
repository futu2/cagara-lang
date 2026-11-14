#!/usr/bin/env python3
"""Read one dialect's SQL, write the same statements in DuckDB's.

The dialects this repository cannot run — T-SQL, BigQuery, Snowflake, and the ANSI
spelling everything else derives from — are still worth *executing*, and an
independent implementation can read them: Python's `sqlglot` is the project the Rust
`sqlglot-rust` crate is modelled on, with its own parser and its own understanding of
each dialect. Transpiling our output to DuckDB and running it there answers the
question a text check cannot — do the rows come out the same — without a SQL Server.

Input, on stdin: `dialect \\x1e statement \\x1e statement…`
Output: one block per statement, `OK|<duckdb sql>` or `ERR|<message>`, joined by `\\x1e`.

    emulate.py --check   exit 0 if `sqlglot` is importable, 3 if not

It is a *second opinion*, not the server: a construct both implementations
misunderstand passes here, and DuckDB's engine is doing the computing for every
dialect. `docs/ENGINE-TESTS.md` says what that is and is not worth.
"""

import sys

SEP = "\x1e"

# This repository's dialect names, in Python sqlglot's spelling. `ansi` is the empty
# string there: the default, standard-SQL reading.
DIALECTS = {
    "ansi": "",
    "postgres": "postgres",
    "duckdb": "duckdb",
    "sqlite": "sqlite",
    "mysql": "mysql",
    "trino": "trino",
    "spark": "spark",
    "bigquery": "bigquery",
    "snowflake": "snowflake",
    "tsql": "tsql",
}


def one_line(text: str) -> str:
    """A block is one line, so a message cannot carry a newline into the output."""
    return " ".join(str(text).split())


def main() -> int:
    try:
        import sqlglot
    except ImportError:
        return 3
    if "--check" in sys.argv:
        return 0

    blocks = sys.stdin.read().split(SEP)
    read = DIALECTS[blocks[0].strip()]
    out = []
    for sql in blocks[1:]:
        try:
            tree = sqlglot.parse_one(sql, read=read)
            out.append("OK|" + one_line(tree.sql(dialect="duckdb")))
        except Exception as error:  # noqa: BLE001 — any failure is a result here
            out.append("ERR|" + one_line(f"{type(error).__name__}: {error}"))
    sys.stdout.write(SEP.join(out))
    return 0


if __name__ == "__main__":
    sys.exit(main())
