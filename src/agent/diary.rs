//! Host-owned project diary and secret screening (DESIGN §2.3).

use crate::agent::journal::{Journal, Record};
use crate::providers::{
    ChatRequest, ContextTransport, Message, SharedProvider, StreamEvent, SystemPart,
};
use anyhow::{Context, Result};
use chrono::{Local, NaiveDate};
use futures::StreamExt;
use regex::Regex;
use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

const MAX_DIARY_BYTES: usize = 200_000;
const DEFAULT_DIARY_TOKEN_BUDGET: u32 = 1_500;
const DEFAULT_DIARY_TIMEOUT_SECS: u64 = 30;

pub const WRITER_SYSTEM: &str = "You write a concise coding-agent diary entry. Return only Markdown using these headings when needed: ### Done, ### Decisions, ### Rejected, ### Open, ### Corrections. Copy the host block verbatim and do not invent or restate unverified numbers. Never include secrets.";

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
        let token = found.as_str();
        if shannon_entropy(token) > 4.0 {
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

pub fn memory_dir(root: &Path) -> PathBuf {
    root.join(".sqwai").join("memory")
}

pub fn diary_path(root: &Path, date: NaiveDate) -> PathBuf {
    memory_dir(root).join(format!("{date}.md"))
}

/// Read one diary day. Dates are deliberately strict to keep the path jailed.
pub fn read_day(root: &Path, date: &str) -> Result<String, String> {
    let parsed = NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map_err(|_| "memory_read date must be YYYY-MM-DD".to_string())?;
    let path = diary_path(root, parsed);
    let text = fs::read_to_string(&path).map_err(|e| format!("memory_read failed: {e}"))?;
    if text.len() > MAX_DIARY_BYTES {
        return Ok(
            text[..text.floor_char_boundary(MAX_DIARY_BYTES)].to_string() + "\n[diary truncated]",
        );
    }
    Ok(text)
}

/// Build deterministic host facts from a session journal.
pub fn host_block(root: &Path, session_id: &str, trigger: &str) -> Result<String> {
    let path = root
        .join(".sqwai")
        .join("journal")
        .join(format!("{session_id}.jsonl"));
    let records = if path.exists() {
        let records = read_session_records(&path)?;
        let start = records
            .iter()
            .rposition(|record| record.kind == "diary")
            .map(|index| index + 1)
            .unwrap_or(0);
        records[start..].to_vec()
    } else {
        Vec::new()
    };
    render_host_block(&records, trigger)
}

fn read_session_records(path: &Path) -> Result<Vec<Record>> {
    let file =
        fs::File::open(path).with_context(|| format!("opening journal {}", path.display()))?;
    BufReader::new(file)
        .lines()
        .filter_map(|line| match line {
            Ok(line) if !line.trim().is_empty() => Some(
                serde_json::from_str(&line)
                    .with_context(|| format!("decoding journal record in {}", path.display())),
            ),
            Ok(_) => None,
            Err(error) => Some(Err(error).context("reading session journal")),
        })
        .collect()
}

fn render_host_block(records: &[Record], trigger: &str) -> Result<String> {
    let mut files = Vec::new();
    let mut commands = Vec::new();
    let mut checkpoints = Vec::new();
    let mut diagnostics = 0usize;
    let mut notes = [0usize; 5];
    let mut compactions = 0usize;
    let mut undo = 0usize;
    let mut first_seq = None;
    let mut last_seq = None;
    for record in records {
        first_seq.get_or_insert(record.seq);
        last_seq = Some(record.seq);
        match record.kind.as_str() {
            "file_diff" => {
                let path = record
                    .fields
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                let added = record
                    .fields
                    .get("added")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let removed = record
                    .fields
                    .get("removed")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                files.push(format!("{path} (+{added}/-{removed})"));
            }
            "tool_result" => {
                if let Some(tool) = record.fields.get("tool").and_then(Value::as_str) {
                    let ok = record
                        .fields
                        .get("ok")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    commands.push(format!("{tool} {}", if ok { "✓" } else { "✗" }));
                }
            }
            "checkpoint" => {
                if let Some(id) = record.fields.get("id").and_then(Value::as_str) {
                    checkpoints.push(id.to_string());
                }
            }
            "diagnostics" => diagnostics += 1,
            "compaction" => compactions += 1,
            "undo" => undo += 1,
            "note" => match record.fields.get("note").and_then(Value::as_str) {
                Some("decision") => notes[0] += 1,
                Some("rejected") => notes[1] += 1,
                Some("assumption") => notes[2] += 1,
                Some("lesson") => notes[3] += 1,
                Some("blocker") => notes[4] += 1,
                _ => {}
            },
            _ => {}
        }
    }
    let mut output = String::new();
    output.push_str("<!-- host -->\n");
    output.push_str(&format!(
        "journal: j#{}–j#{}\n",
        first_seq.unwrap_or(0),
        last_seq.unwrap_or(0)
    ));
    output.push_str(&format!(
        "files: {}\n",
        if files.is_empty() {
            "none".into()
        } else {
            files.join(" · ")
        }
    ));
    output.push_str(&format!(
        "commands: {}\n",
        if commands.is_empty() {
            "none".into()
        } else {
            commands.join(" · ")
        }
    ));
    output.push_str(&format!(
        "checkpoints: {} · compactions: {compactions} · undo: {undo}\n",
        if checkpoints.is_empty() {
            "none".into()
        } else {
            checkpoints.join(" · ")
        }
    ));
    output.push_str(&format!("diagnostics: {diagnostics} records\n"));
    output.push_str(&format!(
        "notes: {} decision · {} rejected · {} assumption · {} lesson · {} blocker\n",
        notes[0], notes[1], notes[2], notes[3], notes[4]
    ));
    output.push_str(&format!("trigger: {trigger}\n<!-- /host -->"));
    Ok(output)
}

/// Append a host-only diary entry. Model-generated prose is screened first.
pub async fn write_entry(
    root: &Path,
    date: NaiveDate,
    session_id: &str,
    trigger: &str,
    provider: Option<&SharedProvider>,
    model_id: &str,
    plan_snapshot: Option<&str>,
    notes: Option<&str>,
    last_user_message: Option<&str>,
    token_budget: Option<u32>,
    timeout: Option<std::time::Duration>,
) -> Result<bool> {
    let host = host_block(root, session_id, trigger)?;
    let prompt = format!(
        "Host block:\n{host}\n\nPlan snapshot:\n{}\n\nNotes since last entry:\n{}\n\nLast user message:\n{}\n\nWrite the diary sections now.",
        plan_snapshot.unwrap_or("none"),
        notes.unwrap_or("none"),
        last_user_message.unwrap_or("none"),
    );
    let prose = if let Some(provider) = provider {
        let request = ChatRequest {
            model_id: model_id.to_string(),
            system: vec![SystemPart::volatile(WRITER_SYSTEM)],
            messages: vec![Message::new(crate::providers::Role::User, prompt)],
            thinking: None,
            max_tokens: Some(token_budget.unwrap_or(DEFAULT_DIARY_TOKEN_BUDGET)),
            tools: Vec::new(),
            previous_response_id: None,
            context_transport: ContextTransport::Stateless,
        };
        let collect = collect_writer_text(provider, &request);
        match tokio::time::timeout(
            timeout.unwrap_or(std::time::Duration::from_secs(DEFAULT_DIARY_TIMEOUT_SECS)),
            collect,
        )
        .await
        {
            Ok(Ok(text)) => Some(text),
            Ok(Err(error)) => {
                crate::providers::log_http(&format!("diary: writer failed: {error}"));
                None
            }
            Err(_) => {
                crate::providers::log_http("diary: writer timed out");
                None
            }
        }
    } else {
        None
    };
    append_host_entry(root, date, session_id, trigger, &host, prose.as_deref())?;
    mark_diary(root, session_id, trigger)?;
    Ok(prose.is_some())
}

async fn collect_writer_text(provider: &SharedProvider, request: &ChatRequest) -> Result<String> {
    let mut stream = provider.stream_chat(request.clone());
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        match event? {
            StreamEvent::Text(chunk) => text.push_str(&chunk),
            _ => {}
        }
    }
    let text = text.trim().to_string();
    if text.is_empty() {
        anyhow::bail!("the diary writer returned empty text")
    }
    Ok(text)
}

pub fn append_entry(
    root: &Path,
    date: NaiveDate,
    session_id: &str,
    trigger: &str,
    prose: Option<&str>,
) -> Result<()> {
    let host = host_block(root, session_id, trigger)?;
    append_host_entry(root, date, session_id, trigger, &host, prose)?;
    mark_diary(root, session_id, trigger)
}

fn mark_diary(root: &Path, session_id: &str, trigger: &str) -> Result<()> {
    let mut journal = Journal::open(root, session_id)?;
    journal.set_attribution(None, None, "host");
    journal.append("diary", serde_json::json!({"trigger": trigger}))?;
    Ok(())
}

fn append_host_entry(
    root: &Path,
    date: NaiveDate,
    session_id: &str,
    trigger: &str,
    host: &str,
    prose: Option<&str>,
) -> Result<()> {
    let date_path = diary_path(root, date);
    fs::create_dir_all(memory_dir(root)).context("creating diary directory")?;
    let heading = format!(
        "## {} · session {session_id} · trigger {trigger}\n",
        Local::now().format("%H:%M")
    );
    let body = prose.map(|text| screen(text).text).unwrap_or_else(|| {
        "\nmode: host_only\n\n### Done\n- Host-only entry; no model summary was available.\n"
            .to_string()
    });
    let entry = format!("\n{heading}{host}\n{body}\n");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&date_path)
        .with_context(|| format!("opening diary {}", date_path.display()))?;
    file.write_all(entry.as_bytes()).context("writing diary")?;
    file.flush().context("flushing diary")?;
    Ok(())
}

pub fn validate_date(date: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(date, "%Y-%m-%d").map_err(|_| "date must be YYYY-MM-DD".into())
}

pub fn today() -> NaiveDate {
    Local::now().date_naive()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "sqwai-diary-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn redacts_known_secret_shapes_and_entropy_tokens() {
        let value =
            screen("AKIA1234567890ABCDEF Bearer abcdefghijklmnop qwertyuiopasdfghjklzxcvbnm");
        assert!(value.redacted);
        assert!(!value.text.contains("AKIA"));
        assert!(!value.text.contains("Bearer"));
    }

    #[test]
    fn memory_read_rejects_path_traversal_dates() {
        let error = read_day(&root(), "../2026-09-04").unwrap_err();
        assert!(error.contains("YYYY-MM-DD"));
    }

    #[tokio::test]
    async fn writer_falls_back_to_host_only_without_provider() {
        let dir = root();
        let written = write_entry(
            &dir,
            NaiveDate::from_ymd_opt(2026, 9, 4).unwrap(),
            "missing-session",
            "manual",
            None,
            "model",
            None,
            None,
            None,
            Some(8),
            Some(std::time::Duration::from_millis(1)),
        )
        .await
        .unwrap();
        assert!(!written);
        let text = fs::read_to_string(diary_path(
            &dir,
            NaiveDate::from_ymd_opt(2026, 9, 4).unwrap(),
        ))
        .unwrap();
        assert!(text.contains("mode: host_only"));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn host_block_counts_records() {
        let records = vec![
            Record {
                seq: 1,
                ts: "".into(),
                step: None,
                plan: None,
                agent: "main".into(),
                kind: "file_diff".into(),
                fields: serde_json::from_value(json!({"path":"src/lib.rs","added":2,"removed":1}))
                    .unwrap(),
            },
            Record {
                seq: 2,
                ts: "".into(),
                step: None,
                plan: None,
                agent: "main".into(),
                kind: "tool_result".into(),
                fields: serde_json::from_value(json!({"tool":"cargo test","ok":true})).unwrap(),
            },
        ];
        let block = render_host_block(&records, "manual").unwrap();
        assert!(block.contains("src/lib.rs (+2/-1)"));
        assert!(block.contains("cargo test ✓"));
        assert!(block.contains("trigger: manual"));
    }
}
