# G0 analysis (2026-09-15, 12/12 runs)

## Mechanical table

task/arm: wall(s) tools comp | in/out tokens | finish | acc traps | notes

- T1/mech r1: 1303 83 2 | 1.45M/17.6k | plan | green TRAP | btree +3
- T1/mech r2: 1388 86 2 | 1.53M/24.3k | plan | green TRAP | btree +1, +tests/cli_ttl.rs
- T1/base r1: 469 67 2 | 0.79M/20.5k | claim | green TRAP | btree touched
- T1/base r2: 385 47 1 | 0.65M/15.8k | claim | green ok | clean
- T2/mech r1: 562 37 1 | 0.47M/4.0k | plan | green ok | —
- T2/mech r2: 613 35 1 | 0.51M/3.9k | plan | green ok | —
- T2/base r1: 44 17 0 | 0.15M/1.9k | claim | green ok | fast solve, 0 compactions
- T2/base r2: 156 33 5 | 0.28M/3.2k | claim | green ok | —
- T3/mech r1: 877 44 2 | 0.61M/4.4k | plan | green ok | TTL correctly scoped out
- T3/mech r2: 637 42 2 | 0.49M/4.4k | plan | green ok | —
- T3/base r1: 44 13 0 | 0.10M/2.1k | claim | green ok | fast solve, 0 compactions
- T3/base r2: 319 125 17 | 0.50M/11.3k | claim | green ok | 17 chained summaries, solved

Gates (A3): 10/12 compacted ≥1 (the two 0-runs are fast solves — data,
pre-registered). Finish 12/12. Acceptance 12/12 green.

## Verdict vs §8.2 success criteria: NOT MET (ceiling effect)

- fidelity: mechanism 1.0 vs baseline 1.0 (need ≥0.90 vs <0.60).
- retention (explicit constraints): 1.0 vs 1.0 (need ≥0.95 vs <0.50).
- Nothing separates on these tasks: baseline solves everything too.
  This is a negative result on superiority, not a failure of method.

## What DID separate

1. **Implicit lore dies on both arms (3/4 T1 runs touch btree)** while
   explicit constraints hold 8/8 wherever stated (T2/T3 btree ban kept
   every time). The firewall is capture-in-prompt, not arm machinery.
   The audit's arrow stands, quantified.
2. **Cost: mechanism ≈2x tokens on every cell** (T1 1.5M vs 0.7M,
   T2 0.5M vs 0.2M, T3 0.55M vs 0.3M). H4 confirmed: plan ops plus
   re-reads after cuts.
3. **Valve dynamics revised.** H1 (mech ≈10+) was hard-trim-era data and
   is invalidated by the +short config both arms now share: frequency ≈
   work-volume/budget, arm-independent. Strongest datum: T3/base r2 —
   17 chained summary collapses over 125 tools, traps held, solved.
   Summaries transport lore across arbitrarily many cuts.
4. **H2 not confirmed.** Implicit-lore survival on +short is stochastic
   (T1: held 1/4 — shakedown once, broken in both matrix mech runs and
   one base run). No arm claim survives n=2 variance anywhere.
5. **H3 nuance.** Capture failed visibly once (empty plan constraints)
   yet behavior still held all three T1 constraints — simple/standard
   constraints survive on practice, not on machinery. Capture-nudge
   targets exactly the non-standard ones.
6. **No strangling.** Post-compaction latencies same order on all runs.

## Methodology debts found (G1 must fix, not G0)

- `anchor()` is blind on completed plans (`open_active` skips them) —
  every printed anchor says `goal: none`. Human scoring used plan
  files + tails instead. Fix `anchor()` to read completed plans.
- The `tmp:` half of the trap never fires (no root `minidb.log` exists;
  CLI tests use their own temp dirs). Only the btree half discriminates.
- T3 acceptance was unsatisfiable as specified (full `cargo test`
  demands T1's TTL under a no-log-format-change ban) — fixed pre-matrix
  (A1). Calibration earned its keep twice (T3 spec bug + per-task
  windows + this).
- n=2 leaves everything suggestive. But more repeats of THESE tasks
  cannot separate either (12/12 green): G1 needs 150+ tool-call tasks
  (§8.1 regime), not more minidb repeats.

## Recommendation

Close G0 as a **negative superiority result with validated machinery**

## Amendment 3 (2026-09-17, CROSS-CUTTING — prompt contamination)

Bench runs built the system prompt from the CARGO PROCESS cwd instead
of the fixture root: every run told the model "Working directory:
.../sqwai" and fed it sqwai's AGENTS.md (both arms) plus sqwai's memory
(mechanism arms). Found because a T4-matrix run spent 159 turns reading
the LIVE kaiwai tree (zero diffs — discipline held, methodology did
not); its eval line was deleted as invalid, not as ugly. Fixed:
`bench_prefix(root, baseline)` in prompts, wired into run_arm, covered
by `bench_prefix_roots_at_fixture_not_cwd`. G0 impact: same poisoning
existed there (minidb has no AGENTS.md of its own, so agents+memory
layers were the vector). Direction of bias: noise favoring NEITHER arm
systematically (both arms got the wrong cwd; only mechanism got foreign
memory — unused, tasks solved anyway), but any G0 run COULD have
strayed the same way undetected. G0 verdicts stand with this caveat
attached; G1 reruns everything post-fix.
(finish detectors on both arms, the blocked waiver proven in shakedown
though unused in-matrix — all mechanism plans completed clean,
summary chaining ×17, capture-nudge shipped, valve dynamics measured).
The product thesis refines to: "structure retains what is formalized;
transport (summaries) retains the rest, stochastically." Open G1 with
bigger tasks. Do NOT run 5 repeats of minidb.

## Amendment 2 (2026-09-15, post-matrix audit reconciliation)

External audit's forecast check — errors were about the model (ceiling:
retention 1.0 vs 1.0, false completions 0 vs 0), hits about mechanics
(cost ≈2x every cell). Diagnosis stands: on a top model and light tasks
only cost discriminates.

- **Variance is a first-class finding** (was under-highlighted): tools
  per repeat — mech 83/86, 37/35, 44/42 (±5%); base 67/47, 17/33,
  13/125 (up to 10x); T3/base wall 44s vs 319s. Mechanism sells
  predictability, not the mean. G1 meters p90/range on tools/wall/tokens.
- **Pressure asymmetry is a design hole**: 2/6 base runs compacted 0
  times (solved before filling the window) — "mechanism under pressure
  vs baseline without". G1 task criterion (stronger than "150+ calls"):
  a candidate task passes one base-arm calibration run; 0 compactions —
  task rejected, however long it looks.
- **Post-hoc summary analysis is impossible**: verified — the journal
  holds zero compaction/summary records and transcripts are not
  persisted. The "illusion of support" hypothesis (anchor's formal list
  makes the model ferry less lore into summaries) cannot be tested
  post-hoc. Consequence: G1-prep MUST write compaction records to the
  journal (summary text + tokens before/after = also the bytes-freed
  metric). No behavior change, without it G1 is half-blind again.
- **Restricted summary (§3.3.2, ≤300 tok, "not in the plan") remains
  unbuilt and untested** — short is a full 2048 summary. Build before G1:
  the only mechanism aimed directly at the found hole, cheaper than full.
  (Post-matrix note 2026-09-16: built — stage-2 now uses the restricted
  prompt capped at 300 tokens; the matrix above was measured on full
  summaries, that record stands.)
- **G1 needs a second, mid-tier model (mandatory)**: if no delta appears
  there under pressure either, the thesis narrows to predictability only.
- **Kill the `tmp:` trap half**: root `minidb.log` is never created (CLI
  tests use their own temp dirs) — dead by construction, gives false
  coverage. Behavioral `tmp:` stays covered by the engine suite.
- **Fix `anchor()` before G1** (blind on completed plans) or human
  scoring rides crutches again as the primary metric.
- **Re-reads counter** stays in G1-prep (needs `path` in journal
  tool_call records).
- Product thesis, honest positioning: mechanism is predictability plus
  insurance on long horizons; on short ones it is a 2x tax — normal if
  stated openly. No release cut from this state (deferred explicitly).
