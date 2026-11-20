# Agents working in connecttrade-rs

Read [docs/coding-standards.md](docs/coding-standards.md) first: the JSF-inspired lint profile, the large-file rules and the `may`-runtime bans all fail CI.

- Run `pre-commit run --all-files --hook-stage pre-push` before pushing; it runs what CI runs.
- Never edit `crates/connecttrade/src/generated/`. Change `spec/overlay.json` or `xtask/`, then `cargo xtask codegen`.
- Never edit `spec/openapi.json`, `spec/llms.txt` or `spec/llms-full.txt`: they are Connect Trade's, as published. `scripts/refresh-spec.sh` updates them.
- Behaviour the documents do not settle is marked `ASSUMPTION` in `crates/ct-mock/src/brokers.rs`. When a paper account disagrees, correct the mock and say so in the commit.
- Every `#[allow]` carries a `reason`. A new one needs a reason a reviewer would accept.
- Commit messages follow Conventional Commits.
