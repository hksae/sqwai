//! Host-owned project diary and secret screening (DESIGN §2.3).

use crate::agent::journal::Record;
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
use std::sync::OnceLock;

const MAX_DIARY_BYTES: usize = 200_000;
const DEFAULT_DIARY_TOKEN_BUDGET: u32 = 1_500;
const DEFAULT_DIARY_TIMEOUT_SECS: u64 = 30;

pub const WRITER_SYSTEM: &str = "You write a concise coding-agent diary entry. Return only Markdown using these headings when needed: ### Done, ### Decisions, ### Rejected, ### Open, ### Corrections. Copy the host block verbatim and do not invent or restate unverified numbers. Never include secrets.";

pub use crate::agent::secrets::screen;

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
/// How much of a tool's recorded summary the host block carries per command.
const HOST_SUMMARY_CHARS: usize = 80;

/// Collapse whitespace and clip, so one command stays one line.
fn one_line(text: &str, limit: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match collapsed.char_indices().nth(limit) {
        Some((cut, _)) => format!("{}…", &collapsed[..cut]),
        None => collapsed,
    }
}

/// Result claims the host block is the only source for (§2.3.2).
///
/// Deliberately narrow: only numbers that describe an outcome the host
/// recorded. Step counts and "added two tests" are the model's own reading of
/// the plan and stay untouched.
///
/// Compiled once: the post-check runs on every diary entry, and these change
/// only when the code does.
fn claim_patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            r"(?i)\d+\s+passed",
            r"(?i)\d+\s+failed",
            r"(?i)\d+\s+ignored",
            r"(?i)\d+\s+errors?",
            r"(?i)\d+\s+warnings?",
            r"(?i)exit\s+code\s+\d+",
            r"(?i)exit\s+\d+",
        ]
        .into_iter()
        .map(|source| Regex::new(source).unwrap())
        .collect()
    })
}

/// Remove lines that state a result the host block does not contain.
///
/// §2.3.2 asks for this in code, not in the prompt: "a post-check rejects an
/// entry that contains a number pattern like `\d+ passed` not present in the
/// host block". The writer prompt says the same thing in words, and §1.1 is
/// explicit that whatever can be decided by code is decided by code — the
/// model is never asked to verify its own claims.
fn strip_unverified_claims(prose: &str, host: &str) -> (String, bool) {
    let haystack = one_line(host, usize::MAX).to_ascii_lowercase();
    let patterns = claim_patterns();
    let mut removed = false;
    let kept: Vec<&str> = prose
        .lines()
        .filter(|line| {
            let unverified = patterns.iter().any(|pattern| {
                pattern.find_iter(line).any(|claim| {
                    let normalized = one_line(claim.as_str(), usize::MAX).to_ascii_lowercase();
                    !haystack.contains(&normalized)
                })
            });
            if unverified {
                removed = true;
            }
            !unverified
        })
        .collect();
    let mut out = kept.join("\n");
    if removed {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("[host: removed unverified claim]");
    }
    (out, removed)
}

pub fn host_block(root: &Path, session_id: &str, trigger: &str) -> Result<String> {
    let path = root
        .join(".sqwai")
        .join("journal")
        .join(format!("{session_id}.jsonl"));
    let boundary = last_diary_seq(root, session_id);
    let records = if path.exists() {
        read_session_records(&path)?
            .into_iter()
            .filter(|record| record.seq > boundary)
            .collect()
    } else {
        Vec::new()
    };
    render_host_block(&records, trigger)
}

fn last_diary_seq(root: &Path, session_id: &str) -> u64 {
    let path = root
        .join(".sqwai")
        .join("journal")
        .join(format!("{session_id}.jsonl"));
    read_session_records(&path)
        .ok()
        .and_then(|records| {
            records
                .into_iter()
                .rev()
                .find(|record| record.kind == "diary")
                .map(|record| record.seq)
        })
        .unwrap_or(0)
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
    // (seq, text) of every assumption note, and the seqs a later note closed
    let mut assumptions: Vec<(u64, String)> = Vec::new();
    let mut resolved: std::collections::HashSet<u64> = std::collections::HashSet::new();
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
                    // Carry the host-derived summary, not just the tick. The
                    // block is the only source of numbers the entry may use
                    // (§2.3.2), and without this it had none — so the
                    // post-check below would delete every true test count
                    // along with the invented ones.
                    let detail = record
                        .fields
                        .get("summary")
                        .and_then(Value::as_str)
                        .map(|summary| one_line(summary, HOST_SUMMARY_CHARS))
                        .filter(|summary| !summary.is_empty())
                        .map(|summary| format!(" ({summary})"))
                        .unwrap_or_default();
                    commands.push(format!("{tool} {}{detail}", if ok { "✓" } else { "✗" }));
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
            "note" => {
                if let Some(seq) = record.fields.get("resolves").and_then(Value::as_u64) {
                    resolved.insert(seq);
                }
                match record.fields.get("note").and_then(Value::as_str) {
                    Some("decision") => notes[0] += 1,
                    Some("rejected") => notes[1] += 1,
                    Some("assumption") => {
                        notes[2] += 1;
                        if record.fields.get("resolves").is_none() {
                            assumptions.push((
                                record.seq,
                                record
                                    .fields
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                            ));
                        }
                    }
                    Some("lesson") => notes[3] += 1,
                    Some("blocker") => notes[4] += 1,
                    _ => {}
                }
            }
            _ => {}
        }
    }
    let open_assumptions: Vec<String> = assumptions
        .iter()
        .filter(|(seq, _)| !resolved.contains(seq))
        .take(6)
        .map(|(seq, text)| {
            let mut short: String = text.chars().take(120).collect();
            if text.chars().count() > 120 {
                short.push('…');
            }
            format!("j#{seq}: {short}")
        })
        .collect();

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
    // §2.1.4: the host block surfaces open assumptions, so tomorrow's session
    // inherits them as facts rather than as something to rediscover. Counting
    // them is not enough — an assumption nobody can read is not one anyone
    // will resolve.
    output.push_str(&format!(
        "open assumptions: {}\n",
        if open_assumptions.is_empty() {
            "none".to_string()
        } else {
            open_assumptions.join(" · ")
        }
    ));
    output.push_str(&format!("trigger: {trigger}\n<!-- /host -->"));
    Ok(output)
}

/// Append a host-only diary entry. Model-generated prose is screened first.
#[allow(clippy::too_many_arguments)] // all parameters are required to build the diary entry
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
    // effort for this call; the diary is a cheap summariser by default (§5.1)
    effort: crate::config::EffortLevel,
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
            effort: Some(effort).filter(|l| *l != crate::config::EffortLevel::Off),
            effort_support: Default::default(),
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
    Ok(prose.is_some())
}

async fn collect_writer_text(provider: &SharedProvider, request: &ChatRequest) -> Result<String> {
    let mut stream = provider.stream_chat(request.clone());
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        if let StreamEvent::Text(chunk) = event? {
            text.push_str(&chunk);
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
    append_host_entry(root, date, session_id, trigger, &host, prose)
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
    let body = prose
        .map(|text| screen(&strip_unverified_claims(text, host).0).text)
        .unwrap_or_else(|| {
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

#[allow(dead_code)]
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
    fn memory_read_rejects_path_traversal_dates() {
        let error = read_day(&root(), "../2026-09-04").unwrap_err();
        assert!(error.contains("YYYY-MM-DD"));
    }

    /// §2.3.2 asks for this in code, and it was only in the prompt: the writer
    /// was told "do not invent or restate unverified numbers" and nothing
    /// checked. §1.1 is explicit that the model is never asked to verify its
    /// own claims.
    #[test]
    fn a_result_the_host_block_does_not_contain_is_removed() {
        let host = "<!-- host -->\nfiles: src/a.rs (+3/-1)\n\
                    commands: bash ✓ ((exit code 0) test result: ok. 253 passed; 0 failed)\n";
        let prose = "### Done\n\
                     - Wired the validator; the suite is green with 253 passed.\n\
                     - Also fixed the flake, 61 passed after the change.\n\
                     - Steps 1-3 done, step 4 blocked on a keybinding.\n";

        let (kept, removed) = strip_unverified_claims(prose, host);
        assert!(removed);
        assert!(
            kept.contains("253 passed"),
            "a number the host block does contain must stay: {kept}"
        );
        assert!(
            !kept.contains("61 passed"),
            "a number it does not contain must go: {kept}"
        );
        assert!(
            kept.contains("Steps 1-3 done"),
            "plan facts are not result claims and must survive: {kept}"
        );
        assert!(kept.contains("[host: removed unverified claim]"));
    }

    /// Nothing to remove means nothing is marked: the marker has to mean
    /// something when it shows up.
    #[test]
    fn an_entry_within_the_host_block_is_left_alone() {
        let host = "commands: bash ✓ ((exit code 0) 253 passed)\n";
        let prose = "### Done\n- 253 passed after the change.\n";
        let (kept, removed) = strip_unverified_claims(prose, host);
        assert!(!removed);
        assert_eq!(kept, prose.trim_end());
        assert!(!kept.contains("[host:"));
    }

    /// The block is the source of those numbers, so it has to carry them. It
    /// used to record only the tool name and a tick, which would have made the
    /// post-check delete every true test count in the entry.
    #[test]
    fn host_block_carries_the_recorded_summary() {
        let records = vec![Record {
            seq: 1,
            ts: String::new(),
            step: None,
            plan: None,
            agent: "main".into(),
            kind: "tool_result".into(),
            fields: serde_json::json!({
                "tool": "bash",
                "ok": true,
                "summary": "(exit code 0)\ntest result: ok. 253 passed; 0 failed; 1 ignored",
            })
            .as_object()
            .cloned()
            .unwrap(),
        }];
        let block = render_host_block(&records, "compaction").unwrap();
        assert!(block.contains("bash ✓"), "{block}");
        assert!(block.contains("253 passed"), "{block}");
        assert_eq!(
            block
                .lines()
                .filter(|line| line.contains("commands:"))
                .count(),
            1,
            "{block}"
        );
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
            crate::config::EffortLevel::Off,
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
