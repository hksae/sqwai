![sqwai](assets/sqwai_header.png)



A terminal coding agent built for long tasks. sqwai keeps the goal fixed across
context compaction, requires evidence before a step can be closed, and answers
"what did you do?" from a record of what actually happened — not from memory.

Written in Rust. Single binary. Works with Anthropic, OpenAI, OpenAI-compatible
endpoints, and local models.

## Why

Every coding agent degrades the same way on a long task: the plan is prose the
model maintains by good will, progress is whatever the model says it is,
compaction replaces history with a model-written summary that inherits every
error, and criticism is answered by arguing. sqwai replaces good will with
structure enforced by the host, not by the prompt.

| Failure | sqwai |
|---|---|
| Goal drifts or gets rewritten | Goal and constraints are host-owned; the model can only propose a change, the user decides |
| Steps closed by assertion | A step cannot be finished without evidence recorded by the host: a diff, a passing command, clean diagnostics |
| Compaction loses the thread | The post-compaction context is assembled from structured state — goal, plan, facts, open notes — not from a summary |
| Memory full of fabricated facts | Diary entries get their numbers and paths from the journal; the model adds the reasoning |
| Edits to code that does not exist | A project graph resolves every referenced symbol before the step starts |
| "You broke it" answered by arguing | Criticism triggers a fact block from the journal and, when needed, a blinded verification pipeline |

## Mechanisms

**Plan.** A structured plan with operations (`start`, `finish`, `block`,
`split`, `verify`, `complete`) validated by code. `finish` requires evidence;
`complete` requires every acceptance criterion verified. Rejections come with a
reason and a hint. The goal changes only via `/goal`.

**Journal.** An append-only event log written by the host at tool dispatch:
calls, results, diffs, checkpoints, approvals, diagnostics, compactions. The
model has one labeled write path, `note`. The journal is what the plan
validator, the diary, and the reflector read.

**Memory.** A daily diary in `.sqwai/memory/` with host-inserted facts and
model-written decisions, rejected approaches, and corrections. A curated
`MEMORY.md` for durable project facts, updated only with user approval.
Another model, another day, another session picks up exactly where things
stopped.

**Graph.** A SQLite index of files, symbols, documents, and memory built with
tree-sitter. `resolve_ref` is a fact, not a suggestion: plan steps and edits
that reference unknown symbols are rejected with candidates. Stale memory is
marked as stale. Explore it with `Ctrl+G`.

**Reflector.** On criticism, the host injects journal facts before the model
answers. If the claim is checkable and contradicts the journal, a read-only
executor that never sees the criticism runs a bounded set of checks, and code
computes the verdict. The user sees `[verified] …` and can inspect every check
with `/verify --full`.

**Undo.** Per-file snapshots before every edit and tree snapshots around every
shell command, in a shadow repository that never touches your `.git`. `/undo`
restores files and reopens the plan steps whose evidence was reverted.
`/undo step 3` reverts one step.

## Also included

Plan and Act modes · streaming with collapsible tool activity and thinking ·
prompt caching with a stable prefix · two-layer dangerous-command classifier
with approval dialogs · subagents that inherit the current mode · MCP client
(stdio and streamable HTTP) · LSP diagnostics fed back to the agent and the
plan · `SKILL.md` skills compatible with existing skill packs · sessions with
resume and fork · themes · a `/settings` hub.

## Install

Prebuilt binaries for Linux, macOS, and Windows are on the
[releases page](https://github.com/hksae/sqwai/releases).

From source:

```bash
cargo install --git https://github.com/hksae/sqwai
```

Requires git on PATH for shell-command checkpoints. Everything else works
without it.

## Quick start

# ~/.config/sqwai/config.toml   (Windows: %APPDATA%\sqwai\config\config.toml)
```toml
default_model = "sonnet"

[providers.anthropic]
preset = "anthropic"
api_key_env = "ANTHROPIC_API_KEY"

[models.sonnet]
provider = "anthropic"
id = "claude-sonnet-4-5"
context = 200000
thinking = true
```

```bash
cd your-project
sqwai            # /init on first run creates .sqwai/ and a starter AGENTS.md
```

Local models: set preset = "ollama" (or format = "openai" with a
base_url) and leave api_key_env empty.

```bash
sqwai bench
```
runs the goal-retention benchmark on your repository with your
model: forced compactions, goal fidelity, redundant work, fabricated references,
total tokens — against a baseline with the mechanisms disabled.

## Project layout

```
.sqwai/
  plans/      structured plans                 ignored
  journal/    event logs and verdicts          ignored
  memory/     diary and MEMORY.md              your choice (default ignored)
  graph/      SQLite index                     ignored, rebuildable
  skills/     project skills                   committed
AGENTS.md     project instructions             committed
```

File tools cannot reach .sqwai/ except skills/; plan, journal, and memory
are modified only through their own tools. That is what makes "host-written"
and "append-only" guarantees rather than requests.

## Design

The full design — state layers, validator rules, compaction anchor, reflector
pipeline, graph model, benchmark, and rejected alternatives — is in
[DESIGN.md](DESIGN.md).

## Development

```bash
cargo fmt --check && cargo clippy -- -D warnings && cargo test
```

## License

[Apache License 2.0](LICENSE).
