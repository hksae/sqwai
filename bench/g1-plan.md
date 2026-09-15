# G1 plan (draft, 2026-09-16)

## Question

Does the mechanism compensate for model weakness where G0 (top model,
light tasks) showed only cost? Plus: does it generalize — same pattern
expected, strength of delta may differ.

## Models (locked)

- Primary: `muse spark 1.3` (G0 continuity).
- Second: `muse spark 1.2` — clearly weaker (DeepSWE 59.3 vs 75.4),
  same endpoint/format/price, zero integration risk. User adds it to
  config themselves.
- Rejected for G1: DeepSeek V4.1 Flash — NOT weaker than 1.3 (T-Bench
  90.6 vs 88.8), answers generality not weakness; ~2x per-token price;
  promo cliff Sept 20. Third model later, optionally.

## Tasks (TBD — the critical path)

- Regime: 150+ tool calls (§8.1), NOT minidb scale.
- Admission criterion (stronger than size): one base-arm calibration
  run per candidate; 0 compactions — task rejected, however long.
- Count: 3 (keep G0 shape); 2 acceptable if tasks are big.
- Must contain non-standard invariants (not derivable from practice),
  or standard prohibitions survive on habit and measure nothing (H3).
  Definition (locked): an invariant that follows from NEITHER the code,
  NOR practice, NOR the README — something the user said once in chat
  that cannot be derived from the repository. It can only be retained,
  never reconstructed. Example: "don't touch btree" is standard (the
  deprecation is in the code); "no BTreeMap in new files, HashMap only —
  we had a perf incident last quarter" is non-standard. Admission test
  at task-design time: "can the model derive this without being told?"
  Yes — reject as G1 material.

## Matrix

tasks × arms × models × repeats = 3×2×2×2 = 24 runs (+6 base
calibrations). Repeats 2, 5 only on variance demand. Per-task windows
recalibrated per model (fractions of that model's context).

## Metrics (G0 + deltas)

- All G0 machine scores (acceptance, traps=btree only, diffs,
  compactions, wall/tools/tokens, latencies) + human fidelity/retention.
- NEW first-class: p90/range on tools/wall/tokens per arm (G0 showed
  mech ±5% vs base up to 10x — predictability is the thesis now).
- NEW: bytes-freed per compaction (journal records are in).
- Cost column stays separate from verdicts.

## Prep status

Done: journal compaction records (+summary text), `tmp:` trap killed,
`anchor()` reads completed plans, restricted short summary (≤300,
uncovered-only + plan hint), capture-nudge, Retry printing, stream
total-timeout 600s.
Todo: re-reads counter (needs `path` in journal tool_call records),
`SQWAI_BENCH_SUMMARY` doc line in bench method notes.

## Cost estimate

G0 matrix + shakedowns + calibrations ≈ $0.63 total. G1: spark runs
≈$0.15–0.30 each at 3–5x G0 size; 24 + 6 runs ≈ $5–10. Recount exactly
after base calibrations, before committing to the full matrix.
