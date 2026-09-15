![sqwai](assets/sqwai_header.png)

A terminal coding agent built for long tasks. sqwai keeps the goal fixed across
context compaction, requires evidence before a step can be closed, and answers
"what did you do?" from a record of what actually happened — not from memory.

Written in Rust. Single binary. Works with Anthropic, OpenAI, OpenAI-compatible
endpoints, and local models.

> **Status.** Under active development. Unmarked features work today; anything
> marked **[in development]** is specified in [DESIGN.md](DESIGN.md) and not
> built yet. The work queue in DESIGN.md §7 is authoritative.

## Quick start

```bash
cargo install --git https://github.com/hksae/sqwai
cd your-project
sqwai
```

The first run writes a config template and exits. Run again, press `ctrl+p`
and pick a provider and model — the catalog ships built in and updates
itself. Then export the key and go:

```bash
export ANTHROPIC_API_KEY=...
```

Keys resolve from `<PROVIDER>_API_KEY` unless the config says otherwise.
`/init` creates `.sqwai/` and a starter `AGENTS.md`.

## What's different

- **Host-owned goal and constraints.** The model proposes, you decide. The
  goal cannot be rewritten mid-task by the agent.
- **Evidence-gated steps.** A plan step closes only on host-recorded proof:
  a diff or a passing command — never on assertion.
- **Compaction from state, not summary.** After context compaction the agent
  resumes from goal, plan, facts and open notes instead of a retold story.
- **Answers from the journal.** History, undo, diary and disputes read an
  append-only event log of what actually happened.

How each of these works is in [DESIGN.md](DESIGN.md).

## Project layout

```
.sqwai/
  plans/      structured plans                 ignored
  journal/    event logs                       ignored
  memory/     diary and MEMORY.md              your choice (default ignored)
  graph/      index                            ignored, rebuildable
  skills/     project skills                   committed
  config.toml project overrides                committed if present
AGENTS.md     project instructions             committed
```

File tools cannot reach `.sqwai/` except `skills/` and `config.toml` —
"host-written" and "append-only" hold by construction, not by request.

## Development

```bash
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test
```

## License

[Apache License 2.0](LICENSE).
