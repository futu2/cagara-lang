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

Every test skips when its engine is missing, unless `CAGARA_REQUIRE_ENGINES` is set,
in which case a missing engine fails the run. That is what CI does.

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

## Why not the other dialects

MySQL, T-SQL, BigQuery and Snowflake each need a server this repository does not
start, and none of them is available as a single self-contained archive. They are
covered by the two text checks in `engines.rs` — `each_dialect_avoids_what_it_cannot_read`
and `each_dialect_uses_the_spelling_it_has` — which is weaker than an engine, and the
tests say so where they are defined.
