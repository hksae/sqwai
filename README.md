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

**Assumptions.** A `note` of kind `assumption` stays open until another note
closes it by sequence number. Finishing a step that still carries one succeeds
with a warning naming it, and the compaction anchor and the diary carry open
assumptions forward — so an assumption cannot quietly outlive the work that
depended on it.

**Journal.** An append-only event log written by the host at tool dispatch:
calls, results, diffs, checkpoints, approvals, compactions. The model has one
labeled write path, `note`. The journal is what the plan validator and the
diary read.

**Memory.** A daily diary in `.sqwai/memory/` with host-inserted facts and
model-written decisions, rejected approaches, and corrections. A curated
`MEMORY.md` for durable project facts, updated only with user approval.
Another model, another day, another session picks up exactly where things
stopped.

**Undo.** Every file the agent is about to change is kept first, byte for
byte, in a content-addressed store under `.sqwai/checkpoints/blobs/` — so
`/undo [n]` puts back exactly what the agent wrote and **does not need git at
all**. A file you edited yourself in the meantime is reported and left alone
rather than overwritten. `/undo step N` reverts one plan step and reopens it, refusing by
name any file a later step has rewritten since. Undoing a `bash` command needs a worktree
snapshot, which is taken in a shadow repository of sqwai's own under
`.sqwai/checkpoints/git` — your `.git` is never written to, so your branches,
index, hooks and `git gc` are unaffected, and snapshots work in a project
that is not a repository at all. It needs the `git` binary; without it,
file reverts still work and `bash` runs uninsured. Both layers are pruned when
a session ends: pre-images no journal references are removed after
`[undo].blob_grace_secs`, and a finished session's snapshot chain is dropped
and collected.

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
`git_branch`) shell out to `git`, so they need it on `PATH`, and so do the
worktree snapshots taken around `bash`. Neither needs the project to *be* a
repository, and file reverts need no git at all.

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
continuation = true                         # let the provider continue from its own
                                            # copy of the conversation when the format
                                            # documents a reference for it; set false
                                            # to always resend the transcript

[models.sonnet]
provider = "anthropic"
id = "claude-sonnet-5"
context = 1000000
effort = "high"                             # off | low | medium | high | max
effort_control = "levels"                   # none | toggle | levels | xhigh | budget
                                            # omit it and the wire format decides:
                                            # a token budget on Anthropic, the three
                                            # documented levels elsewhere. Declare
                                            # "xhigh" for a model that documents it,
                                            # otherwise `max` is sent as `high` and
                                            # the status bar says `ef:max→high`.
effort_always_on = false                    # the model cannot stop reasoning, so
                                            # `off` is reported as ignored
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

## Debugging a provider

`[ui] http_log = true` (or the `http debug log` switch in the debug menu)
writes failed requests, dropped SSE payloads and a per-turn effort line to
`debug.log` in the platform data directory. The effort line reports what was
asked for, what went on the wire, and what the provider counted:

```
turn: effort requested=max sent=Level("high") reasoning_tokens=not reported
turn: effort requested=high sent=Level("high") reasoning_tokens=1088
turn: effort requested=max sent=nothing (parameter refused by the provider) reasoning_tokens=0
```

Every usage event is logged too, which is how an inconsistent gateway becomes
visible:

```
usage event: prompt=30 completion=900 cached=None reasoning=Some(1088)
usage event: prompt=0 completion=900 cached=None reasoning=Some(0)
```

Counters only grow, so the largest value seen in a turn wins — a trailing stub
of zeros cannot erase a real count.

`reasoning_tokens=not reported` and `reasoning_tokens=0` mean different things:
the first is a provider that says nothing about reasoning, the second is a
measurement. Even a zero is only reported after several turns in a row, and it
is worded as what was measured — API relays that translate between protocols
have been seen pinning both `cached` and `reasoning` at zero while the model
was demonstrably working.

Successful request and response bodies are not logged.

### Relays and API proxies

A relay that accepts one protocol and forwards another can drop everything that
is not the answer itself: `reasoning_effort` on the way in, reasoning and cache
counters on the way out. If `cached` stays at 0 across turns with an identical
prompt prefix, prompt caching is not reaching the upstream provider, and the
prefix work in DESIGN.md §3.2 buys nothing there. Check the model's documented
protocol on the relay and configure that `format`, not whichever one happens to
answer.

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
