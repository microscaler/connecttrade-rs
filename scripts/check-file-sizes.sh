#!/usr/bin/env bash
# Large-file rules (docs/coding-standards.md), on every tracked file:
#   1. no file over MAX_BYTES (512 KiB): binaries, dumps and vendored blobs do not belong in git;
#   2. no hand-written Rust file over MAX_RUST_LINES (500): split it into modules by concern.
# Generated code (crates/connecttrade/src/generated/) is exempt from 2: the generator decides its shape.
# Exceptions to 1 are listed, with a reason each, in .large-files-allow (path<TAB>reason); none today.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
MAX_BYTES=${MAX_BYTES:-524288}
MAX_RUST_LINES=${MAX_RUST_LINES:-500}
allowed() { [ -f .large-files-allow ] && cut -f1 .large-files-allow | grep -qxF "$1"; }
fail=0
while IFS= read -r -d '' f; do
  [ -f "$f" ] || continue
  size=$(wc -c < "$f")
  if [ "$size" -gt "$MAX_BYTES" ] && ! allowed "$f"; then
    echo "too large: $f is $size bytes (limit $MAX_BYTES); keep it out of git or list it in .large-files-allow with a reason"
    fail=1
  fi
  case "$f" in
    crates/connecttrade/src/generated/*) ;;
    *.rs)
      lines=$(wc -l < "$f")
      if [ "$lines" -gt "$MAX_RUST_LINES" ]; then
        echo "too long: $f has $lines lines (limit $MAX_RUST_LINES); split it into modules by concern"
        fail=1
      fi
      ;;
  esac
done < <(git ls-files -z)
[ "$fail" -eq 0 ] && echo "file sizes ok (bytes <= $MAX_BYTES, hand-written Rust <= $MAX_RUST_LINES lines)"
exit "$fail"
