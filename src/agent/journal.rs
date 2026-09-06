//! Host-owned append-only session journal (§2.2).

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

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

/// What a per-step revert can and cannot put back (§2.5).
#[derive(Debug, Default, Clone)]
pub struct StepRevert {
    /// pre-images that can be restored without disturbing another step
    pub files: Vec<PreImage>,
    /// paths this step wrote that a later step wrote again; reverting them
    /// alone would undo that later step too, so they are refused by name
    pub written_since: Vec<String>,
}

/// What one file looked like before an undone window touched it (§2.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreImage {
    pub path: String,
    /// layer-1 blob to write back, when one was stored
    pub blob_before: Option<String>,
    /// whether the file existed before this window at all. Without it, a
    /// record that has no blob is ambiguous: it could be a file the agent
    /// created (revert = delete) or a record written before the blob store
    /// existed (revert = impossible). Deleting in the second case would
    /// destroy a file the agent had merely edited.
    pub existed_before: bool,
    /// `hash_after` of the host's last write, for the outside-edit check
    pub agent_hash: Option<String>,
}

impl Journal {
    /// Open or create `journal/<session-id>.jsonl`, repairing a partial tail.
    pub fn open(root: &Path, session_id: &str) -> Result<Self> {
        let dir = root.join(".sqwai").join("journal");
        fs::create_dir_all(&dir).context("creating journal directory")?;
        let path = dir.join(format!("{session_id}.jsonl"));
        let repaired = repair_tail(&path)?;
        let next_seq = last_seq(&path)?.saturating_add(1);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening journal {}", path.display()))?;
        let mut journal = Self {
            path,
            file,
            next_seq,
            step: None,
            plan: None,
            agent: "main".to_string(),
        };
        if repaired > 0 {
            journal.append(
                "journal_repair",
                json!({"truncated_bytes": repaired, "by": "host"}),
            )?;
        }
        Ok(journal)
    }

    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn set_attribution(&mut self, step: Option<String>, plan: Option<String>, agent: &str) {
        self.step = step;
        self.plan = plan;
        self.agent = agent.to_string();
    }

    #[allow(dead_code)]
    pub fn attribution(&self) -> (Option<String>, Option<String>) {
        (self.step.clone(), self.plan.clone())
    }

    /// Append a host-owned record and, when a step is in progress, atomically
    /// attach it to that step's evidence (§2.1.4).
    ///
    /// The record is written either way. With nothing in progress it carries
    /// `step: null` and counts as a session fact — for the diary host block and
    /// the compaction anchor — but never as evidence (§2.2.3).
    pub fn append_evidence(&mut self, kind: &str, fields: Value) -> Result<u64> {
        let seq = self.append(kind, fields)?;
        let (Some(step_id), Some(plan_id)) = (self.step.clone(), self.plan.clone()) else {
            return Ok(seq);
        };
        if !matches!(kind, "tool_result" | "file_diff" | "diagnostics") {
            return Ok(seq);
        }
        let root = self
            .path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .context("journal path has no project root")?;
        let mut active = match crate::plan::open_active(root)? {
            Some(plan) if plan.id == plan_id => plan,
            _ => return Ok(seq),
        };
        let session = self
            .path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string();
        if let Some(step) = active.step_mut(&step_id)
            && !step
                .evidence
                .iter()
                .any(|reference| reference.session == session && reference.seq == seq)
        {
            step.evidence
                .push(crate::plan::EvidenceRef { session, seq });
            active.revision += 1;
            crate::plan::store(root, &active)?;
        }
        Ok(seq)
    }

    #[allow(dead_code)]
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Read records from every journal belonging to this project.
    pub fn records(root: &Path) -> Result<Vec<Record>> {
        let dir = root.join(".sqwai").join("journal");
        let mut records = Vec::new();
        let Ok(entries) = fs::read_dir(dir) else {
            return Ok(records);
        };
        for entry in entries.flatten() {
            if entry.path().extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let file = File::open(entry.path()).context("opening journal for evidence")?;
            for line in BufReader::new(file).lines() {
                let line = line.context("reading journal for evidence")?;
                if !line.trim().is_empty() {
                    records.push(serde_json::from_str(&line).context("decoding journal evidence")?);
                }
            }
        }
        Ok(records)
    }

    /// Read records from one session journal only.
    pub fn records_for(root: &Path, session_id: &str) -> Result<Vec<Record>> {
        let path = root
            .join(".sqwai")
            .join("journal")
            .join(format!("{session_id}.jsonl"));
        let Ok(file) = File::open(path) else {
            return Ok(Vec::new());
        };
        BufReader::new(file)
            .lines()
            .filter_map(|line| match line {
                Ok(line) if !line.trim().is_empty() => {
                    Some(serde_json::from_str(&line).context("decoding session journal record"))
                }
                Ok(_) => None,
                Err(error) => Some(Err(error.into())),
            })
            .collect()
    }

    /// Check that a sequence belongs to this plan and is useful evidence.
    pub fn evidence(
        root: &Path,
        plan: &str,
        step: Option<&str>,
        reference: &crate::plan::EvidenceRef,
        after_seq: Option<u64>,
    ) -> Result<Option<Record>> {
        let records = if reference.session.is_empty() {
            Self::records(root)?
        } else {
            Self::records_for(root, &reference.session)?
        };
        Ok(records.into_iter().find(|r| {
            r.seq == reference.seq
                && after_seq.is_none_or(|start| r.seq > start)
                && r.plan.as_deref() == Some(plan)
                && step.is_none_or(|expected| r.step.as_deref() == Some(expected))
                && matches!(r.kind.as_str(), "tool_result" | "file_diff" | "diagnostics")
        }))
    }

    /// Paths the host recorded as its own writes for the given checkpoints,
    /// each with the hash the agent left the file at.
    ///
    /// This is what scopes `/undo`: the file tools journal a `file_diff` per
    /// mutation carrying the checkpoint it belongs to and `hash_after`, so undo
    /// can put back exactly those files and leave everything else — including
    /// whatever the user edited meanwhile — alone.
    ///
    /// Returns an empty list when nothing is recorded, which is the honest
    /// answer for a `bash` mutation: the host cannot enumerate what a shell
    /// command touched, so there is nothing to narrow the scope with.
    pub fn recorded_writes(
        root: &Path,
        checkpoints: &[String],
    ) -> Result<Vec<(String, Option<String>)>> {
        let mut out: Vec<(String, Option<String>)> = Vec::new();
        for record in Self::records(root)? {
            if record.kind != "file_diff" {
                continue;
            }
            let belongs = record
                .fields
                .get("checkpoint")
                .and_then(Value::as_str)
                .is_some_and(|sha| checkpoints.iter().any(|wanted| wanted == sha));
            if !belongs {
                continue;
            }
            let Some(path) = record.fields.get("path").and_then(Value::as_str) else {
                continue;
            };
            let hash = record
                .fields
                .get("hash_after")
                .and_then(Value::as_str)
                .map(str::to_string);
            // Later records win: a file written twice in the undone window was
            // last left at the newest hash.
            match out.iter_mut().find(|(known, _)| known == path) {
                Some(slot) => slot.1 = hash,
                None => out.push((path.to_string(), hash)),
            }
        }
        Ok(out)
    }

    /// The layer-1 pre-image of every file the host wrote in `checkpoints`,
    /// oldest first per path — that is the content the file had *before* the
    /// undone window started, so a scoped revert writes the earliest blob and
    /// not the latest.
    ///
    /// Returned per path: the blob id to write back (`None` when the file did
    /// not exist before the window, i.e. it was created and should be
    /// removed), and the hash the host last left it at, so a file edited
    /// outside sqwai afterwards can be skipped instead of overwritten.
    pub fn recorded_pre_images(root: &Path, checkpoints: &[String]) -> Result<Vec<PreImage>> {
        let mut out: Vec<PreImage> = Vec::new();
        for record in Self::records(root)? {
            if record.kind != "file_diff" {
                continue;
            }
            let belongs = record
                .fields
                .get("checkpoint")
                .and_then(Value::as_str)
                .is_some_and(|sha| checkpoints.iter().any(|wanted| wanted == sha));
            if !belongs {
                continue;
            }
            let Some(path) = record.fields.get("path").and_then(Value::as_str) else {
                continue;
            };
            let field = |name: &str| {
                record
                    .fields
                    .get(name)
                    .and_then(Value::as_str)
                    .map(str::to_string)
            };
            match out.iter_mut().find(|found| found.path == path) {
                // The first record for a path holds the state to return to;
                // later ones only move the "as the host left it" hash forward.
                Some(slot) => slot.agent_hash = field("hash_after"),
                None => out.push(PreImage {
                    path: path.to_string(),
                    blob_before: field("blob_before"),
                    existed_before: field("hash_before").is_some(),
                    agent_hash: field("hash_after"),
                }),
            }
        }
        Ok(out)
    }

    /// Pre-images for one plan step, and what stands in the way of using them.
    ///
    /// §2.5: reverting a single step needs layer 1 only — *"restore
    /// `hash_before` from the blob store if the file has not been touched by
    /// any other steps since (verified via the journal's `file_diff` chain)"*.
    /// That proviso is the whole difficulty: a file the step wrote and a later
    /// step then rewrote cannot be reverted in isolation, because putting the
    /// old bytes back would silently undo the later step as well.
    ///
    /// Returns the revertible pre-images and, separately, the paths a later
    /// step has since written, so the caller can refuse and say which.
    pub fn step_pre_images(root: &Path, step: &str) -> Result<StepRevert> {
        let records = Self::records(root)?;
        let mut revert = StepRevert::default();
        let mut last_seq_of_step = 0u64;

        for record in &records {
            if record.kind != "file_diff" || record.step.as_deref() != Some(step) {
                continue;
            }
            last_seq_of_step = last_seq_of_step.max(record.seq);
            let Some(path) = record.fields.get("path").and_then(Value::as_str) else {
                continue;
            };
            let field = |name: &str| {
                record
                    .fields
                    .get(name)
                    .and_then(Value::as_str)
                    .map(str::to_string)
            };
            match revert.files.iter_mut().find(|found| found.path == path) {
                // first record wins for the pre-image, last for the hash the
                // host left the file at
                Some(slot) => slot.agent_hash = field("hash_after"),
                None => revert.files.push(PreImage {
                    path: path.to_string(),
                    blob_before: field("blob_before"),
                    existed_before: field("hash_before").is_some(),
                    agent_hash: field("hash_after"),
                }),
            }
        }

        // Anything written after this step's last record, by a different step,
        // makes that path un-revertible on its own.
        for record in &records {
            if record.kind != "file_diff"
                || record.seq <= last_seq_of_step
                || record.step.as_deref() == Some(step)
            {
                continue;
            }
            if let Some(path) = record.fields.get("path").and_then(Value::as_str)
                && revert.files.iter().any(|item| item.path == path)
                && !revert.written_since.iter().any(|known| known == path)
            {
                revert.written_since.push(path.to_string());
            }
        }
        revert
            .files
            .retain(|item| !revert.written_since.contains(&item.path));
        Ok(revert)
    }

    /// Record a per-step revert (§2.2.2's `undo` kind, `step` variant).
    pub fn append_undo_step(
        &mut self,
        step: &str,
        files: &[String],
        reopened_steps: &[String],
    ) -> Result<u64> {
        self.append(
            "undo",
            json!({
                "step": step,
                "files": files,
                "reopened_steps": reopened_steps,
            }),
        )
    }

    /// Every layer-1 blob any journal in this project still names.
    ///
    /// Retention reads this rather than the current session's journal alone:
    /// a resumed session, or a plan whose evidence points at an older
    /// session's records, must not have its pre-images collected out from
    /// under it (§2.5).
    pub fn referenced_blobs(root: &Path) -> Result<std::collections::HashSet<String>> {
        let mut ids = std::collections::HashSet::new();
        for record in Self::records(root)? {
            if record.kind != "file_diff" {
                continue;
            }
            for field in ["blob_before", "blob_after"] {
                if let Some(id) = record.fields.get(field).and_then(Value::as_str) {
                    ids.insert(id.to_string());
                }
            }
        }
        Ok(ids)
    }

    /// Session ids that still have a journal on disk. A chain whose journal is
    /// gone has nothing left that could reference its checkpoints, which is
    /// what §2.5 means by blobs being *"purged together with the session
    /// journal"*.
    pub fn sessions_on_disk(root: &Path) -> Vec<String> {
        let dir = root.join(".sqwai").join("journal");
        let Ok(entries) = fs::read_dir(dir) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                (path.extension().and_then(|s| s.to_str()) == Some("jsonl"))
                    .then(|| path.file_stem()?.to_str().map(str::to_string))
                    .flatten()
            })
            .collect()
    }

    /// Which of `checkpoints` have at least one recorded write.
    ///
    /// The complement is what `/undo` cannot account for: a `bash` checkpoint
    /// produces no `file_diff`, so its effects survive a scoped restore and the
    /// user has to be told rather than left to find out.
    pub fn checkpoints_with_writes(root: &Path, checkpoints: &[String]) -> Result<Vec<String>> {
        let mut found: Vec<String> = Vec::new();
        for record in Self::records(root)? {
            if record.kind != "file_diff" {
                continue;
            }
            if let Some(sha) = record.fields.get("checkpoint").and_then(Value::as_str)
                && checkpoints.iter().any(|wanted| wanted == sha)
                && !found.iter().any(|known| known == sha)
            {
                found.push(sha.to_string());
            }
        }
        Ok(found)
    }

    /// Return the journal sequence of a step's host-recorded start operation.
    pub fn step_started_at(root: &Path, plan: &str, step: &str) -> Result<Option<u64>> {
        Ok(Self::records(root)?
            .into_iter()
            .filter(|r| {
                r.plan.as_deref() == Some(plan)
                    && r.step.as_deref() == Some(step)
                    && r.kind == "plan"
                    && r.fields.get("op").and_then(Value::as_str) == Some("start")
            })
            .map(|r| r.seq)
            .max())
    }

    /// Return a non-blocking reminder when a step has accumulated actions
    /// since its last plan operation.
    pub fn nudge(root: &Path, threshold: usize) -> Result<Option<String>> {
        let records = Self::records(root)?;
        let active = crate::plan::open_active(root)?;
        let Some(active) = active else {
            return Ok(None);
        };
        let Some(step) = active
            .steps
            .iter()
            .find(|step| step.status == crate::plan::StepStatus::InProgress)
        else {
            return Ok(None);
        };
        let mut actions = 0usize;
        for record in records.iter().rev() {
            if record.plan.as_deref() != Some(active.id.as_str())
                || record.step.as_deref() != Some(step.id.as_str())
            {
                continue;
            }
            if record.kind == "plan" {
                break;
            }
            if matches!(record.kind.as_str(), "file_diff" | "tool_result") {
                actions += 1;
            }
        }
        if actions >= threshold {
            Ok(Some(format!(
                "plan: step {} has {actions} actions and no update — finish, split or block it.",
                step.id
            )))
        } else {
            Ok(None)
        }
    }

    /// Append one host-owned record and flush it before returning.
    pub fn append(&mut self, kind: &str, fields: Value) -> Result<u64> {
        if !fields.is_object() {
            bail!("journal fields must be a JSON object");
        }
        let mut fields = fields.as_object().cloned().unwrap_or_default();
        // §2.2.2: "no secrets (the same screening as §2.3.6 applies to
        // `summary` and `text`)". Those two carry command output and model
        // prose; everything else is a path, a hash or a count the host built
        // itself, and screening those would only mangle facts.
        for name in ["summary", "text"] {
            if let Some(Value::String(raw)) = fields.get(name) {
                let screened = crate::agent::secrets::screen(raw);
                if screened.redacted {
                    fields.insert(name.to_string(), Value::String(screened.text));
                }
            }
        }
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
        self.file
            .write_all(line.as_bytes())
            .context("writing journal")?;
        self.file
            .write_all(b"\n")
            .context("terminating journal record")?;
        self.file.flush().context("flushing journal")?;
        self.next_seq = seq.saturating_add(1);
        Ok(seq)
    }

    pub fn append_undo(
        &mut self,
        checkpoint: &str,
        files: &[String],
        reopened_steps: &[String],
    ) -> Result<u64> {
        self.append(
            "undo",
            json!({
                "to_checkpoint": checkpoint,
                "files": files,
                "reopened_steps": reopened_steps,
            }),
        )
    }

    pub fn session_start(
        &mut self,
        model: &str,
        mode: &str,
        head: Option<&str>,
        cwd_hash: &str,
        resumed_from: Option<&str>,
    ) -> Result<u64> {
        self.append(
            "session_start",
            json!({
                "model": model,
                "mode": mode,
                "head": head,
                "cwd_hash": cwd_hash,
                "resumed_from": resumed_from,
            }),
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
            bail!(
                "journal sequence is not monotonic: {} after {last}",
                record.seq
            );
        }
        last = record.seq;
    }
    Ok(last)
}

/// Truncate a partial trailing line, reporting how many bytes went.
///
/// A crash mid-append leaves half a record; §2.2.1 has the reader drop it. It
/// also has the host write a `journal_repair` record, which is the part that
/// was missing: an append-only log that exists for auditing was quietly losing
/// bytes with nothing to show it happened.
fn repair_tail(path: &Path) -> Result<u64> {
    let Ok(mut file) = OpenOptions::new().read(true).write(true).open(path) else {
        return Ok(0);
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
            if !line[..line.len() - 1].trim().is_empty()
                && serde_json::from_str::<Record>(line.trim_end()).is_err()
            {
                last_complete = offset - bytes as u64;
                break;
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
        file.set_len(last_complete)
            .context("truncating journal tail")?;
        file.flush().context("flushing journal repair")?;
        return Ok(len - last_complete);
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    fn root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "sqwai-journal-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn appends_monotonic_records_with_host_fields() {
        let root = root();
        let mut journal = Journal::open(&root, "session").unwrap();
        journal.set_attribution(Some("2".into()), Some("plan".into()), "main");
        assert_eq!(
            journal
                .append("tool_call", json!({"tool": "read"}))
                .unwrap(),
            1
        );
        assert_eq!(
            journal.append("tool_result", json!({"ok": true})).unwrap(),
            2
        );
        let text = fs::read_to_string(journal.path()).unwrap();
        let records: Vec<Record> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].seq, 1);
        assert_eq!(records[1].kind, "tool_result");
        assert_eq!(records[0].step.as_deref(), Some("2"));
        fs::remove_dir_all(root).ok();
    }

    /// §2.2.2 forbids secrets in the journal. The filter existed and was wired
    /// to the diary and to MEMORY.md, but not here — so a command that echoed
    /// a token wrote it verbatim into `.sqwai/journal/*.jsonl`, and from there
    /// into the diary host block and the compaction anchor, both of which are
    /// built from these records.
    #[test]
    fn secrets_are_screened_out_of_summaries_and_notes() {
        let root = root();
        let mut journal = Journal::open(&root, "session").unwrap();
        journal
            .append(
                "tool_result",
                json!({
                    "tool": "bash",
                    "ok": true,
                    "summary": "export ANTHROPIC_API_KEY=sk-ant-api03-abcdefghijklmnopqrstuvwxyz012345",
                }),
            )
            .unwrap();
        journal
            .append(
                "note",
                json!({"by": "model", "note": "lesson", "text": "the token is ghp_abcdefghijklmnopqrstuvwxyz0123456789"}),
            )
            .unwrap();
        // a path is not a secret and has to survive, or the record stops being
        // able to say what happened
        journal
            .append(
                "tool_result",
                json!({"tool": "write", "ok": true, "summary": "wrote .sqwai/plans/01M1V0GK22W0PFVYBM0501N1FJ.json (+31/-31)"}),
            )
            .unwrap();

        let raw = fs::read_to_string(root.join(".sqwai/journal/session.jsonl")).unwrap();
        assert!(!raw.contains("sk-ant-api03"), "the key reached the journal");
        assert!(!raw.contains("ghp_abcdef"), "the token reached the journal");
        assert!(raw.contains("[redacted]"));
        assert!(
            raw.contains("01M1V0GK22W0PFVYBM0501N1FJ.json"),
            "the plan path must survive screening: {raw}"
        );
        fs::remove_dir_all(root).ok();
    }

    /// §2.2.3: a record taken while nothing is in progress carries `step: null`
    /// and is still a session fact. Dropping it emptied the diary host block
    /// and the compaction anchor for every session without an active plan.
    #[test]
    fn unattributed_results_are_recorded_as_session_facts() {
        let root = root();
        let mut journal = Journal::open(&root, "session").unwrap();
        // no set_attribution: no active plan, nothing in progress
        assert_eq!(
            journal
                .append_evidence("tool_result", json!({"tool": "bash", "ok": true}))
                .unwrap(),
            1
        );
        assert_eq!(
            journal
                .append_evidence("file_diff", json!({"path": "src/main.rs", "added": 3}))
                .unwrap(),
            2
        );

        let records = Journal::records_for(&root, "session").unwrap();
        assert_eq!(records.len(), 2, "records must reach the journal");
        assert_eq!(records[0].kind, "tool_result");
        assert_eq!(records[1].kind, "file_diff");
        for record in &records {
            assert!(
                record.step.is_none(),
                "unattributed record needs step: null"
            );
            assert!(record.plan.is_none());
        }

        // ... and are never usable as evidence
        for (index, record) in records.iter().enumerate() {
            let reference = crate::plan::EvidenceRef {
                session: "session".to_string(),
                seq: record.seq,
            };
            assert!(
                Journal::evidence(&root, "any-plan", Some("1"), &reference, None)
                    .unwrap()
                    .is_none(),
                "record {index} must not qualify as evidence"
            );
        }
        fs::remove_dir_all(root).ok();
    }

    /// The pre-image to return to is the state *before* the window, so a file
    /// written twice must revert to the first record's blob, not the last —
    /// while the outside-edit check still compares against the newest hash the
    /// host left. Getting this backwards would restore an intermediate state
    /// and call it done.
    #[test]
    fn a_file_written_twice_reverts_to_the_state_before_the_window() {
        let root = root();
        let mut journal = Journal::open(&root, "session").unwrap();
        for (blob, hash) in [
            ("blake3:first", "sha256:one"),
            ("blake3:second", "sha256:two"),
        ] {
            journal
                .append(
                    "file_diff",
                    serde_json::json!({
                        "path": "src/main.rs",
                        "checkpoint": "cp1",
                        "blob_before": blob,
                        "hash_after": hash,
                    }),
                )
                .unwrap();
        }
        let pre = Journal::recorded_pre_images(&root, &["cp1".to_string()]).unwrap();
        assert_eq!(pre.len(), 1, "one entry per path: {pre:?}");
        assert_eq!(pre[0].blob_before.as_deref(), Some("blake3:first"));
        assert_eq!(pre[0].agent_hash.as_deref(), Some("sha256:two"));
    }

    /// A record from before the blob store existed has no pre-image. It must
    /// not be read as "this file was created" — that would delete a file the
    /// agent merely edited.
    #[test]
    fn a_record_without_a_blob_is_not_mistaken_for_a_created_file() {
        let root = root();
        let mut journal = Journal::open(&root, "session").unwrap();
        journal
            .append(
                "file_diff",
                serde_json::json!({
                    "path": "legacy.rs",
                    "checkpoint": "cp1",
                    "hash_before": "sha256:old",
                    "hash_after": "sha256:new",
                }),
            )
            .unwrap();
        let pre = Journal::recorded_pre_images(&root, &["cp1".to_string()]).unwrap();
        assert_eq!(pre[0].blob_before, None);
        assert!(
            pre[0].existed_before,
            "the file existed, so a revert cannot mean deleting it"
        );
        assert_eq!(pre[0].agent_hash.as_deref(), Some("sha256:new"));
    }

    /// §2.5's proviso for per-step undo: a file may be reverted alone only if
    /// no later step has written it since. Reverting it anyway would put back
    /// bytes from before the *later* step, silently undoing that step too —
    /// which is worse than refusing, because the plan would still call it done.
    #[test]
    fn a_step_whose_file_a_later_step_rewrote_is_refused_by_name() {
        let root = root();
        let mut journal = Journal::open(&root, "session").unwrap();

        journal.set_attribution(Some("1".into()), Some("plan".into()), "main");
        journal
            .append(
                "file_diff",
                serde_json::json!({
                    "path": "src/a.rs",
                    "blob_before": "blake3:a-before-step-1",
                    "hash_before": "sha256:a0",
                    "hash_after": "sha256:a1",
                }),
            )
            .unwrap();
        journal
            .append(
                "file_diff",
                serde_json::json!({
                    "path": "src/untouched.rs",
                    "blob_before": "blake3:u-before-step-1",
                    "hash_before": "sha256:u0",
                    "hash_after": "sha256:u1",
                }),
            )
            .unwrap();

        // a later step rewrites one of the two files
        journal.set_attribution(Some("2".into()), Some("plan".into()), "main");
        journal
            .append(
                "file_diff",
                serde_json::json!({
                    "path": "src/a.rs",
                    "blob_before": "blake3:a-before-step-2",
                    "hash_before": "sha256:a1",
                    "hash_after": "sha256:a2",
                }),
            )
            .unwrap();

        let revert = Journal::step_pre_images(&root, "1").unwrap();
        assert_eq!(
            revert.written_since,
            vec!["src/a.rs".to_string()],
            "the contested file has to be named, not silently dropped"
        );
        assert_eq!(
            revert.files.len(),
            1,
            "the other file is still revertible: {:?}",
            revert.files
        );
        assert_eq!(revert.files[0].path, "src/untouched.rs");
        assert_eq!(
            revert.files[0].blob_before.as_deref(),
            Some("blake3:u-before-step-1")
        );

        // and step 2 itself, being the latest writer, reverts freely
        let revert = Journal::step_pre_images(&root, "2").unwrap();
        assert!(revert.written_since.is_empty());
        assert_eq!(
            revert.files[0].blob_before.as_deref(),
            Some("blake3:a-before-step-2"),
            "step 2 goes back to the state step 1 left, not to the original"
        );
    }

    /// Records from other steps must not leak into a step's own revert, and a
    /// step that wrote nothing has nothing to put back.
    #[test]
    fn a_step_reverts_only_its_own_writes() {
        let root = root();
        let mut journal = Journal::open(&root, "session").unwrap();
        journal.set_attribution(Some("7".into()), Some("plan".into()), "main");
        journal
            .append(
                "file_diff",
                serde_json::json!({"path": "mine.rs", "blob_before": "blake3:mine"}),
            )
            .unwrap();
        journal.set_attribution(None, Some("plan".into()), "main");
        journal
            .append(
                "file_diff",
                serde_json::json!({"path": "unattributed.rs", "blob_before": "blake3:other"}),
            )
            .unwrap();

        let revert = Journal::step_pre_images(&root, "7").unwrap();
        assert_eq!(revert.files.len(), 1);
        assert_eq!(revert.files[0].path, "mine.rs");

        assert!(
            Journal::step_pre_images(&root, "9")
                .unwrap()
                .files
                .is_empty(),
            "a step with no records reverts nothing"
        );
    }

    /// What scopes `/undo`: only the paths this checkpoint's own `file_diff`
    /// records name, each with the hash the agent left the file at.
    #[test]
    fn recorded_writes_scopes_undo_to_the_hosts_own_writes() {
        let root = root();
        let mut journal = Journal::open(&root, "session").unwrap();
        journal
            .append(
                "file_diff",
                json!({"path": "src/a.rs", "hash_after": "aaa", "checkpoint": "sha-1"}),
            )
            .unwrap();
        journal
            .append(
                "file_diff",
                json!({"path": "src/b.rs", "hash_after": "bbb", "checkpoint": "sha-2"}),
            )
            .unwrap();
        // same file written twice inside the undone window: the newest hash is
        // the one it was left at
        journal
            .append(
                "file_diff",
                json!({"path": "src/a.rs", "hash_after": "aaa2", "checkpoint": "sha-2"}),
            )
            .unwrap();
        // a record belonging to a checkpoint that is not being undone
        journal
            .append(
                "file_diff",
                json!({"path": "src/untouched.rs", "hash_after": "zzz", "checkpoint": "sha-9"}),
            )
            .unwrap();

        let writes =
            Journal::recorded_writes(&root, &["sha-1".to_string(), "sha-2".to_string()]).unwrap();
        assert_eq!(
            writes,
            vec![
                ("src/a.rs".to_string(), Some("aaa2".to_string())),
                ("src/b.rs".to_string(), Some("bbb".to_string())),
            ]
        );

        // a checkpoint with no file records — a bash mutation — cannot narrow
        // the scope, and says so by returning nothing
        assert!(
            Journal::recorded_writes(&root, &["sha-bash".to_string()])
                .unwrap()
                .is_empty()
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn records_undo_checkpoint_files_and_reopened_steps() {
        let root = root();
        let mut journal = Journal::open(&root, "session").unwrap();
        journal.set_attribution(None, Some("plan-1".into()), "host");
        let seq = journal
            .append_undo(
                "checkpoint-1",
                &["src/lib.rs".into(), "src/main.rs".into()],
                &["1".into()],
            )
            .unwrap();

        let record = Journal::records_for(&root, "session")
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(seq, record.seq);
        assert_eq!(record.kind, "undo");
        assert_eq!(record.agent, "host");
        assert_eq!(record.plan.as_deref(), Some("plan-1"));
        assert_eq!(record.fields["to_checkpoint"], "checkpoint-1");
        assert_eq!(record.fields["files"], json!(["src/lib.rs", "src/main.rs"]));
        assert_eq!(record.fields["reopened_steps"], json!(["1"]));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn repairs_partial_tail_and_records_that_it_did() {
        let root = root();
        let path = root.join(".sqwai").join("journal").join("session.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{\"seq\":1,\"ts\":\"1\",\"step\":null,\"plan\":null,\"agent\":\"main\",\"kind\":\"note\",\"text\":\"ok\"}\n{\"seq\":2").unwrap();
        let mut journal = Journal::open(&root, "session").unwrap();

        // §2.2.1: the partial line goes, and the host says so. Truncating an
        // append-only log in silence is the one thing an audit record cannot do.
        let records = Journal::records_for(&root, "session").unwrap();
        assert_eq!(records.len(), 2, "{records:?}");
        assert_eq!(records[1].kind, "journal_repair");
        assert!(
            records[1]
                .fields
                .get("truncated_bytes")
                .and_then(Value::as_u64)
                .is_some_and(|bytes| bytes > 0)
        );

        // and the sequence continues past it
        assert_eq!(journal.next_seq(), 3);
        assert_eq!(journal.append("note", json!({"text": "next"})).unwrap(), 3);
        assert_eq!(
            fs::read_to_string(journal.path()).unwrap().lines().count(),
            3
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn nudges_after_unaccounted_step_actions() {
        let root = root();
        let mut plan = crate::plan::create(
            "keep working".to_string(),
            Vec::new(),
            Vec::new(),
            vec![crate::plan::NewStep {
                title: "step".into(),
                kind: Some(crate::plan::StepKind::Change),
                refs: Vec::new(),
            }],
            1000,
            &crate::plan::Limits::default(),
        )
        .unwrap();
        crate::plan::store(&root, &plan).unwrap();
        crate::plan::apply(
            &mut plan,
            crate::plan::Op::Start {
                id: "1".into(),
                confirm: None,
            },
            &crate::plan::Limits::default(),
        )
        .unwrap();
        crate::plan::store(&root, &plan).unwrap();
        let mut journal = Journal::open(&root, "nudge").unwrap();
        journal.set_attribution(Some("1".into()), Some(plan.id.clone()), "main");
        for _ in 0..3 {
            journal
                .append("tool_result", json!({"tool": "read", "ok": true}))
                .unwrap();
        }
        assert!(
            Journal::nudge(&root, 2)
                .unwrap()
                .unwrap()
                .contains("3 actions")
        );
        journal.append("plan", json!({"op": "show"})).unwrap();
        assert!(Journal::nudge(&root, 2).unwrap().is_none());
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
