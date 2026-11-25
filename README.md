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

## Try it

```sh
cargo build --release
alias agentvc=$PWD/target/release/agentvc

cd /path/to/your/repo
agentvc init                                  # snapshot the working dir as attempt a1
agentvc task new "make the parser 2x faster"  # -> t1
agentvc fork --task t1 --actor agent:claude   # -> a2 in .agentvc/views/a2, build output included
agentvc fork --task t1 --actor agent:codex    # -> a3, a sibling attempt
cd "$(agentvc path a2)" && ...                # let an agent work there
agentvc snapshot a2 --meta tests=pass         # record state with metadata
agentvc diff a2 a3                            # files that differ between two attempts
agentvc promote a2                            # pick the winner; a3 becomes abandoned (lost to a2)
agentvc log --task t1                         # the attempt tree
agentvc export a2 --branch parser-2x          # a2 as a commit on a branch; worktree untouched
agentvc pr a2 --draft                         # push agentvc/a2 and open a PR listing rejected attempts
```

`agentvc pr` reads a token from `GITHUB_TOKEN`, `GH_TOKEN` or `gh auth token`.
Record `tests` and `cost` with `snapshot --meta` and they show up in the PR body.

Every command takes `--json` and returns one JSON object; errors carry a
stable `error.code`. Benchmark forks against `git worktree` with
`scripts/bench-fork.sh <repo>`.

## Build

```sh
cargo build
cargo run -p agentvc-cli -- --help
cargo test --workspace
```
