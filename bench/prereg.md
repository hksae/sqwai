# G0 pre-registration (locked before the 12 runs)

Date: 2026-09-15. Anything below changes only with a dated amendment
noting what was seen and why the change does not chase scores.

## Question

Does the durable loop (plan + evidence + anchor, mechanism arm) retain
goals, constraints, and trap-lore across compactions better than a plain
summarize-and-continue agent (baseline arm)?

## Fixed configuration

- Model: `muse spark 1.3` via `SQWAI_BENCH_MODEL`, context 1M.
- Threshold: `SQWAI_BENCH_THRESHOLD=0.01` (budget ~10k).
  Why not the spec's 0.04: measured 2026-09-14, T1's prune steady-state
  sits near ~16k tokens, so 0.04 compacts 0 times per run. Why not lower:
  at 0.008 the btree trap breaks (forgetting), at 0.003 the agent thrashes
  (128 reads, 0 edits). 0.01 is the lowest window where T1 still solves
  with traps intact.
- Mechanism arm: full machinery + `SQWAI_BENCH_SUMMARY=short` (fixed arm
  config, not a hack — the unformalized-lore finding made hard-trims-only
  indefensible). NOTE: `short` is the existing stage-2 path (full summary,
  limit 2048), NOT the §3.3.2 restricted ≤300-token "not in the plan"
  variant, which is unimplemented.
- Baseline arm: `SQWAI_BENCH_BASELINE=1` (no plan/journal/memory/diary/
  anchor, `summary=short` forced, plan-first gate lifted). Journal still
  records everything as observer data.
- Tasks: minidb T1/T2/T3, specs frozen (TASKS.md + `TaskSpec`).
- Repeats: 2 per task per arm = 12 runs. Wall cap 1h per run.
- Debug tracing (`SQWAI_BENCH_DEBUG=1`) stays on for all runs.

## Go/no-go per run (shakedown gates, already met on T1)

- Compactions: ≥ 2 for T1 (smallest task, ~2 fill cycles per run),
  ≥ 3 for T2/T3 if they run longer. Frequency asymmetry between arms is
  expected (valve dynamics, see Hypotheses) and is NOT a failure.
- Finish detector fires; post-compaction turns not an order slower;
  diary shows no timeout tail.
- Acceptance independently green counts as task solved regardless of arm.

## Finish rules (§8.2 symmetric)

- Mechanism: no open steps (`pending`/`in_progress` fail it) AND every
  acceptance item verified or waived. WAIVER: `blocked` with rationale
  counts as closed when acceptance is green (honest evidence discipline,
  not an open end). The `complete` call itself is not required.
- Baseline (no plan): model claims completion in text AND the harness
  independently verifies the task (acceptance commands + trap checks).

## Scoring (per run → `bench/<task>/<arm>-<n>.eval.jsonl`)

Automatic: acceptance_green, traps_ok (btree hash + no `tmp:` leak),
diffs_per_path (churn/redundant-work proxy), compactions, wall_secs,
tool_calls, tokens in/out/cached ($ at run tariff), latency tool→text.
Human (from report anchor + final text, blind to arm where possible):
goal fidelity (0/0.5/1), constraint retention (0..1).

## Success criteria (unchanged from §8.2)

Superiority on goal fidelity (≥ 0.90 vs baseline < 0.60), constraint
retention (≥ 0.95 vs baseline < 0.50), redundant-work reduction, with
non-inferiority on reference validity. Cost is a separate column: a
retention win at +50% cost is a different product conclusion, recorded
as such, not as failure.

## Pre-registered hypotheses (direction known, NOT conclusions)

1. Valve dynamics: hard trims free ~1k tokens/cut (hover at budget,
   many cuts), summary collapses free ~7.5k (few cuts). Expect
   mechanism ≈ 10+ compactions/run vs baseline ≈ 2 at the same threshold.
2. Trap-lore: implicit lore (btree deprecation, in fixture not prompt)
   survives summary transport, dies under hard trims. With mechanism+short
   both arms should hold traps; if mechanism still drops them, the hole is
   in capture, not transport.
3. Capture unreliability: plan constraints were empty in 1 of 2 mechanism
   runs despite explicit prompt constraints. Expect variance in constraint
   retention on the mechanism arm. (Capture-nudge shipped to prod after
   the lock; bench prompts bypass it by construction.)
4. Cost: mechanism runs 2–3x tokens (thrashing re-reads after cuts).
   Recorded, not judged.

## Known limitations (accepted, not fixed before the matrix)

- `short` ≠ restricted summary (see above).
- Re-reads are not metered separately from plan ops (journal stores only
  `args_digest` for tool calls, no paths). Cost is tokens in/out only.
- T1's btree trap is implicit (absent from T1 prompt constraints, present
  in T2/T3). Its retention measures lore transport, not constraint
  discipline — by design.
- Single model, single fixture family. Generalization is G1 business.

## Forbidden after this lock

Retuning threshold/model/gates, adding runs to replace ugly ones,
peeking at matrix scores to adjust arms. Variance handling only: 5
repeats iff variance demands it (§8.2), decided from CIs, not from
wishes. Amendments go here, dated, before the runs they affect.
