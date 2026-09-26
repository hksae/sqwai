//! AB `/why`: deterministic evidence gathering for "why" questions.
//!
//! The host digs the journal and the plan; the model only narrates what
//! was found (via [`micro_call`]). Nothing is
//! interpreted by patterns here beyond token matching — the moment the
//! evidence ends, the narrator is instructed to say so instead of
//! inventing. No evidence at all means no model call (status line,
//! money saved).

use std::path::Path;

/// What the host found for a why-question. All strings are short,
/// pre-truncated renderings — never raw outputs.
#[derive(Debug, Default)]
pub struct Evidence {
    pub plan: Vec<String>,
    pub tools: Vec<String>,
    pub files: Vec<String>,
    pub notes: Vec<String>,
}

const MAX_HITS: usize = 8;
const SUMMARY_CHARS: usize = 160;

/// Tokens that carry meaning for matching: quoted spans, path-like
/// tokens, words of 4+ chars. Stop-words are not filtered — ranking is
/// by recency, and the cap keeps noise out.
fn tokens(query: &str) -> Vec<String> {
    let mut out = Vec::new();
    let chars: Vec<char> = query.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if matches!(c, '"' | '\'' | '`') {
            let mut j = i + 1;
            while j < chars.len() && chars[j] != c {
                j += 1;
            }
            if j > i + 1 {
                out.push(chars[i + 1..j].iter().collect());
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
    for token in query
        .split(|c: char| !(c.is_alphanumeric() || matches!(c, '/' | '\\' | '.' | '_' | '-' | ':')))
    {
        let token = token.trim_matches(|c| matches!(c, '.' | ':' | '/' | '\\'));
        if token.chars().count() >= 4 {
            out.push(token.to_lowercase());
        }
    }
    out.sort();
    out.dedup();
    out
}

fn hit(text: &str, tokens: &[String]) -> bool {
    let lower = text.to_lowercase();
    tokens.iter().any(|t| lower.contains(t))
}

fn clip(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let taken: String = s.chars().take(max).collect();
    format!("{taken}…")
}

/// Dig the session journal and the active plan for anything the query
/// tokens touch: plan steps, tool results, file diffs, notes. Newest
/// first, capped — a why-answer is a summary, not a dump.
pub fn gather(root: &Path, session: &str, query: &str) -> Evidence {
    let mut evidence = Evidence::default();
    let tokens = tokens(query);
    if tokens.is_empty() {
        return evidence;
    }
    if let Ok(Some(active)) = crate::plan::open_active_for_session(root, Some(session)) {
        evidence.plan.push(format!("goal: {}", active.goal.text));
        for step in &active.steps {
            let line = format!(
                "step {} [{}]: {}",
                step.id,
                step.status.as_str(),
                step.title
            );
            if hit(&line, &tokens) && evidence.plan.len() <= MAX_HITS {
                evidence.plan.push(line);
            }
        }
    }
    let records = crate::agent::journal::Journal::records_for(root, session).unwrap_or_default();
    for record in records.iter().rev() {
        if evidence.tools.len() + evidence.files.len() + evidence.notes.len() >= MAX_HITS * 3 {
            break;
        }
        match record.kind.as_str() {
            "tool_result" => {
                let tool = record
                    .fields
                    .get("tool")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let summary = record
                    .fields
                    .get("summary")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let line = format!("{tool}: {}", clip(summary, SUMMARY_CHARS));
                if hit(&line, &tokens) && evidence.tools.len() < MAX_HITS {
                    evidence.tools.push(line);
                }
            }
            "file_diff" => {
                let path = record
                    .fields
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                if hit(path, &tokens) && evidence.files.len() < MAX_HITS {
                    let line = match (
                        record.fields.get("added").and_then(|v| v.as_u64()),
                        record.fields.get("removed").and_then(|v| v.as_u64()),
                    ) {
                        (Some(a), Some(r)) => format!("{path} (+{a}/-{r})"),
                        _ => path.to_string(),
                    };
                    evidence.files.push(line);
                }
            }
            "note" => {
                let text = record
                    .fields
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if hit(text, &tokens) && evidence.notes.len() < MAX_HITS {
                    evidence.notes.push(clip(text, SUMMARY_CHARS).to_string());
                }
            }
            _ => {}
        }
    }
    evidence
}

impl Evidence {
    pub fn is_empty(&self) -> bool {
        self.plan.len() <= 1
            && self.tools.is_empty()
            && self.files.is_empty()
            && self.notes.is_empty()
    }
}

/// Narration prompt: evidence first, explicit stop rule, user's language.
/// The model answers from these facts or says the trail is cold.
pub fn render_prompt(evidence: &Evidence, query: &str) -> String {
    let mut out = String::from(
        "Answer the user's why-question using ONLY the host evidence below. \
         If it does not cover the question, say the trail is cold instead of inventing. \
         Answer in the user's language.\n\nQuestion:\n\"",
    );
    out.push_str(query.trim());
    out.push_str("\"\n\nHost evidence:\n");
    for line in &evidence.plan {
        out.push_str(&format!("- plan: {line}\n"));
    }
    for line in &evidence.tools {
        out.push_str(&format!("- tool: {line}\n"));
    }
    for line in &evidence.files {
        out.push_str(&format!("- file: {line}\n"));
    }
    for line in &evidence.notes {
        out.push_str(&format!("- note: {line}\n"));
    }
    out
}

pub const NARRATOR_SYSTEM: &str = "You explain past agent actions from host-supplied evidence. No prose beyond the answer; no blame, no apology theater.";
pub const NARRATOR_MAX_TOKENS: u32 = 1500;
pub const NARRATOR_TIMEOUT_SECS: u64 = 90;

/// One tool-free model call with a timeout, for the `/why` narrator:
/// schema-bound (or stop-ruled) micro-call, not a turn.
pub(crate) async fn micro_call(
    provider: &crate::providers::SharedProvider,
    model_id: &str,
    system: &str,
    prompt: &str,
    max_tokens: u32,
    timeout_secs: u64,
) -> anyhow::Result<String> {
    let request = crate::providers::ChatRequest {
        model_id: model_id.to_string(),
        system: vec![crate::providers::SystemPart::volatile(system)],
        messages: vec![crate::providers::Message::new(
            crate::providers::Role::User,
            prompt,
        )],
        // Low, not None: the gateway returns an empty completion without an
        // effort budget on some models (observed, not theorized). Cheap call
        // either way — schema-bound, no tools.
        effort: Some(crate::config::EffortLevel::Low),
        effort_support: Default::default(),
        max_tokens: Some(max_tokens),
        tools: Vec::new(),
        previous_response_id: None,
        context_transport: crate::providers::ContextTransport::Stateless,
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        collect_text(provider, &request),
    )
    .await
    .map_err(|_| anyhow::anyhow!("micro call timed out"))?
}

async fn collect_text(
    provider: &crate::providers::SharedProvider,
    request: &crate::providers::ChatRequest,
) -> anyhow::Result<String> {
    use futures::StreamExt;
    let mut stream = provider.stream_chat(request.clone());
    let mut text = String::new();
    let (mut reasoning, mut tools, mut other) = (0u32, 0u32, 0u32);
    while let Some(event) = stream.next().await {
        match event? {
            crate::providers::StreamEvent::Text(chunk) => text.push_str(&chunk),
            crate::providers::StreamEvent::Reasoning(_) => reasoning += 1,
            crate::providers::StreamEvent::ToolCall(_) => tools += 1,
            _ => other += 1,
        }
    }
    let text = text.trim().to_string();
    if text.is_empty() {
        anyhow::bail!(
            "narrator returned empty text (reasoning={reasoning} toolcalls={tools} other={other})"
        );
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> (std::path::PathBuf, String) {
        let dir = std::env::temp_dir().join(format!("sqwai-why-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut journal =
            crate::agent::journal::Journal::open(&dir, "sess").expect("journal opens");
        journal
            .append("tool_result", serde_json::json!({"tool": "bash", "ok": false, "summary": "auth tests failed: argon2 salt mismatch"}))
            .unwrap();
        journal
            .append(
                "file_diff",
                serde_json::json!({"path": "src/auth.rs", "added": 4, "removed": 1}),
            )
            .unwrap();
        journal
            .append("note", serde_json::json!({"by": "model", "note": "decision", "text": "chose argon2 for auth"}))
            .unwrap();
        (dir, "sess".to_string())
    }

    #[test]
    fn gather_finds_matching_records_by_token() {
        let (dir, session) = fixture("gather");
        let found = gather(&dir, &session, "why did auth fail");
        assert!(found.tools.iter().any(|l| l.contains("auth")), "{found:?}");
        assert!(
            found.files.iter().any(|l| l.contains("src/auth.rs")),
            "{found:?}"
        );
        assert!(
            found.notes.iter().any(|l| l.contains("argon2")),
            "{found:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn gather_stays_quiet_without_matches() {
        let (dir, session) = fixture("quiet");
        let found = gather(&dir, &session, "why is the sky blue");
        assert!(found.is_empty(), "{found:?}");
        assert!(gather(&dir, &session, "   ").is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Narrator on the live model: answers from the gathered evidence,
    /// not from memory. Run explicitly:
    /// `SQWAI_BENCH_MODEL=<key> cargo test -- --ignored why_narrator_live --test-threads=1`
    #[test]
    #[ignore]
    fn why_narrator_live() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let Some(model) = crate::agent::bench_harness::bench_model() else {
                eprintln!("SKIP: no bench model (set SQWAI_BENCH_MODEL)");
                return;
            };
            crate::providers::set_conversation_id("why-narrator-test");
            let (dir, session) = fixture("live");
            let evidence = gather(&dir, &session, "why did auth fail");
            assert!(!evidence.is_empty());
            let answer = micro_call(
                &model.provider,
                &model.model_id,
                NARRATOR_SYSTEM,
                &render_prompt(&evidence, "why did auth fail"),
                NARRATOR_MAX_TOKENS,
                NARRATOR_TIMEOUT_SECS,
            )
            .await
            .expect("narrates");
            assert!(
                answer.contains("argon2") || answer.contains("salt") || answer.contains("mismatch"),
                "answer strays from evidence: {answer}"
            );
            std::fs::remove_dir_all(&dir).ok();
        });
    }

    #[test]
    fn prompt_carries_evidence_and_stop_rule() {
        let (dir, session) = fixture("prompt");
        let prompt = render_prompt(&gather(&dir, &session, "auth"), "почему упало");
        for needle in [
            "почему упало",
            "auth tests failed",
            "src/auth.rs",
            "trail is cold",
        ] {
            assert!(prompt.contains(needle), "{prompt}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
