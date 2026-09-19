//! H0 criticism detector: pure-Rust inference for the learned student.
//!
//! The model is a logistic regression on hashed char-trigrams, trained
//! offline by `bench/criticism/train.py` from LLM-labeled examples.
//! Weights ship as `criticism_weights.json` (sparse, versioned) and are
//! embedded at compile time — inference is microseconds, $0, offline.
//!
//! Normalization, trigram windows and the FNV-1a hash below MUST stay
//! byte-identical to train.py (each mirrored spot is marked). Change one
//! side, change both, retrain, re-embed.
//!
//! Output is a three-way verdict per user message: `Fire` (confident
//! criticism), `Maybe` (gray zone — the strict trigger and the artifact
//! signal decide at the call site), `Silent`. Thresholds ride with the
//! weights file so a retrain can move them without touching this code.

use std::collections::HashMap;
use std::sync::OnceLock;

const WEIGHTS_JSON: &str = include_str!("criticism_weights.json");

/// Per-message verdict. `Maybe` is not indecision to hide — it is the
/// documented handoff to the strict trigger (fire needs ≥2 signal groups
/// plus prior-turn mutations; the artifact signal breaks the tie).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Fire,
    Maybe,
    Silent,
}

struct Model {
    dim: u64,
    bias: f64,
    weights: HashMap<u64, f64>,
    threshold_fire: f64,
    threshold_maybe: f64,
}

fn model() -> &'static Model {
    static MODEL: OnceLock<Model> = OnceLock::new();
    MODEL.get_or_init(|| {
        let v: serde_json::Value =
            serde_json::from_str(WEIGHTS_JSON).expect("criticism_weights.json parses");
        let weights = v["weights"]
            .as_object()
            .expect("weights is a map")
            .iter()
            .map(|(k, val)| {
                (
                    k.parse::<u64>().expect("weight key is an index"),
                    val.as_f64().expect("weight is a number"),
                )
            })
            .collect();
        Model {
            dim: v["dim"].as_u64().expect("dim"),
            bias: v["bias"].as_f64().expect("bias"),
            weights,
            threshold_fire: v["threshold_fire"].as_f64().expect("threshold_fire"),
            threshold_maybe: v["threshold_maybe"].as_f64().expect("threshold_maybe"),
        }
    })
}

/// Lowercase (full Unicode mapping, like Python's str.lower), ё→е, and
/// the elongation cap: runs longer than 2 collapse to 2.
/// MIRRORED in train.py::normalize.
fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run_char: Option<char> = None;
    let mut run_len = 0usize;
    for ch in text.chars().flat_map(|c| c.to_lowercase()) {
        let ch = if ch == 'ё' { 'е' } else { ch };
        if Some(ch) == run_char {
            run_len += 1;
        } else {
            run_char = Some(ch);
            run_len = 1;
        }
        if run_len <= 2 {
            out.push(ch);
        }
    }
    out
}

fn fnv1a64(data: &[u8]) -> u64 {
    // MIRRORED in train.py::fnv1a64
    let mut h: u64 = 14695981039346656037;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    h
}

/// Raw criticism score in [0, 1]. Deterministic for a fixed weights file.
pub fn score(text: &str) -> f64 {
    let m = model();
    let norm = normalize(text);
    let padded = format!(" {norm} ");
    let chars: Vec<char> = padded.chars().collect();
    let mut sum = m.bias;
    for window in chars.windows(3) {
        let tri: String = window.iter().collect();
        let idx = fnv1a64(tri.as_bytes()) % m.dim;
        if let Some(w) = m.weights.get(&idx) {
            sum += w;
        }
    }
    1.0 / (1.0 + (-sum).exp())
}

/// Three-way verdict using the weights file's own thresholds.
pub fn classify(text: &str) -> Verdict {
    let m = model();
    let p = score(text);
    if p >= m.threshold_fire {
        Verdict::Fire
    } else if p >= m.threshold_maybe {
        Verdict::Maybe
    } else {
        Verdict::Silent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strong_criticism_fires_both_languages() {
        for text in [
            "ты сломал сборку",
            "ничего не работает после тебя",
            "ты всё испортил",
            "you broke the build",
            "nothing works after your change",
            "you broke auth again",
        ] {
            assert_eq!(classify(text), Verdict::Fire, "{text}");
        }
    }

    #[test]
    fn typo_tolerance_survives_single_typos() {
        for text in [
            "ты сламал сборку",
            "не рабоатет",
            "you broke teh build",
            "it doesnt work",
        ] {
            assert_ne!(classify(text), Verdict::Silent, "{text}");
        }
    }

    #[test]
    fn requests_praise_and_chatter_stay_silent() {
        for text in [
            "сделай вот так",
            "ты можешь проверить?",
            "покажи дифф",
            "спасибо, работает",
            "что дальше делаем",
            "can you refactor this?",
            "thanks, nice work",
            "show me the diff",
            " ты не поверишь, но всё завелось ",
        ] {
            assert_eq!(classify(text), Verdict::Silent, "{text}");
        }
    }

    #[test]
    fn normalization_mirrors_training() {
        // ё folds, elongation caps at 2, case folds
        assert_eq!(normalize("СЛОМАААЛ"), normalize("сломаал"));
        assert_eq!(normalize("её"), "ее");
        assert!(normalize("ТЫ СЛОМАЛ").contains("ты сломал"));
    }

    #[test]
    fn mirror_matches_python_bit_for_bit() {
        // pinned against bench/criticism/train.py output:
        // normalize('Ты СЛОМАААЛ ёж') == 'ты сломаал еж'
        assert_eq!(normalize("Ты СЛОМАААЛ ёж"), "ты сломаал еж");
        // fnv1a64('сло') == 0x6fa517ad5f825a50
        assert_eq!(fnv1a64("сло".as_bytes()), 0x6fa517ad5f825a50);
        // full-train scores: fire vs silent with margin
        assert!(score("ты сломал сборку") > 0.99);
        assert!(score("can you refactor this?") < 0.01);
    }

    #[test]
    fn scoring_is_deterministic() {
        let text = "ты сломал сборку опять";
        assert_eq!(score(text).to_bits(), score(text).to_bits());
    }

    /// Dev probe, not an assertion test: scores arbitrary phrases so a
    /// human can spot-check the detector without wiring the turn loop.
    /// Usage (PowerShell):
    ///   $env:SQWAI_CRITIC_PROBE = "ты сломал всё|сделай вот так|you broke it";
    ///   cargo test critic_probe -- --nocapture
    /// Unset → no-op pass.
    #[test]
    fn critic_probe_prints_scores() {
        let Ok(raw) = std::env::var("SQWAI_CRITIC_PROBE") else {
            return;
        };
        for text in raw.split('|').map(str::trim).filter(|t| !t.is_empty()) {
            println!("p={:.4} {:?}  {text}", score(text), classify(text));
        }
    }
}
