![sqwai](assets/sqwai_header.png)



A terminal coding agent built for long tasks. sqwai keeps the goal fixed across
context compaction, requires evidence before a step can be closed, and answers
"what did you do?" from a record of what actually happened — not from memory.

Written in Rust. Single binary. Works with Anthropic, OpenAI, OpenAI-compatible
endpoints, and local models.

> **Status.** sqwai is under active development. Everything below without a
> marker works today; anything marked **[in development]** is specified in
> [DESIGN.md](DESIGN.md) and not built yet. The work queue in DESIGN.md §7 is
> the authoritative list of what is done and what is next.

## Why

Every coding agent degrades the same way on a long task: the plan is prose the
model maintains by good will, progress is whatever the model says it is,
compaction replaces history with a model-written summary that inherits every
error, and criticism is answered by arguing. sqwai replaces good will with
structure enforced by the host, not by the prompt.

| Failure | sqwai |
|---|---|
| Goal drifts or gets rewritten | Goal and constraints are host-owned; the model can only propose a change, the user decides |
| Steps closed by assertion | A step cannot be finished without evidence recorded by the host: a diff or a passing command |
| Compaction loses the thread | The post-compaction context is assembled from structured state — goal, plan, facts, open notes — not from a summary |
| Memory full of fabricated facts | Diary entries get their numbers and paths from the journal; the model adds the reasoning |
| Edits to code that does not exist | **[in development]** A project graph resolves every referenced symbol before the step starts |
| "You broke it" answered by arguing | **[in development]** Criticism triggers a fact block from the journal and, when needed, a blinded verification pipeline |

## Mechanisms

**Plan.** A structured plan with operations (`start`, `finish`, `block`,
`split`, `verify`, `complete`) validated by code. `finish` requires evidence
recorded by the host. Rejections come with a reason and a hint. The goal
changes only via `/goal`. Executable acceptance criteria — the host running a
`cmd:` item itself and attaching the result — are **[in development]**.

**Journal.** An append-only event log written by the host at tool dispatch:
calls, results, diffs, checkpoints, approvals, compactions. The model has one
labeled write path, `note`. The journal is what the plan validator and the
diary read.

**Memory.** A daily diary in `.sqwai/memory/` with host-inserted facts and
model-written decisions, rejected approaches, and corrections. A curated
`MEMORY.md` for durable project facts, updated only with user approval.
Another model, another day, another session picks up exactly where things
stopped.

**Undo.** A snapshot of the worktree before every mutating action, taken as a
dangling commit that leaves your branches, `HEAD` and staging area untouched.
`/undo [n]` restores the files and reopens the plan steps whose evidence was
reverted. Requires the project to be a git repository. The two-layer scheme
from DESIGN.md §2.5 — a content-addressed per-file blob store plus a separate
shadow repository, so undo also works outside git — is **[in development]**,
and so is `/undo step N`.

**Graph. [in development]** An index of files, symbols, documents and memory,
with `resolve_ref` as a fact rather than a suggestion: plan steps and edits
that reference unknown symbols get rejected with candidates. What exists today
is a prototype that indexes files and Markdown structure and is not yet
exposed to the model; `/graph-rebuild` rebuilds it.

**Reflector. [in development]** On criticism, the host injects journal facts
before the model answers. If the claim is checkable and contradicts the
journal, a read-only executor that never sees the criticism runs a bounded set
of checks, and code computes the verdict.

## Also included

Plan and Act modes · streaming with collapsible tool activity and thinking ·
prompt caching with a stable prefix · two-layer dangerous-command classifier
with approval dialogs, including PowerShell and cmd on Windows · subagents
that inherit the current mode · MCP client (stdio and streamable HTTP) ·
`SKILL.md` skills compatible with existing skill packs · sessions with resume
and fork · themes · a `/settings` hub. LSP diagnostics are collected;
feeding them back into the plan is **[in development]**.

## Install

From source:

```bash
cargo install --git https://github.com/hksae/sqwai
```

Prebuilt binaries are not published yet.

The git tools (`git_status`, `git_diff`, `git_log`, `git_commit`,
`git_branch`) shell out to `git`, so they need it on `PATH`. Everything else,
including checkpoints, works without it — but checkpoints do need the project
to be a git repository.

## Quick start

```bash
cd your-project
sqwai
```

The first run has no config, so sqwai writes a template and exits. The template
already contains working providers — Anthropic, OpenAI and Gemini — with their
endpoints and the environment variable each key is read from. Export the one you
use:

```bash
export ANTHROPIC_API_KEY=...
```

Then run `sqwai` again and press `ctrl+p` to pick a model. `/init` creates
`.sqwai/` and a starter `AGENTS.md`.

The config lives at `~/.config/sqwai/config.toml`
(macOS: `~/Library/Application Support/sqwai/config.toml`,
Windows: `%APPDATA%\sqwai\config\config.toml`). Add an endpoint the template
does not cover by hand:

```toml
default_model = "sonnet"

[providers.anthropic]
format = "anthropic"                        # anthropic | openai | responses
base_url = "https://api.anthropic.com"
api_key_env = "ANTHROPIC_API_KEY"           # optional: <PROVIDER>_API_KEY by default

[models.sonnet]
provider = "anthropic"
id = "claude-sonnet-5"
context = 1000000
thinking = "high"                           # off | low | medium | high | max
```

Local models: point `base_url` at the server, set `format = "openai"` and leave
`api_key_env` out.

```bash
sqwai bench
```
**[in development]** — the goal-retention benchmark from DESIGN.md §8.2: forced
compactions, goal fidelity, redundant work, fabricated references and total
tokens, against a baseline with the mechanisms disabled.

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

File tools cannot reach `.sqwai/` except `skills/` and `config.toml`; plan,
journal and memory are modified only through their own tools. That is what
makes "host-written" and "append-only" guarantees rather than requests.

## Design

The full design — state layers, validator rules, compaction anchor, reflector
pipeline, graph model, benchmark, and rejected alternatives — is in
[DESIGN.md](DESIGN.md). Section 7 carries the work queue and the status of every
item.

## Development

```bash
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test
```

CI runs all three on Linux, macOS and Windows.

## License

[Apache License 2.0](LICENSE).
