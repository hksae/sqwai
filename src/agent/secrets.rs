//! Secret screening for everything the host writes down (§2.3.6).
//!
//! Applied to the diary, MEMORY.md, the prompt blocks built from them, and the
//! free-text fields of journal records. It is the last thing between a command
//! that echoes a token and a file that keeps it forever.

use regex::Regex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Screened {
    pub text: String,
    pub redacted: bool,
}

/// Screen text before it reaches durable diary or summary storage.
pub fn screen(text: &str) -> Screened {
    let patterns = [
        Regex::new(r"(?i)\bAKIA[0-9A-Z]{16}\b").unwrap(),
        Regex::new(r"\bsk-[A-Za-z0-9_-]{16,}\b").unwrap(),
        Regex::new(r"\bghp_[A-Za-z0-9]{20,}\b").unwrap(),
        Regex::new(r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----").unwrap(),
        Regex::new(r"(?i)\bBearer\s+[A-Za-z0-9._~+/=-]{16,}").unwrap(),
        Regex::new(r"(?i)https?://[^\s/@:]+:[^\s/@]+@[^\s]+\b").unwrap(),
        Regex::new(r"(?i)\b[A-Z][A-Z0-9_]*(?:KEY|TOKEN|SECRET|PASSWORD|CREDENTIAL)[A-Z0-9_]*\s*=\s*[^\s]+\b").unwrap(),
    ];
    let mut output = text.to_string();
    let mut redacted = false;
    for pattern in patterns {
        let replaced = pattern.replace_all(&output, "[redacted]");
        if replaced != output {
            redacted = true;
            output = replaced.into_owned();
        }
    }
    let token_re = Regex::new(r##"[^\s`\"']{20,}"##).unwrap();
    let mut replacements = Vec::new();
    for found in token_re.find_iter(&output) {
        if shannon_entropy(found.as_str()) > OPAQUE_ENTROPY {
            replacements.push((found.start(), found.end()));
        }
    }
    for (start, end) in replacements.into_iter().rev() {
        output.replace_range(start..end, "[redacted]");
        redacted = true;
    }
    Screened {
        text: output,
        redacted,
    }
}

/// Entropy above which a long token is treated as an opaque blob.
///
/// The threshold used to be 4.0, which is below where real facts sit and so
/// redacted them. Measured on this project's own strings:
///
/// | token | entropy |
/// |---|---|
/// | `.sqwai/plans/01M1V0GK22W0PFVYBM0501N1FJ.json` | 4.54 |
/// | `/home/runner/work/sqwai/sqwai/target/debug/build/...` | 4.26 |
/// | `target/debug/deps/sqwai-1d33ade74ad820b7` | 4.21 |
/// | `https://api.anthropic.com/v1/messages` | 4.01 |
/// | AWS secret access key | 4.71 |
/// | `sk-ant-api03-…` | 4.93 |
/// | `ghp_…` | 5.17 |
/// | a JWT | 5.45 |
///
/// At 4.0 the plan path, the build path and every https URL were replaced by
/// `[redacted]` — in the journal, whose whole purpose is to hold those facts.
/// The highest benign value measured is 4.54 and the lowest credential 4.71,
/// so 4.6 separates them, with the explicit patterns above covering the shapes
/// every common provider uses. It is a heuristic and the margin is thin: the
/// patterns are the real defence, this is the net under them.
const OPAQUE_ENTROPY: f64 = 4.6;

fn shannon_entropy(value: &str) -> f64 {
    if value.is_empty() {
        return 0.0;
    }
    let mut counts = [0usize; 256];
    for byte in value.bytes() {
        counts[byte as usize] += 1;
    }
    let len = value.len() as f64;
    counts
        .into_iter()
        .filter(|count| *count > 0)
        .map(|count| {
            let p = count as f64 / len;
            -p * p.log2()
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_known_secret_shapes_and_entropy_tokens() {
        let value =
            screen("AKIA1234567890ABCDEF Bearer abcdefghijklmnop qwertyuiopasdfghjklzxcvbnm");
        assert!(value.redacted);
        assert!(!value.text.contains("AKIA"));
        assert!(!value.text.contains("Bearer"));
    }

    /// The journal's `summary` carries command output, which is full of paths.
    /// A path is not a secret and must survive screening, or the record stops
    /// being able to answer "what did you do?".
    #[test]
    fn ordinary_paths_and_identifiers_survive() {
        for text in [
            "src/agent/tools/exec.rs",
            "(exit code 0) test result: ok. 253 passed; 0 failed",
            "wrote .sqwai/plans/01M1V0GK22W0PFVYBM0501N1FJ.json (+31/-31)",
            "5b7f51dbdcb503a6d1274429ba88d8dd696e481e",
        ] {
            let value = screen(text);
            assert!(!value.redacted, "{text:?} was redacted as {:?}", value.text);
        }
    }

    /// The thresholds are load-bearing, so they are pinned rather than trusted.
    #[test]
    fn the_entropy_threshold_sits_between_facts_and_credentials() {
        // the highest-entropy benign string measured in this project
        assert!(shannon_entropy(".sqwai/plans/01M1V0GK22W0PFVYBM0501N1FJ.json") < OPAQUE_ENTROPY);
        // the lowest-entropy credential measured
        assert!(shannon_entropy("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY") > OPAQUE_ENTROPY);
    }

    /// A real key must not survive it.
    #[test]
    fn credentials_do_not_survive() {
        for text in [
            "export ANTHROPIC_API_KEY=sk-ant-api03-abcdefghijklmnopqrstuvwxyz012345",
            "https://user:hunter2hunter2@internal.example.com/repo.git",
            "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
        ] {
            let value = screen(text);
            assert!(value.redacted, "{text:?} survived screening");
        }
    }
}
