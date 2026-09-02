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
- MCP client support for stdio and streamable HTTP transports, with tool discovery and namespaced tool calls.
- LSP client foundation for initialization, file synchronization, and queued diagnostics.
- Compatible Skills loader with frontmatter, project overrides, configured directories, and prompt injection.
- Category-based settings hub with reusable Appearance, Themes, Providers, MCP, LSP, and Skills sections.
- Animated themes: `lava`, `gum`, `bloom`, and `neon`.
- Static themes including `white`, with direct RGB animation for endpoint-controlled color transitions.
- Markdown rendering with fenced-code highlighting, tables, headings, and inline styles.
- Safety approval flow for potentially dangerous commands.
- Multiple color themes and configurable thinking levels.

## Requirements

- Rust toolchain with Cargo.
- A configured model provider and API key.
- Git is recommended for checkpoints and undo.

## Build from source

```bash
git clone https://github.com/hksae/sqwai
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

Use `/settings` as the main category hub. Existing shortcuts such as `/providers`, `/models`, `/themes`, and `/debug` remain available. The `Appearance` section reuses the existing Themes menu instead of creating a second theme picker.

### MCP

MCP servers are configured under the `mcp` section. Enabled stdio and streamable HTTP servers are connected asynchronously at the start of an agent turn. Their tools are discovered and exposed to the model with names such as `mcp__github__list_issues`. Stdio servers support command arguments and environment variables; HTTP servers support custom headers.

### LSP

LSP server definitions are configured under `lsp`. The client supports JSON-RPC framing, initialization, `didOpen`, `didChange`, `didSave`, and queued `textDocument/publishDiagnostics` notifications. Runtime integration with file mutation events is still being completed.

### Skills

Skills use the compatible `SKILL.md` format with frontmatter fields such as `name`, `description`, and `triggers`. Skills can be loaded from configured directories, the user skills directory, and `.sqwai/skills` in the project. Later project-specific definitions override earlier definitions, and loaded skills are added to the agent's stable prompt.

## Run

```bash
sqwai
```

A normal launch opens the startup screen without creating an empty session. To resume a known session directly:

```bash
sqwai --resume <session-id>
```

## Safety and undo

Potentially dangerous shell commands can require explicit approval. File changes are checkpointed through git when possible, allowing recent changes to be reverted with `/undo`.

Review commands and diffs before accepting them. Keep credentials in environment variables or local configuration that is excluded from version control.

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
