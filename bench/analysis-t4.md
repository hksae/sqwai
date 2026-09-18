# T4 retention analysis (2026-09-18, 4/4 cells: T4 x mech/base x 1.3/1.2, n=1)

Regime: 256K simulated, threshold 0.15 (~40K budget), restricted-short
summaries (intended ≤300 tok), fixture kaiwai-frozen.

## Mechanical table

cell: wall tools comp | in / cached / out | finish | acc traps | rereads

- mech/1.3: 3213s 321 4 | 15.9M / 5.2M / 63k | waiver-PASS | T T | 133
- base/1.3: 1906s 495 20 | 18.3M / 16.5M / 111k | claim-PASS | T T | 210
- mech/1.2: 3252s 381 6 | 18.2M / 5.8M / 71k | plan-PASS | T T | 125
- base/1.2: 2073s 361 9 | 17.0M / 15.5M / 68k | claim-PASS | T T | 74

Gates: all cells compacted (4/6/20/9), all finished per arm rules
(mech/1.3 via harness-green waiver), acceptance 4/4, traps 4/4.

## Verdict vs success criteria: NOT MET (ceiling again, on a HARD task)

- fidelity 1.0 x4, retention (explicit) 1.0 x4. Same as G0: baseline
  solves everything too. A hard task did not separate correctness.
- Capture (new G1 metric): mech captured 7/7 prompt constraints
  verbatim into plans on BOTH models (incl. both lore items); baseline
  has no plan by design, lore ferried via summaries (keywords present
  in every summary chain). Retention 7/7 behaviorally on all arms.

## What DID separate

1. **Cache rate: mechanism 33% vs baseline 90% cached (both models).**
   Mechanism machinery (plan block revs, anchor/memory refresh) churns
   the SYSTEM prefix every turn → prefix-cache misses → full-price
   retransmission. Baseline's churn is mid-transcript → cache holds.
   Refined cost thesis: the mechanism tax is cache invalidation, not
   turn count. Totals look similar ONLY because mech ran fewer tools;
   per-token-effective cost diverges hard.
2. **Variance (p90 thesis confirmed):** tools mech 321/381 (±9%) vs
   base 495/361 (±27% + an order of shape difference); compactions
   4/6 vs 20/9. Mechanism sells predictability — now measured, not
   claimed. n=1 limits strength; range ratio is the honest statistic.
3. **Summary ballooning (cap ignored):** summaries run 33K→140K CHARS
   despite max_output_tokens=300 on the wire — the endpoint does not
   enforce it. Chained headers stack ("## Earlier..." xN), late
   compactions free ~0 tokens (before≈after: 38687→38683) while burning
   full model calls. The ≤300 design is not operational; host-side
   truncation is the fix (filed, not built).
4. **Valve dynamics hold:** frequency ≈ work/budget on both arms
   (495 tools→20 cuts; 321→4). No arm effect on frequency.
5. **rereads scale with amnesia, not failure:** 74–210 per cell, all
   cells solved. Re-reading is the price of pressure, survivable with
   plan+anchor (mech) or summaries (base) alike.

## Incidents (methodology cost, all disclosed)

- Stray run (prompt pointed at live tree, 159 turns read-only):
  invalid, eval line deleted, fixed by bench_prefix rooting + G0 caveat.
- propose_plan advertised on baseline (filter omission): 1.3 called it
  once, auto-accepted, never used. Cell valid (1 wasted turn), fixed.
- Quote-stripping acceptance: false reds on green trees, fixed by
  direct spawn (verified 167+280).
- 1h wall cap clipped a solved run → raised to 2h; clipped line
  dropped as superseded series.
- Rate-limit storms (429s) mid-matrix: retried through, no cell lost.
- Excluded, not data: 2 killed partials, 3 manual/exploratory runs,
  1 stray. 4 matrix lines only.

## Deviations locked post-hoc (budget-driven)

- Single task (T4) not three; n=1 per cell, not 2. With ~$1/cell,
  the locked $5–7 fits 5–6 runs total. No 5-repeat follow-up on THESE
  cells would separate correctness (16/16 green across G0+T4-matrix
  so far) — variance bands stay descriptive.

## Recommendation

Close T4-measurement as **negative superiority, positive
machinery**: plans capture lore verbatim on both models, summaries
ferry it on baseline, everything solves, traps hold. The product
thesis stands refined: mechanism = predictability + cache-taxed
insurance; the next engineering (not measurement) items are host-side
summary truncation and cache-stable machinery. No more paid matrix
runs without a new question that n=1 cannot answer.
