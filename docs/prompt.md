# System prompt

The model-facing system prompt lives in [`src/prompts/system.md`](../src/prompts/system.md) and is embedded at compile time. A user-level `system.md` in the config directory may override it.

## Composition

For a normal tool-enabled request, sqwai sends these parts in order:

1. Built-in or user-overridden system prompt.
2. `AGENTS.md` project instructions, if present.
3. Stable environment context and durable user/project memory.
4. The active durable plan, if present.
5. The volatile host block: mode line, verify-command names, the `ANCHOR` when the history has been compacted, resume and stopped-turn notices, the step-state plan tail, and the plan nudge.

The volatile parts are joined into one host block that trails the history as a `system` item, inserted before the newest user turn so the last thing in a request is the human's words or a tool result. It used to ride as a `user` item at the very end of every request, including every iteration of a tool loop, and a live session answered that block instead of the user (fixed in B10, `0b7c14f`).

`ANCHOR` is state, not a model summary: journal-derived facts about this session — files it changed, what was last verified, open assumptions, decisions, recent failures. It carries no goal or plan content, because those ride live on every request and a snapshot would resurrect the two-plan problem. It appears only where the transcript has lost detail, after a compaction or on a restored session. The prompt identifies `ANCHOR`, the durable plan and its per-turn tail (compact current-state block), the on-demand `journal` tool, and nudges as host blocks that preserve provenance: goals and constraints are user-approved state, journal lines are time-stamped observations, and summaries or assumptions inside them stay model-authored claims. The current plan tail supersedes an older anchor snapshot. Blocks state facts; the rules built on them — do not re-read an active plan, do not recreate one, no plan block means no active plan, a request that is not about the plan gets answered — live in `system.md`, stated once.

## Design requirements

The prompt follows the design rules in [`../DESIGN.md`](../DESIGN.md) (§1 principles, §3 what the model sees, in order): one statement per rule, no incident-specific examples or magic thresholds, project-specific development rules in `AGENTS.md`, and host-generated blocks described with provenance rather than blanket authority.

It defines plan workflow (start before acting, finish with summary and limitations, host-validated transitions, rejection codes) without duplicating validator thresholds — the exact evidence rules live in the `plan` tool schema — and distinguishes user-only waiver of `manual:` acceptance, describes blocked/cancelled steps, Plan-mode restrictions, assumption notes, untrusted input as task data versus host-designated project instructions, and the completion report (changed / verified / unverified-or-blocked).

The tool list is derived from the registry in `src/agent/tools/mod.rs`; it must not be maintained as a separate hand-written inventory. The test suite verifies that rendering `{{TOOLS}}` yields exactly the sorted built-in registry names.

## Changelog

- 2026-09-26: Reordered system prompt (identity, how to work, plans+evidence, claims, host context, tools, safety, style); How-to-work section added; Reporting/Prompt layers/Recovery folded in; tool mechanics moved to schemas ({{TOOLS}} list and bash/journal/read-before-edit paragraphs removed from the prompt); negations kept only for true prohibitions.
- 2026-09-26: Fixed prompt-code contradictions (goal/constraint changes only via propose_plan/propose_reset/add-split; tool schemas identical in both modes; dead FACTS block replaced by journal; strict-mode clause removed; plan-first gate documented with exact semantics); added Mode: plan/act and Checks: verify-commands volatile lines; toolchains probed from project manifests.
- 2026-09-09: Reworked host facts around provenance (user-approved state vs time-stamped observations vs model-authored claims; plan tail supersedes older anchor); moved validator thresholds into the `plan` schema; narrowed execution/state claims and added tool-scope line; rewrote untrusted content as task data vs host-designated instructions; removed bash-block promise and `/plan delete` hint; narrowed think/background to checkable approach and timeout rule with independent-batch discipline; fixed stop rule for blocked steps; replaced style bans with observation/interpretation split, minimal-demo and URL rules; narrowed safety to unauthorized-access scope and secret handling without value copying; added completion report (changed / verified / unverified-or-blocked).
- 2026-09-05: Clarified that `ANCHOR` is host-built post-compaction state rather than a summary; documented manual acceptance, block/cancel/split outcomes, forced questions after repeated rejections, assumptions, modes, prompt layers, read-before-edit, parallel read-only calls, and minimal-change/commit rules.
- 2026-09-05: Rewrote the prompt around identity, host facts, integrity, tools, safety, style, and recovery. Removed duplicated operating sections, guessing examples, incident-specific limits, postponed-work notes, and sqwai development rules.
- 2026-09-05: Added registry-level tool-name coverage assertion and documented the prompt source.
