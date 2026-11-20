# Connect Trade's spec

| File | What it is |
| --- | --- |
| `openapi.json` | Connect Trade's OpenAPI spec, exactly as published at https://connecttrade.com/openapi.json. Never edited. |
| `llms.txt`, `llms-full.txt` | Connect Trade's own documentation for AI tools, as published at https://connecttrade.com/llms.txt and https://connecttrade.com/llms-full.txt. The Streaming Broker API (not in the spec) and the broker capability matrix are described here. |
| `SHA256SUMS` | Hashes of the three, written by `scripts/refresh-spec.sh`. |
| `overlay.json` | **Ours.** Everything we add to or correct in the spec for code generation, each with its reason. |

These three files are Connect Trade's work, included so the client is built from a known version. Connect Trade, Inc. publishes them; the licence of this repository covers our code, not them.

## The overlay

`cargo xtask codegen` reads `openapi.json` and `overlay.json` and writes `crates/connecttrade/src/generated/`. The overlay:

- names every operation (the spec has no `operationId`s);
- moves `DELETE /users`'s `user_id` to the query (the spec declares it as a path parameter of a path without one);
- merges the spec's four error schemas into one;
- names inline objects (fills, legs, connection accounts), and uses the documented shape where an array's item schema is missing;
- names inline enums that must stay narrower than the merged ones;
- makes response fields optional except identifiers, because the spec's `required` lists do not match Connect Trade's own examples;
- corrects two request rules that contradict the docs: a multi-leg order sends `"symbol": null` and leaves out `side`.

## Refreshing

```
scripts/refresh-spec.sh
```

It re-fetches the three files, regenerates the client, and exits non-zero when anything changed. CI runs it every Monday; a red `spec-drift` run means Connect Trade published a new spec.
