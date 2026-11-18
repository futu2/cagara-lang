#!/usr/bin/env bash
# Run the test binaries concurrently, within one thread-per-core budget.
#
# `cargo test --workspace` runs test *binaries* one after another. Within a
# binary, libtest already uses every core — but the workspace has 19 binaries, so
# a single 30-second binary holds the whole run while fifteen cores idle. The
# binaries are independent processes with independent scratch directories (each
# test makes its own `TempDir`), so there is nothing to serialise.
#
# This is what `cargo nextest` does; it is 40 lines here, needs no install, and
# leaves `cargo test` as the thing CI still runs.
#
# The budget matters more than the parallelism. `JOBS` binaries x `TEST_THREADS`
# each should not exceed the core count, because libtest already fills a binary's
# threads with work; oversubscribing makes the language-server tests — which wait
# on a fixed wall-clock window for the server to publish — fail for reasons that
# have nothing to do with the code.
#
# Usage: scripts/parallel-test.sh [extra cargo args...]
#   scripts/parallel-test.sh                 # whole workspace
#   scripts/parallel-test.sh -p cagara-sql   # one package
set -uo pipefail

cd "$(dirname "$0")/.."

# Build every test binary first, so the parallel phase is pure test execution.
cargo test "$@" --no-run --quiet || exit 1

# `--message-format=json` names the built artifacts. Only those with
# `"test":true` are libtest binaries — the CLI's own `cagara` binary is in the
# same list and does not accept `--quiet`.
mapfile -t bins < <(
  cargo test "$@" --no-run --message-format=json 2>/dev/null \
    | python3 -c '
import json, sys
for line in sys.stdin:
    try:
        a = json.loads(line)
    except Exception:
        continue
    if a.get("executable") and (a.get("profile") or {}).get("test"):
        print(a["executable"])
' | sort -u
)

if [ ${#bins[@]} -eq 0 ]; then
  echo "no test binaries" >&2
  exit 1
fi

cores=$(nproc 2>/dev/null || echo 4)
# The parallelism budget is *total*: `jobs` binaries times `threads` each should
# not exceed the cores. libtest already uses one thread per core inside a single
# binary, so running one binary per core multiplies the load by the core count
# several times over — and the LSP tests wait on a fixed 600 ms wall-clock window
# for the server to publish, so they fail under that load for reasons that have
# nothing to do with the code.
#
# One test thread per binary, several binaries at once, keeps the total honest and
# the failures meaningful.
jobs=${JOBS:-$(( cores > 4 ? 4 : cores ))}
threads=${TEST_THREADS:-$(( cores / jobs > 0 ? cores / jobs : 1 ))}
echo "running ${#bins[@]} test binaries, $jobs at a time, $threads test threads each" >&2

# One log per binary so concurrent output does not interleave, and so a failure
# names the binary that produced it.
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

pids=(); names=()
for b in "${bins[@]}"; do
  name=$(basename "$b")
  "$b" --quiet --test-threads="$threads" >"$tmp/$name.log" 2>&1 &
  pids+=($!)
  names+=("$name")
  # Throttle to one process per core.
  while [ "$(jobs -rp | wc -l)" -ge "$jobs" ]; do wait -n; done
done

failed=0
for i in "${!pids[@]}"; do
  if ! wait "${pids[$i]}"; then
    echo "FAILED: ${names[$i]}" >&2
    cat "$tmp/${names[$i]}.log" >&2
    failed=1
  fi
done

if [ $failed -eq 0 ]; then
  echo "all ${#bins[@]} test binaries passed" >&2
fi
exit $failed
