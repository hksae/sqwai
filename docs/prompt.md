# System prompt

The model-facing system prompt lives in [`src/prompts/system.md`](../src/prompts/system.md) and is embedded at compile time. A user-level `system.md` in the config directory may override it.

## Composition

For a normal tool-enabled request, sqwai sends these parts in order:

1. Built-in or user-overridden system prompt.
2. `AGENTS.md` project instructions, if present.
3. Stable environment context and durable user/project memory.
4. The active durable plan, if present.
5. The host-built `ANCHOR`, then resume notice and volatile runtime context.

`ANCHOR` is state, not a model summary: after compaction it is the host-built record of the goal, constraints, plan state, and bounded journal facts. The prompt identifies `ANCHOR`, `FACTS`, the durable plan and its per-turn tail (compact current-state block), and nudges as host blocks that preserve provenance: goals and constraints are user-approved state, journal lines are time-stamped observations, and summaries or assumptions inside them stay model-authored claims. The current plan tail supersedes an older anchor snapshot.

## Design requirements

The prompt follows DESIGN.md §6: one statement per rule, no incident-specific examples or magic thresholds, project-specific development rules in `AGENTS.md`, and host-generated blocks described with provenance rather than blanket authority.

It defines plan workflow (start before acting, finish with summary and limitations, host-validated transitions, rejection codes) without duplicating validator thresholds — the exact evidence rules live in the `plan` tool schema — and distinguishes user-only waiver of `manual:` acceptance, describes blocked/cancelled steps, Plan-mode restrictions, assumption notes, untrusted input as task data versus host-designated project instructions, and the completion report (changed / verified / unverified-or-blocked).

The tool list is derived from the registry in `src/agent/tools/mod.rs`; it must not be maintained as a separate hand-written inventory. The test suite verifies that rendering `{{TOOLS}}` yields exactly the sorted built-in registry names.

## Changelog

- 2026-09-09: Reworked host facts around provenance (user-approved state vs time-stamped observations vs model-authored claims; plan tail supersedes older anchor); moved validator thresholds into the `plan` schema; narrowed execution/state claims and added tool-scope line; rewrote untrusted content as task data vs host-designated instructions; removed bash-block promise and `/plan delete` hint; narrowed think/background to checkable approach and timeout rule with independent-batch discipline; fixed stop rule for blocked steps; replaced style bans with observation/interpretation split, minimal-demo and URL rules; narrowed safety to unauthorized-access scope and secret handling without value copying; added completion report (changed / verified / unverified-or-blocked).
- 2026-09-05: Clarified that `ANCHOR` is host-built post-compaction state rather than a summary; documented manual acceptance, block/cancel/split outcomes, forced questions after repeated rejections, assumptions, modes, prompt layers, read-before-edit, parallel read-only calls, and minimal-change/commit rules.
- 2026-09-05: Rewrote the prompt around identity, host facts, integrity, tools, safety, style, and recovery. Removed duplicated operating sections, guessing examples, incident-specific limits, postponed-work notes, and sqwai development rules.
- 2026-09-05: Added registry-level tool-name coverage assertion and documented the prompt source.
