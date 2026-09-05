# System prompt

The model-facing system prompt lives in [`src/prompts/system.md`](../src/prompts/system.md) and is embedded at compile time. A user-level `system.md` in the config directory may override it.

## Composition

For a normal tool-enabled request, sqwai sends these parts in order:

1. Built-in or user-overridden system prompt.
2. `AGENTS.md` project instructions, if present.
3. Stable environment context and durable user/project memory.
4. The active durable plan, if present.
5. The host-built `ANCHOR`, then resume notice and volatile runtime context.

`ANCHOR` is state, not a model summary: after compaction it is the authoritative record of the goal, constraints, plan state, and bounded journal facts. The prompt identifies `ANCHOR`, `FACTS`, the durable plan, and nudges as host facts.

## Design requirements

The prompt follows DESIGN.md §6: one statement per rule, no incident-specific examples or magic thresholds, project-specific development rules in `AGENTS.md`, and host-generated blocks described as facts.

It defines plan evidence by step kind, distinguishes user-only waiver of `manual:` acceptance, describes blocked/cancelled steps, Plan-mode restrictions, assumption notes, untrusted input, and truthful reporting.

The tool list is derived from the registry in `src/agent/tools/mod.rs`; it must not be maintained as a separate hand-written inventory. The test suite verifies that rendering `{{TOOLS}}` yields exactly the sorted built-in registry names.

## Changelog

- 2026-09-05: Clarified that `ANCHOR` is host-built post-compaction state rather than a summary; documented manual acceptance, block/cancel/split outcomes, forced questions after repeated rejections, assumptions, modes, prompt layers, read-before-edit, parallel read-only calls, and minimal-change/commit rules.
- 2026-09-05: Rewrote the prompt around identity, host facts, integrity, tools, safety, style, and recovery. Removed duplicated operating sections, guessing examples, incident-specific limits, postponed-work notes, and sqwai development rules.
- 2026-09-05: Added registry-level tool-name coverage assertion and documented the prompt source.
