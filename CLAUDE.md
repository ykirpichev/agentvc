# agentvc

Agent-first version control. Read `docs/design.md` before larger changes.

- Rust cargo workspace under `crates/`; review UI (React + TypeScript) under `ui/`.
- Before pushing: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
- Substantive changes go through PRs, not straight to `main`.
- Keep the CLI non-interactive: every command supports `--json`, errors carry stable codes.
