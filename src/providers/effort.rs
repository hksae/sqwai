//! Mapping from the user's effort slider to what a provider actually receives.
//!
//! DESIGN.md §5.1 asks for two things at once: an honest mapping per
//! provider, and honest *reporting* of that mapping — "when a model ignores
//! the selected level, the status/header text must say `effort: <level>
//! (ignored by model)` rather than implying that more work is active".
//!
//! Both come from the same place here. [`plan`] is total: every
//! level/declaration pair produces a [`Plan`] that says what goes on the wire
//! **and** how far that is from what the user asked for. The providers read
//! `wire`; the UI reads `status`. Neither can drift from the other, because
//! there is no second copy of the decision.

use crate::config::{EffortControl, EffortLevel, EffortSupport};

/// what the request carries for this level, if anything
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    /// nothing at all: no reasoning parameter in the body
    Nothing,
    /// a named level (`reasoning_effort`, `reasoning.effort`)
    Level(&'static str),
    /// a token budget (Anthropic `thinking.budget_tokens`)
    Budget(u32),
}

/// how faithfully the request represents what the user asked for
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// the model gets exactly the level that was selected
    Applied,
    /// something is sent, but not the level that was asked for
    Clamped { to: &'static str },
    /// nothing the model will act on
    Ignored { why: &'static str },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub level: EffortLevel,
    pub wire: Wire,
    pub status: Status,
}

/// Anthropic thinking budgets per level. `max` is a real step here, which is
/// why `budget` support honours all five levels.
fn budget(level: EffortLevel) -> u32 {
    match level {
        EffortLevel::Off => 0,
        EffortLevel::Low => 2048,
        EffortLevel::Medium => 8192,
        EffortLevel::High => 16384,
        EffortLevel::Max => 32768,
    }
}

pub fn plan(level: EffortLevel, support: EffortSupport) -> Plan {
    let (wire, status) = match (support.control, level) {
        // ── no reasoning control ────────────────────────────────────────────
        // `off` is the one level a model without reasoning satisfies for free:
        // asking for no thinking and getting none is not an ignored request.
        (EffortControl::None, EffortLevel::Off) => (Wire::Nothing, Status::Applied),
        (EffortControl::None, _) => (
            Wire::Nothing,
            Status::Ignored {
                why: "this model has no reasoning control",
            },
        ),

        // ── on/off only ─────────────────────────────────────────────────────
        (EffortControl::Toggle, EffortLevel::Off) if !support.always_on => {
            (Wire::Nothing, Status::Applied)
        }
        (EffortControl::Toggle, EffortLevel::Off) => (
            Wire::Level("medium"),
            Status::Ignored {
                why: "this model always reasons",
            },
        ),
        // reasoning is switched on, but the level never reaches the model
        (EffortControl::Toggle, _) => (Wire::Level("medium"), Status::Clamped { to: "medium" }),

        // ── named levels ────────────────────────────────────────────────────
        (EffortControl::Levels | EffortControl::Xhigh, EffortLevel::Off) => {
            if support.always_on {
                (
                    Wire::Nothing,
                    Status::Ignored {
                        why: "this model always reasons",
                    },
                )
            } else {
                (Wire::Nothing, Status::Applied)
            }
        }
        (EffortControl::Levels | EffortControl::Xhigh, EffortLevel::Low) => {
            (Wire::Level("low"), Status::Applied)
        }
        (EffortControl::Levels | EffortControl::Xhigh, EffortLevel::Medium) => {
            (Wire::Level("medium"), Status::Applied)
        }
        (EffortControl::Levels | EffortControl::Xhigh, EffortLevel::High) => {
            (Wire::Level("high"), Status::Applied)
        }
        (EffortControl::Xhigh, EffortLevel::Max) => (Wire::Level("xhigh"), Status::Applied),
        // `max` above a three-level API is the case #20 was filed about: it
        // used to be sent as `high` with the UI still showing `max`.
        (EffortControl::Levels, EffortLevel::Max) => {
            (Wire::Level("high"), Status::Clamped { to: "high" })
        }

        // ── numeric budget ──────────────────────────────────────────────────
        (EffortControl::Budget, EffortLevel::Off) if support.always_on => (
            Wire::Nothing,
            Status::Ignored {
                why: "this model always reasons",
            },
        ),
        (EffortControl::Budget, EffortLevel::Off) => (Wire::Nothing, Status::Applied),
        (EffortControl::Budget, l) => (Wire::Budget(budget(l)), Status::Applied),
    };
    Plan {
        level,
        wire,
        status,
    }
}

impl Plan {
    /// Full sentence for places with room for one: the effort menu, and the
    /// confirmation shown when the level changes. The wording for an ignored
    /// level is the one §5.1 prescribes.
    pub fn label(&self) -> String {
        let level = self.level.as_str();
        match self.status {
            Status::Applied => match self.wire {
                Wire::Budget(b) if b > 0 => format!("effort: {level} ({b} thinking tokens)"),
                _ => format!("effort: {level}"),
            },
            Status::Clamped { to } => format!("effort: {level} (sent as {to})"),
            Status::Ignored { why } => format!("effort: {level} (ignored by model — {why})"),
        }
    }

    /// Status-bar form. The bar is 70 columns wide on a small terminal, so the
    /// reason is dropped — but never the fact that the level did not land.
    pub fn short_label(&self) -> String {
        let level = self.level.as_str();
        match self.status {
            Status::Applied => format!("ef:{level}"),
            Status::Clamped { to } => format!("ef:{level}→{to}"),
            Status::Ignored { .. } => format!("ef:{level} (ignored)"),
        }
    }

    pub fn is_honoured(&self) -> bool {
        matches!(self.status, Status::Applied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn support(control: EffortControl, always_on: bool) -> EffortSupport {
        EffortSupport { control, always_on }
    }

    /// The bug from #20: `max` was silently sent as `high` on every
    /// three-level API while the UI kept showing `max`.
    #[test]
    fn max_is_clamped_where_xhigh_is_not_documented_and_says_so() {
        let p = plan(EffortLevel::Max, support(EffortControl::Levels, false));
        assert_eq!(p.wire, Wire::Level("high"));
        assert_eq!(p.status, Status::Clamped { to: "high" });
        assert_eq!(p.short_label(), "ef:max→high");

        let p = plan(EffortLevel::Max, support(EffortControl::Xhigh, false));
        assert_eq!(p.wire, Wire::Level("xhigh"));
        assert!(p.is_honoured());
    }

    /// A model that cannot stop reasoning makes `off` a request the provider
    /// will not honour, and that has to be visible rather than implied.
    #[test]
    fn off_on_an_always_reasoning_model_is_reported_as_ignored() {
        let p = plan(EffortLevel::Off, support(EffortControl::Levels, true));
        assert_eq!(p.wire, Wire::Nothing);
        assert!(matches!(p.status, Status::Ignored { .. }));
        assert_eq!(p.short_label(), "ef:off (ignored)");
        assert!(p.label().contains("ignored by model"));
    }

    /// Asking a model with no reasoning for no reasoning is not a failure.
    #[test]
    fn off_without_any_reasoning_control_is_honoured() {
        let p = plan(EffortLevel::Off, support(EffortControl::None, false));
        assert!(p.is_honoured());
        assert_eq!(p.wire, Wire::Nothing);

        let p = plan(EffortLevel::High, support(EffortControl::None, false));
        assert_eq!(p.wire, Wire::Nothing);
        assert!(matches!(p.status, Status::Ignored { .. }));
    }

    #[test]
    fn a_toggle_api_gets_reasoning_on_but_not_the_level() {
        let p = plan(EffortLevel::High, support(EffortControl::Toggle, false));
        assert_eq!(p.wire, Wire::Level("medium"));
        assert_eq!(p.status, Status::Clamped { to: "medium" });
        let p = plan(EffortLevel::Off, support(EffortControl::Toggle, false));
        assert_eq!(p.wire, Wire::Nothing);
        assert!(p.is_honoured());
    }

    /// With a numeric budget every level is a distinct request, so all five
    /// are honoured and the label can name the actual budget.
    #[test]
    fn a_budget_api_honours_every_level() {
        for level in EffortLevel::ALL {
            let p = plan(level, support(EffortControl::Budget, false));
            assert!(p.is_honoured(), "{level:?} should be honoured");
        }
        let p = plan(EffortLevel::Max, support(EffortControl::Budget, false));
        assert_eq!(p.wire, Wire::Budget(32768));
        assert!(p.label().contains("32768 thinking tokens"));
    }

    /// Nothing may fall through: the mapping must answer for every pair, since
    /// a missing case would silently become "send nothing" at runtime.
    #[test]
    fn every_level_and_declaration_pair_is_answered() {
        for control in EffortControl::ALL {
            for always_on in [false, true] {
                for level in EffortLevel::ALL {
                    let p = plan(level, support(control, always_on));
                    assert_eq!(p.level, level);
                    // an ignored plan must carry a reason a user can read
                    if let Status::Ignored { why } = p.status {
                        assert!(!why.is_empty(), "{control:?}/{level:?} has no reason");
                    }
                }
            }
        }
    }
}
