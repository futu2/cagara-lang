# Running the engines the differential suite needs

`crates/cagara-sql/tests/engines.rs` compares *rows*, not SQL text: the same Cagara
queries are compiled per dialect and run on real engines, and a difference is a bug.
Three engines are provided by `nix develop`; two are not, and this is how to start
them.

| engine | how it runs | provided by `nix develop` |
|---|---|---|
| SQLite | `sqlite3` shell | yes |
| DuckDB | `duckdb` shell | yes |
| PostgreSQL | PGlite (Postgres in WebAssembly) through `node` | `npm install --no-save @electric-sql/pglite` |
| Trino | `tests/trino.sh` → a server on `http://localhost:18080` | no — see below |
| MySQL family | `tests/mysql.sh` → a MariaDB on `$MYSQL_SOCKET` | no — see below |
| T-SQL, BigQuery, Snowflake, ANSI | `tests/emulate.py` → DuckDB, via Python sqlglot | `uv pip install sqlglot` |

Every test skips when its engine is missing, unless `CAGARA_REQUIRE_ENGINES` is set,
in which case a missing engine fails the run. That is what a full local run does:

```sh
CAGARA_REQUIRE_ENGINES=1 MYSQL_SOCKET=… MYSQL=… CAGARA_PYTHON=… \
  cargo test --workspace
```

## CI does not run these

CI sets `CAGARA_NO_ENGINES=1`, which turns every test that executes SQL on an engine into
an explicit skip. The reason is weight, not doubt: the comparison needs three downloads
and two servers on every push.

Leaving the engines out is *not* enough, and that is why the switch exists. The runner
ships its own `sqlite3`, older than the 3.46 that the `floor` date modifier needs, so the
engine is present and wrong rather than absent — it would fail instead of skipping.
`CAGARA_NO_ENGINES` makes the omission a decision.

What still runs in CI is every test that compares the generated SQL *text*
(`each_dialect_uses_the_spelling_it_has`, `each_dialect_avoids_what_it_cannot_read`,
`every_statement_is_well_formed_sql`, and the rest), which catches a wrong dialect
spelling without a server. What CI cannot catch is a dialect that is spelled correctly and
still means something else; that is what this document is for.

## Trino

Trino is the strictest target in the family and the one worth the setup: no implicit
casts between numeric types, `OFFSET` before `LIMIT`, `||` for concatenation, and its
own spelling for half the functions. Two bugs were found the first time it ran — a
`date` literal reaching `DATE_DIFF` as a string, and the paging order — and both were
invisible to every text check.

A Trino distribution is ~680 MiB of jars, so it is not part of the dev shell. It needs
**Java 17**, and releases are built for one Java version: 435 is the newest that runs
on 17, and 450+ require 21+.

```sh
# 1. the tarball (Maven Central redirects to GitHub, which may not serve it; the
#    Google mirror does)
curl -o trino-server-435.tar.gz \
  https://maven-central.storage-download.googleapis.com/maven2/io/trino/trino-server/435/trino-server-435.tar.gz
tar xzf trino-server-435.tar.gz && cd trino-server-435

# 2. configuration: one node, the `memory` connector, and a fixed port
mkdir -p etc/catalog data
cat > etc/node.properties <<'P'
node.environment=test
node.id=cagara-test
node.data-dir=$PWD/data
P
cat > etc/jvm.config <<'P'
-server
-Xmx2G
-XX:+UseG1GC
-XX:+ExitOnOutOfMemoryError
P
cat > etc/config.properties <<'P'
coordinator=true
node-scheduler.include-coordinator=true
http-server.http.port=18080
discovery.uri=http://localhost:18080
query.max-memory=1GB
query.max-memory-per-node=512MB
P
echo 'connector.name=memory' > etc/catalog/memory.properties

# 3. run it (takes about a minute to answer)
./bin/launcher run

# 4. the schema and tables the tests use
curl -sS -X POST http://localhost:18080/v1/statement -H 'X-Trino-User: cagara' \
  --data-binary 'CREATE SCHEMA IF NOT EXISTS main'
```

`TRINO_URL` overrides the address. The harness needs `curl`, `python3` and `bash`,
which the dev shell already has.

The `memory` connector has no `DELETE` and no `DROP SCHEMA … CASCADE`, so the setup
drops and recreates each table; a test run leaves them behind, which is harmless.

## MySQL

The MySQL dialect is worth checking by *execution* more than any other: its
expressions mean different things, not merely look different — `||` is boolean OR,
integer division and `%` round differently, `FULL JOIN` does not exist, a boolean is
`1`/`0`.

MariaDB is used because it is a drop-in for the dialect and ships as a self-contained
tarball, so none of this needs root, a package manager or a container:

```sh
curl -o mariadb.tar.gz \
  https://archive.mariadb.org/mariadb-11.4.14/bintar-linux-systemd-x86_64/mariadb-11.4.14-linux-systemd-x86_64.tar.gz
tar xzf mariadb.tar.gz
M=mariadb-11.4.14-linux-systemd-x86_64

# a data directory of its own, and a root user with no password
./$M/scripts/mariadb-install-db --no-defaults --basedir=$PWD/$M --datadir=$PWD/data \
  --auth-root-authentication-method=normal

# the server: a socket, a private port, and no grants to configure
./$M/bin/mariadbd --no-defaults --basedir=$PWD/$M --datadir=$PWD/data \
  --port=13306 --socket=$PWD/mysql.sock --skip-grant-tables &

# the schema the shared cases use
./$M/bin/mariadb --no-defaults --socket=$PWD/mysql.sock -u root \
  -e 'CREATE DATABASE IF NOT EXISTS main'

# then the test, with the two variables it looks for
MYSQL_SOCKET=$PWD/mysql.sock MYSQL=$PWD/$M/bin/mariadb \
  cargo test -p cagara-sql --test engines -- mysql_agrees
```

`MYSQL_DB` overrides the database (default `main`, because that is the schema the
shared cases name). The harness runs the client with `-N -B -r`: no header, batch
mode, and *raw* output — without `-r` the client escapes a backslash as `\\`, which
looks exactly like a dialect bug and is not one.

## The emulated dialects

T-SQL, BigQuery, Snowflake and ANSI have no server here. They are still *executed*, by
translating them to DuckDB with an independent implementation:

```sh
uv pip install sqlglot        # or: CAGARA_PYTHON=/path/to/python cargo test ...
```

`tests/emulate.py` reads our output in the target dialect and writes it as DuckDB;
`the_emulated_dialects_mean_the_same_thing` runs those statements on DuckDB and compares
the rows with what the real engines return for the same case. 403 statements go through
it.

**What it is worth.** It found two BigQuery bugs on its first run, both invisible to a
text check because the SQL looks right: a bare `UNION`, which that dialect's grammar
does not allow, and a doubled `''` quote, which it parses as two adjacent literals.

**What it is not.** A second opinion, not the server:

- A construct *both* implementations misunderstand passes. The Rust generator and
  Python sqlglot share a lineage.
- DuckDB computes every dialect's answer, so a semantic difference DuckDB also has is
  invisible.
- It cannot judge a type. `a / b` on two integers is integer division in ANSI and
  T-SQL and float division in DuckDB, and the emulator has no column types to know
  which — `annotate_types` with the real schema still emits `a / b`. Those cases are
  listed in `EMULATION_BLIND` and are run on the five real engines instead.

Three T-SQL statements cannot run through it at all (`EMULATION_UNREADABLE`), each for
a limit of the second opinion rather than of our output: `+` on strings, `CAST(… AS
VARCHAR(MAX))`, and `@@DATEFIRST`. The set is pinned, so a statement that stops running
fails the test rather than quietly shrinking the coverage.

## Why not the other dialects

SQL Server has no self-contained archive, and BigQuery and Snowflake need a cloud
account — so those three are covered by the emulation above plus the two text checks in
`engines.rs` (`each_dialect_avoids_what_it_cannot_read`,
`each_dialect_uses_the_spelling_it_has`).

## How strong each claim is, as code

The table above is prose, and prose drifts. `engines.rs` carries the same information as
a `VERIFIED` table — one entry per dialect the crate accepts, each `Engine(name)`,
`Emulated` or `Text` — and two tests keep it honest:

- `every_dialect_says_what_verifies_it` — the table covers every accepted dialect (a new
  one that is not described fails), names no dialect the crate does not accept, claims no
  engine that has no harness, and its `Emulated` set is what
  `the_emulated_dialects_mean_the_same_thing` actually iterates.
- `a_text_only_target_is_a_deliberate_choice` — every `Text` target still compiles, so
  the entry cannot come to mean "this one is broken and nobody noticed".

`Text` is for the aliases: `presto` and `athena` are Trino's spelling in another era,
`hive` and `databricks` are Spark's, `doris`/`starrocks`/`singlestore` are the MySQL
family's. Each is covered by its family's engine; no separate server here speaks it.

**Why this exists.** The `range`/`groups` work found two limits that no emulation could
have shown — MariaDB rejects `GROUPS` outright, and refuses a `range` frame with two sort
keys because its `NULLS LAST` emulation adds a second. Both were found by running a
server, and both would have shipped if the only evidence had been "sqlglot translated it
and DuckDB agreed". The table makes that distinction a test instead of a paragraph.
