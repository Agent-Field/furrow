#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN="$ROOT/target/release/agit"
WORK=$(mktemp -d "${TMPDIR:-/tmp}/agit-five-agents.XXXXXX")
REPO="$WORK/project"
export AGIT_DATA_DIR="$WORK/data"
export AGIT_NO_DAEMON=1

bold='\033[1m'
green='\033[0;32m'
reset='\033[0m'

step() { printf '\n%b%s%b\n' "$bold" "$1" "$reset"; }
ok() { printf '%bPASS%b  %s\n' "$green" "$reset" "$1"; }
fail() { printf 'FAIL  %s\n' "$1" >&2; exit 1; }

step "Build agit"
cargo build --release --quiet --manifest-path "$ROOT/Cargo.toml"

step "Create one dirty workspace with a 32 MiB warm dependency"
mkdir -p "$REPO/node_modules/runtime"
git -C "$REPO" init -q -b main
git -C "$REPO" config user.email demo@example.com
git -C "$REPO" config user.name Demo
printf 'node_modules/\n.env\n' >"$REPO/.gitignore"
printf 'base source\n' >"$REPO/app.txt"
git -C "$REPO" add .gitignore app.txt
git -C "$REPO" commit -q -m initial
printf 'dirty source must survive\n' >"$REPO/app.txt"
printf 'LOCAL_TOKEN=five-agent-demo\n' >"$REPO/.env"
dd if=/dev/zero of="$REPO/node_modules/runtime/cache.bin" bs=1048576 count=32 2>/dev/null
"$BIN" --repo "$REPO" watch --no-daemon >/dev/null
ok "complete warm state protected"

step "Materialize five full-state agent workspaces"
for index in 1 2 3 4 5; do
  "$BIN" --repo "$REPO" fork "agent-$index" \
    --destination "$WORK/agent-$index"
done

step "Run and seal five agents concurrently"
for index in 1 2 3 4 5; do
  (
    printf 'result from agent %s\n' "$index" >"$WORK/agent-$index/result-$index.txt"
    printf 'agent %s implementation\n' "$index" >"$WORK/agent-$index/app.txt"
    "$BIN" --repo "$WORK/agent-$index" snap -m "agent $index complete" >/dev/null
  ) &
done
wait

step "Verify pairwise isolation and complete warm state"
test "$(cat "$REPO/app.txt")" = "dirty source must survive" \
  || fail "an agent modified the source"
test ! -e "$REPO/result-1.txt" || fail "an agent result leaked into the source"
for index in 1 2 3 4 5; do
  fork="$WORK/agent-$index"
  test -f "$fork/result-$index.txt" || fail "agent $index lost its result"
  test -f "$fork/node_modules/runtime/cache.bin" || fail "agent $index lost warm dependencies"
  grep -q LOCAL_TOKEN "$fork/.env" || fail "agent $index lost ignored local configuration"
  for other in 1 2 3 4 5; do
    if test "$other" != "$index" && test -e "$fork/result-$other.txt"; then
      fail "agent $other leaked into agent $index"
    fi
  done
done
ok "five agents completed with zero cross-workspace or source interference"

step "Inspect disclosed fork costs"
"$BIN" --repo "$REPO" forks

printf '\n%bDemo complete.%b Six workspaces retained at %s\n' "$green" "$reset" "$WORK"
