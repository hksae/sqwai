# Changelog

## 0.1.0 — 2026-11-05

First public release.

### Highlights

- Full-screen terminal UI with a streamed transcript, tool rows, diffs,
  sessions, plan view and context/cost accounting.
- File, shell, web, image and PDF tools with read-before-write protection,
  atomic writes, secret screening and a shell safety classifier.
- Durable project plans, checkpoints and turn-attributed undo/redo.
- Subagents with hidden sessions, their own plans, background reports,
  pause/resume, restart recovery and a four-child limit.
- ASK / AUTO / FULL approval modes and a taint-aware egress gate.
- Provider catalog with OpenAI-compatible, Anthropic and Responses wire
  formats.
- Project and user skills, `AGENTS.md` / `SQWAI.md` instructions, and a
  project-local display/budget config overlay.

### Scope

- MCP server support is not included in 0.1.0.
- The multi-agent orchestrator is not included; subagents are available
  without a nested-agent scheduler.

### Notes

- This is an early release. Configuration and terminal UI details may change
  in later versions.
