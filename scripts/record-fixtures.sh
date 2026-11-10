#!/usr/bin/env bash
# Record failed testnet transactions as offline RPC fixtures.
#
# Usage:
#   scripts/record-fixtures.sh
#   scripts/record-fixtures.sh <name> <hash>
#
# Build sororewind first. Only a clean fidelity match replaces a fixture; a
# drifted replay (exit 2), RPC error, or decode error keeps the old capture.
set -euo pipefail
cd "$(dirname "$0")/.."

BIN=${BIN:-target/release/sororewind}
[ -x "$BIN" ] || BIN=target/debug/sororewind
[ -x "$BIN" ] || { echo "build sororewind first (cargo build)" >&2; exit 1; }

if [ "$#" -eq 2 ]; then
  pairs=("$1" "$2")
elif [ "$#" -eq 0 ]; then
  pairs=()
  while IFS=$'\t' read -r name hash; do
    [ -z "${name:-}" ] && continue
    pairs+=("$name" "$hash")
  done < fixtures/MANIFEST
else
  echo "usage: $0 [<name> <hash>]" >&2
  exit 1
fi

[ ${#pairs[@]} -gt 0 ] || { echo "no fixtures configured" >&2; exit 1; }

for ((i=0; i<${#pairs[@]}; i+=2)); do
  name=${pairs[i]}
  hash=${pairs[i+1]}
  case "$name" in
    ''|.*|*/*|*..*) echo "invalid fixture name: $name" >&2; exit 1 ;;
  esac

  dir="fixtures/$name"
  stage=$(mktemp -d "fixtures/.${name}.staging.XXXXXX")
  backup="fixtures/.${name}.backup.$$"
  echo "$hash" > "$stage/hash.txt"
  echo "recording $name ($hash) on testnet…"

  set +e
  "$BIN" "$hash" --network testnet --record "$stage" --no-color > "$stage/report.txt" 2>&1
  code=$?
  set -e

  if [ "$code" -ne 0 ]; then
    if [ "$code" -eq 2 ]; then
      echo "  FAILED: replay diverged; previous fixture was kept. Use a fresh hash." >&2
    else
      echo "  FAILED (exit $code); previous fixture was kept:" >&2
    fi
    sed 's/^/    /' "$stage/report.txt" >&2
    rm -rf -- "$stage"
    exit "$code"
  fi
  response_file=$(find "$stage" -maxdepth 1 -name '*.json' -print -quit)
  if [ -z "$response_file" ]; then
    echo "  FAILED: command produced no RPC response files; previous fixture was kept" >&2
    rm -rf -- "$stage"
    exit 1
  fi

  if [ -e "$dir" ]; then
    mv -- "$dir" "$backup"
  fi
  if mv -- "$stage" "$dir"; then
    rm -rf -- "$backup"
  else
    [ ! -e "$backup" ] || mv -- "$backup" "$dir"
    rm -rf -- "$stage"
    exit 1
  fi
  echo "  ok ($(find "$dir" -maxdepth 1 -name '*.json' | wc -l | tr -d ' ') responses)"
done
