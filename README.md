# agentvc

Version control built for AI agents. Every agent attempt runs in a cheap
copy-on-write view, failed and competing attempts stay as queryable history,
and a review UI lets a human compare attempts side by side, pick the winner
and ship it as a clean GitHub PR.

Status: early. Milestone M1 (core model and views) is in progress.

- Design: [docs/design.md](docs/design.md)
- Core library: [crates/agentvc-core](crates/agentvc-core)
- CLI: [crates/agentvc-cli](crates/agentvc-cli)
- Review UI: [ui](ui)

## Build

```sh
cargo build
cargo run -p agentvc-cli -- --help
cargo test --workspace
```
