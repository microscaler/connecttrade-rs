#!/usr/bin/env bash
# Re-fetch Connect Trade's OpenAPI spec and LLM docs into spec/, regenerate the client, and report what changed.
# Exit 0 with no changes, 1 on any change (so a scheduled run fails loudly when Connect Trade publishes a new spec).
set -euo pipefail
cd "$(git rev-parse --show-toplevel)/spec"
for f in openapi.json llms.txt llms-full.txt; do
  tmp="$(mktemp)"
  curl -fsS -m 60 -o "$tmp" "https://connecttrade.com/$f"
  [ -s "$tmp" ] || { echo "empty download: $f" >&2; rm -f "$tmp"; exit 2; }
  mv "$tmp" "$f"
done
python3 -c 'import json; json.load(open("openapi.json"))' || { echo "openapi.json is not valid JSON" >&2; exit 2; }
sha256sum openapi.json llms.txt llms-full.txt > SHA256SUMS
cd ..
cargo xtask codegen
if git diff --quiet -- spec crates/connecttrade/src/generated; then
  echo "Connect Trade's published spec is unchanged."
  exit 0
fi
echo "Connect Trade's published spec changed:"
git --no-pager diff --stat -- spec crates/connecttrade/src/generated
exit 1
