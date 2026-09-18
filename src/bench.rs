//! Benchmark support (G0, §8.2).
//!
//! The baseline arm runs the same agent loop with the durable machinery
//! switched off, so retention can be measured against a plain
//! summarize-and-continue agent:
//!
//! - hidden tools: `plan`, `propose_plan`, `note`, `journal`, `memory_propose`,
//!   `memory_read` (the model cannot see or touch plans, notes, the
//!   journal projection, or durable memory);
//! - prompt: no `USER.md`/`MEMORY.md`/diary block, no durable-plan block,
//!   no host anchor, no resume notice;
//! - no diary writes;
//! - `compaction.summary` forced to `short` (model summaries instead of
//!   the host anchor);
//! - the plan-first gate is lifted (there is no plan to require).
//!
//! The host journal keeps recording everything: it is the observer's data
//! source, not a mechanism under test.

/// True when the baseline arm is requested via the environment.
pub fn baseline() -> bool {
    baseline_override().unwrap_or_else(|| {
        matches!(
            std::env::var("SQWAI_BENCH_BASELINE")
                .unwrap_or_default()
                .to_ascii_lowercase()
                .as_str(),
            "1" | "true" | "yes"
        )
    })
}

thread_local! {
    static BASELINE_OVERRIDE: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

/// True when a mechanism-arm run should take short model summaries instead
/// of hard trims: `SQWAI_BENCH_SUMMARY=short`. The baseline arm already
/// implies this via [`baseline`]; this switch covers the mechanism arm for
/// the G0 shakedown follow-up (unformalized lore dies at hard trims):
/// one run, fixed in the method before pre-registration.
pub fn summary_short() -> bool {
    matches!(
        std::env::var("SQWAI_BENCH_SUMMARY")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "short"
    )
}

fn baseline_override() -> Option<bool> {
    BASELINE_OVERRIDE.with(|slot| slot.get())
}

/// Deterministic switch for tests: the env var is process-global, so
/// parallel tests must not touch it. Always reset to `None` at the end
/// of the test (a guard is cheapest).
#[cfg(test)]
pub fn set_baseline_override(value: Option<bool>) {
    BASELINE_OVERRIDE.with(|slot| slot.set(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ResetGuard;

    impl Drop for ResetGuard {
        fn drop(&mut self) {
            set_baseline_override(None);
        }
    }

    #[test]
    fn baseline_flag_is_off_unless_requested() {
        let _guard = ResetGuard;
        set_baseline_override(None);
        // NOTE: fails if SQWAI_BENCH_BASELINE is set in the test runner's
        // environment — that variable is the production switch, not test state.
        assert!(!baseline());
    }

    #[test]
    fn baseline_override_wins_without_touching_the_environment() {
        let _guard = ResetGuard;
        set_baseline_override(Some(true));
        assert!(baseline());
        set_baseline_override(Some(false));
        assert!(!baseline());
    }
}
