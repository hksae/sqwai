# sqwai

A terminal coding agent with a full-screen TUI: talk to a model, let it read and
write files, run commands, keep a durable plan, and undo its changes.

sqwai is an early 0.1.0 release. It is usable for real repository work, but the
interface and configuration may still change.

## Features

- Full-screen TUI with streamed answers, tool rows, diffs, sessions and a
  context/cost view.
- File tools (`read`, `write`, `edit`, `grep`, `glob`, `outline`, `ls`), shell
  commands, background jobs, web search/fetch, image viewing and PDF reading.
- Durable project plans with steps, evidence and checkpoints.
- Undo/redo across file edits and shell mutations, including subagent writes.
- Subagents with their own hidden sessions, plans, pause/resume and restart
  recovery.
- Skills loaded from project or user directories.
- ASK / AUTO / FULL approval modes, a shell safety classifier, secret
  screening and a taint-aware egress gate.
- Provider catalog with OpenAI-compatible, Anthropic and Responses wire
  formats.

## Install

### Windows release

Download the `sqwai-v0.1.0-x86_64-pc-windows-msvc.zip` asset from the
[Releases](https://github.com/hksae/sqwai/releases) page, unpack it, and run
`sqwai.exe` from a project directory.

### From source

Requires Rust 1.98 (the repository pins the toolchain) and Git.

```bash
git clone https://github.com/hksae/sqwai
cd sqwai
./install.sh                 # Unix-like; installs to ~/.cargo/bin
```

```powershell
git clone https://github.com/hksae/sqwai
cd sqwai
powershell -ExecutionPolicy Bypass -File .\install.ps1
```

Or build without installing:

```bash
cargo build --release --locked
```

## Quick start

```bash
cd your-project
sqwai
```

Inside the TUI:

- `/init` creates `AGENTS.md` and `SQWAI.md` project instructions.
- `/providers` and `/models` configure the endpoint and model.
- `/settings` opens the rest of the configuration.
- `/context` shows what is consuming the context window.
- `/undo` reverts the last turn; `/help` lists the full keyboard and command
  surface.

Provider API keys are stored in the host-only key store, not in the project
config. A project-local `.sqwai/config.toml` can tune display and budget
settings, but cannot redefine providers, models, keys or safety settings.

## Scope of 0.1.0

MCP server support is not included in this release. The multi-agent
orchestrator is also not part of 0.1.0; subagents are available, but there is
no nested-agent scheduler.

## Security

sqwai runs shell commands and changes files. Review commands in ASK mode, and
use FULL only for work you are prepared to let the agent perform without
confirmation. See [SECURITY.md](SECURITY.md) for the threat model and the
private vulnerability reporting process.

## License

Apache-2.0. See [LICENSE](LICENSE).
