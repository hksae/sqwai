# sqwai

An interactive coding agent for the terminal, written in Rust.

sqwai runs an AI coding agent directly inside your project directory. It connects an LLM to a practical terminal UI, project files, shell commands, sessions, safety checks, and development tools.


## Features

- Edit and inspect project files from an interactive terminal UI.
- Run shell commands through the agent.
- Stream model responses and tool activity.
- Separate Plan and Act modes.
- Save, resume, switch, fork, rename, pin, and delete sessions.
- Create git checkpoints before changes and undo recent changes.
- Provider abstraction for Anthropic, OpenAI-compatible APIs, and OpenAI Responses.
- MCP and LSP integration points.
- Project instructions through `AGENTS.md`.
- Markdown rendering with fenced-code highlighting, tables, headings, and inline styles.
- Safety approval flow for potentially dangerous commands.
- Multiple color themes and configurable thinking levels.

## Requirements

- Rust toolchain with Cargo.
- A configured model provider and API key.
- Git is recommended for checkpoints and undo.

## Build from source

```bash
git clone <repository-url>
cd sqwai
cargo build --release
```

The executable is created at `target/release/sqwai` (or `target/release/sqwai.exe` on Windows).

## Installation

### Windows

```powershell
powershell -ExecutionPolicy Bypass -File .\install.ps1
```

### Linux and macOS

```bash
bash ./install.sh
```

The installers build the release binary and place it in the user's local executable directory.

## Configuration

sqwai reads its configuration from the platform-specific application configuration directory. On Windows the default path is:

```text
%APPDATA%\sqwai\config\config.toml
```

Configure a provider, model, endpoint, and API key environment variable in that file. Do not commit API keys or other credentials. Use `/providers`, `/models`, and `/debug` inside sqwai to inspect runtime configuration.

If a project contains `AGENTS.md`, sqwai loads it as project-specific instructions for the agent. Use `/init` to create a starter file.

## Run

```bash
sqwai
```

A normal launch opens the startup screen without creating an empty session. To resume a known session directly:

```bash
sqwai --resume <session-id>
```

## Startup screen

```text
Enter  open sessions
n      create a new session
q      quit
```

If there are no saved sessions, Enter opens an empty sessions menu. Press `n` to start working immediately.

## Chat controls

```text
Enter       send the current message
Tab         switch between Plan and Act
Esc         stop generation or close the active menu
Ctrl+S      open sessions
Ctrl+T      open the todo panel
```

Click a completed tool call to expand its output or diff. Drag across chat content to copy a selection.

## Commands

```text
/help              commands, symbols, and keybindings
/new               start a new session
/sessions          open the sessions menu
/fork              fork the current session
/providers         open providers and models
/models            list models for the current provider
/plan              switch to Plan mode
/act               switch to Act mode
/undo [n]          revert the last change or last n changes
/init              create AGENTS.md
/themes            browse color themes
/debug             open runtime settings
/exit              quit sqwai
```

## Safety and undo

Potentially dangerous shell commands can require explicit approval. File changes are checkpointed through git when possible, allowing recent changes to be reverted with `/undo`.

Review commands and diffs before accepting them. Keep credentials in environment variables or local configuration that is excluded from version control.

## Architecture

The main execution flow is:

```text
provider -> agent loop -> tools -> TUI
```

The source tree is organized into configuration, providers, agent execution, tools, sessions, planning, undo, graph, and TUI modules.

## Development

```bash
cargo fmt --check
cargo check
cargo test
cargo build --release
```

When changing the TUI, check keyboard and mouse paths, empty and populated states, resizing, scrolling, wrapping, overlays, and transitions between startup, new, resumed, and switched sessions.

## Roadmap

The project is being developed in phases:

- Foundation and terminal UI.
- Agent loop and provider integration.
- Tools, safety, and reliable execution.
- Sessions, checkpoints, and undo.
- Knowledge graph and persistent project memory.
- MCP, LSP, and skills.
- Stabilization, polish, and release packaging.

## License

Licensed under the [Apache License 2.0](LICENSE).
