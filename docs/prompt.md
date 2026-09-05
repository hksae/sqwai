# System prompt

The model-facing system prompt lives in [`src/prompts/system.md`](../src/prompts/system.md) and is embedded at compile time. A user-level `system.md` in the config directory may override it.

## Design requirements

The prompt follows DESIGN.md §6: one statement per rule, no incident-specific examples or magic thresholds, project-specific development rules in `AGENTS.md`, and host-generated blocks described as facts.

The tool list is derived from the registry in `src/agent/tools/mod.rs`; it must not be maintained as a separate hand-written inventory.

## Changelog

- 2026-09-05: Rewrote the prompt around identity, host facts, integrity, tools, safety, style, and recovery. Removed duplicated operating sections, guessing examples, incident-specific limits, postponed-work notes, and sqwai development rules.
- 2026-09-05: Added registry-level tool-name coverage assertion and documented the prompt source.
