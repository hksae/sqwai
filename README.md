# sqwai

A terminal coding agent for working inside a repository. It runs a model in a
full-screen terminal UI, lets it inspect and change files, run commands, keep a
plan, and undo its work. This is an early 0.1.0 release.

MCP server support is not included in 0.1.0, and neither is the multi-agent
orchestrator.

## Build

Requires Rust 1.98 (the repository pins the toolchain).

```bash
cargo build --release --locked
./target/release/sqwai
```

Run `sqwai` from the project directory you want it to work in.

## License

Apache-2.0. See [LICENSE](LICENSE).
