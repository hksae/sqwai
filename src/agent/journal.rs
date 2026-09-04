//! Host-owned append-only session journal (§2.2).

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use chrono::Utc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub seq: u64,
    pub ts: String,
    pub step: Option<String>,
    pub plan: Option<String>,
    pub agent: String,
    pub kind: String,
    #[serde(flatten)]
    pub fields: serde_json::Map<String, Value>,
}

pub struct Journal {
    path: PathBuf,
    file: File,
    next_seq: u64,
    step: Option<String>,
    plan: Option<String>,
    agent: String,
}

impl Journal {
    /// Open or create `journal/<session-id>.jsonl`, repairing a partial tail.
    pub fn open(root: &Path, session_id: &str) -> Result<Self> {
        let dir = root.join(".sqwai").join("journal");
        fs::create_dir_all(&dir).context("creating journal directory")?;
        let path = dir.join(format!("{session_id}.jsonl"));
        repair_tail(&path)?;
        let next_seq = last_seq(&path)?.saturating_add(1);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening journal {}", path.display()))?;
        Ok(Self {
            path,
            file,
            next_seq,
            step: None,
            plan: None,
            agent: "main".to_string(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn set_attribution(&mut self, step: Option<String>, plan: Option<String>, agent: &str) {
        self.step = step;
        self.plan = plan;
        self.agent = agent.to_string();
    }

    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Append one host-owned record and flush it before returning.
    pub fn append(&mut self, kind: &str, fields: Value) -> Result<u64> {
        if !fields.is_object() {
            bail!("journal fields must be a JSON object");
        }
        let mut fields = fields.as_object().cloned().unwrap_or_default();
        fields.remove("seq");
        fields.remove("ts");
        fields.remove("step");
        fields.remove("plan");
        fields.remove("agent");
        fields.remove("kind");
        let seq = self.next_seq;
        let record = Record {
            seq,
            ts: timestamp(),
            step: self.step.clone(),
            plan: self.plan.clone(),
            agent: self.agent.clone(),
            kind: kind.to_string(),
            fields,
        };
        let line = serde_json::to_string(&record).context("encoding journal record")?;
        self.file.write_all(line.as_bytes()).context("writing journal")?;
        self.file.write_all(b"\n").context("terminating journal record")?;
        self.file.flush().context("flushing journal")?;
        self.next_seq = seq.saturating_add(1);
        Ok(seq)
    }

    pub fn session_start(&mut self, model: &str, mode: &str, cwd_hash: &str) -> Result<u64> {
        self.append(
            "session_start",
            json!({"model": model, "mode": mode, "cwd_hash": cwd_hash}),
        )
    }
}

fn timestamp() -> String {
    Utc::now().to_rfc3339()
}

fn last_seq(path: &Path) -> Result<u64> {
    let Ok(file) = File::open(path) else {
        return Ok(0);
    };
    let mut last = 0;
    for line in BufReader::new(file).lines() {
        let line = line.context("reading journal")?;
        if line.trim().is_empty() {
            continue;
        }
        let record: Record = serde_json::from_str(&line).context("invalid journal record")?;
        if record.seq < last {
            bail!("journal sequence is not monotonic: {} after {last}", record.seq);
        }
        last = record.seq;
    }
    Ok(last)
}

fn repair_tail(path: &Path) -> Result<()> {
    let Ok(mut file) = OpenOptions::new().read(true).write(true).open(path) else {
        return Ok(());
    };
    let mut reader = BufReader::new(&file);
    let mut offset = 0u64;
    let mut last_complete = 0u64;
    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line).context("scanning journal")?;
        if bytes == 0 {
            break;
        }
        offset += bytes as u64;
        if line.ends_with('\n') {
            if !line[..line.len() - 1].trim().is_empty() {
                if serde_json::from_str::<Record>(line.trim_end()).is_err() {
                    last_complete = offset - bytes as u64;
                    break;
                }
            }
            last_complete = offset;
        } else {
            last_complete = offset - bytes as u64;
            break;
        }
    }
    let len = file.metadata().context("stat journal")?.len();
    if last_complete < len {
        file.seek(SeekFrom::Start(last_complete))
            .context("seeking journal repair")?;
        file.set_len(last_complete).context("truncating journal tail")?;
        file.flush().context("flushing journal repair")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "sqwai-journal-{}",
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ))
    }

    #[test]
    fn appends_monotonic_records_with_host_fields() {
        let root = root();
        let mut journal = Journal::open(&root, "session").unwrap();
        journal.set_attribution(Some("2".into()), Some("plan".into()), "main");
        assert_eq!(journal.append("tool_call", json!({"tool": "read"})).unwrap(), 1);
        assert_eq!(journal.append("tool_result", json!({"ok": true})).unwrap(), 2);
        let text = fs::read_to_string(journal.path()).unwrap();
        let records: Vec<Record> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].seq, 1);
        assert_eq!(records[1].kind, "tool_result");
        assert_eq!(records[0].step.as_deref(), Some("2"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn repairs_partial_tail_and_continues_sequence() {
        let root = root();
        let path = root.join(".sqwai").join("journal").join("session.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{\"seq\":1,\"ts\":\"1\",\"step\":null,\"plan\":null,\"agent\":\"main\",\"kind\":\"note\",\"text\":\"ok\"}\n{\"seq\":2").unwrap();
        let mut journal = Journal::open(&root, "session").unwrap();
        assert_eq!(journal.next_seq(), 2);
        assert_eq!(journal.append("note", json!({"text": "next"})).unwrap(), 2);
        let lines: Vec<_> = fs::read_to_string(journal.path()).unwrap().lines().map(str::to_string).collect();
        assert_eq!(lines.len(), 2);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn rejects_non_object_fields() {
        let root = root();
        let mut journal = Journal::open(&root, "session").unwrap();
        assert!(journal.append("bad", json!("nope")).is_err());
        fs::remove_dir_all(root).ok();
    }
}
