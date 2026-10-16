//! Host-owned append-only session journal.

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// Process-global per-file append locks. Several handles append to one
/// journal file (loop writer, plan commits, receipts, undo), each with its
/// own counter — without serialization two handles read the same tail and
/// write the same seq twice. The tail check inside `append` is not enough;
/// the check AND the write must hold this lock.
fn file_locks() -> &'static Mutex<HashMap<PathBuf, Arc<Mutex<()>>>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    LOCKS.get_or_init(Default::default)
}

fn lock_for(path: &Path) -> Arc<Mutex<()>> {
    file_locks()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .entry(path.to_path_buf())
        .or_default()
        .clone()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub seq: u64,
    pub ts: String,
    /// Owning session journal (the file stem). Never serialized: the file
    /// already is the namespace, persisting it would duplicate that. Filled
    /// on read by [`Journal::records`] / [`Journal::records_for`], because
    /// `seq` is only ordered *within* one file — comparing raw seqs across
    /// merged sessions mixes unrelated counters (undo's `written_since`
    /// did exactly that). Empty for hand-built records.
    ///
    /// The serde name is deliberately NOT `session`: record *fields* may
    /// carry their own `session` key (the join intent's new member), and a
    /// same-named struct field would swallow it out of the flattened map.
    #[serde(rename = "journal_session", default, skip_serializing)]
    pub session: String,
    pub step: Option<String>,
    pub plan: Option<String>,
    pub agent: String,
    /// Step epoch the record was written under. `None` for
    /// pre-epoch records and the main agent, which always count as current.
    /// A subagent stamps its inherited epoch; after the step is reopened
    /// (epoch bumped) such records stop counting as evidence.
    #[serde(default)]
    pub epoch: Option<u64>,
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
    epoch: Option<u64>,
}

/// What a per-step revert can and cannot put back.
#[derive(Debug, Default, Clone)]
pub struct StepRevert {
    /// pre-images that can be restored without disturbing another step
    pub files: Vec<PreImage>,
    /// paths this step wrote that a later step wrote again; reverting them
    /// alone would undo that later step too, so they are refused by name
    pub written_since: Vec<String>,
}

/// What one file looked like before an undone window touched it.
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

/// An assumption the model recorded and nothing has resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assumption {
    pub seq: u64,
    pub step: Option<String>,
    pub text: String,
}

impl Assumption {
    /// One-line form for the finish warning, the anchor and the recap.
    pub fn label(&self, limit: usize) -> String {
        let mut text: String = self.text.chars().take(limit).collect();
        if self.text.chars().count() > limit {
            text.push('…');
        }
        format!("j#{}: {text}", self.seq)
    }
}

/// One session inside a [`Journal::recap`].
#[derive(Debug, Clone, Default)]
pub struct Recap {
    pub session: String,
    /// calendar day of the session's first record in the window
    pub day: String,
    pub from: String,
    pub to: String,
    /// what the session was working on: the goal of the first plan it touched
    pub ask: Option<String>,
    pub plans: Vec<RecapPlan>,
    /// path, lines added, lines removed — summed over the window
    pub files: Vec<(String, u64, u64)>,
    pub calls: usize,
    pub failures: usize,
    pub notes: usize,
    pub open: Vec<String>,
}

/// A plan as the recap sees it: identity, verdict, and what is still open.
#[derive(Debug, Clone)]
pub struct RecapPlan {
    pub id: String,
    pub goal: String,
    pub status: String,
    pub closed: usize,
    pub total: usize,
    pub unfinished: Vec<String>,
}

fn clock(ts: &str) -> (String, String) {
    match chrono::DateTime::parse_from_rfc3339(ts) {
        Ok(t) => (
            t.format("%Y-%m-%d").to_string(),
            t.format("%H:%M").to_string(),
        ),
        Err(_) => (String::new(), String::new()),
    }
}

impl Recap {
    fn from_records(
        session: String,
        records: &[Record],
        plans: &[crate::plan::Plan],
        root: &Path,
    ) -> Result<Self> {
        let (day, from) = clock(&records[0].ts);
        let (_, to) = clock(&records[records.len() - 1].ts);
        let mut out = Recap {
            session,
            day,
            from,
            to,
            ..Default::default()
        };
        let mut seen_plans: Vec<String> = Vec::new();
        for r in records {
            match r.kind.as_str() {
                "tool_call" => out.calls += 1,
                "tool_result" => {
                    if r.fields.get("ok").and_then(Value::as_bool) == Some(false) {
                        out.failures += 1;
                    }
                }
                "note" => out.notes += 1,
                "file_diff" => {
                    let Some(path) = r.fields.get("path").and_then(Value::as_str) else {
                        continue;
                    };
                    let added = r.fields.get("added").and_then(Value::as_u64).unwrap_or(0);
                    let removed = r.fields.get("removed").and_then(Value::as_u64).unwrap_or(0);
                    match out.files.iter_mut().find(|(p, _, _)| p == path) {
                        Some((_, a, b)) => {
                            *a += added;
                            *b += removed;
                        }
                        None => out.files.push((path.to_string(), added, removed)),
                    }
                }
                _ => {}
            }
            if let Some(id) = &r.plan
                && !seen_plans.iter().any(|seen| seen == id)
            {
                seen_plans.push(id.clone());
            }
        }
        for id in seen_plans {
            let Some(plan) = plans.iter().find(|p| p.id == id) else {
                continue;
            };
            let closed = plan
                .steps
                .iter()
                .filter(|s| {
                    matches!(
                        s.status,
                        crate::plan::StepStatus::Done | crate::plan::StepStatus::Cancelled
                    )
                })
                .count();
            let unfinished = plan
                .steps
                .iter()
                .filter(|s| {
                    !matches!(
                        s.status,
                        crate::plan::StepStatus::Done | crate::plan::StepStatus::Cancelled
                    )
                })
                .map(|s| {
                    let title: String = s.title.chars().take(60).collect();
                    format!("{id} {title}", id = s.id)
                })
                .collect();
            let goal: String = plan.goal.text.chars().take(90).collect();
            if out.ask.is_none() {
                out.ask = Some(goal.clone());
            }
            out.plans.push(RecapPlan {
                id: id.clone(),
                goal,
                status: match plan.status {
                    crate::plan::PlanStatus::Active => "active",
                    crate::plan::PlanStatus::Completed => "completed",
                    crate::plan::PlanStatus::Abandoned => "abandoned",
                    crate::plan::PlanStatus::Blocked => "blocked",
                }
                .to_string(),
                closed,
                total: plan.steps.len(),
                unfinished,
            });
        }
        out.open = Self::open_labels(root, &out.session);
        Ok(out)
    }

    fn open_labels(root: &Path, session: &str) -> Vec<String> {
        Journal::open_assumptions_in(root, session, None)
            .unwrap_or_default()
            .iter()
            .take(4)
            .map(|a| a.label(80))
            .collect()
    }
}

impl Journal {
    /// Open or create `journal/<session-id>.jsonl`, repairing a partial tail.
    pub fn open(root: &Path, session_id: &str) -> Result<Self> {
        let dir = root.join(".sqwai").join("journal");
        fs::create_dir_all(&dir).context("creating journal directory")?;
        let path = dir.join(format!("{session_id}.jsonl"));
        // Repair and seq-scan under the same per-file lock append holds:
        // an unlocked repair_tail can truncate a line another handle is
        // mid-write (bytes written, '\n' not yet), losing the record and
        // handing its seq to a later append. The guard must drop before
        // the journal_repair append below — the mutex is not reentrant.
        let (repaired, next_seq) = {
            let file_lock = lock_for(&path);
            let _guard = file_lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let repaired = repair_tail(&path)?;
            let next_seq = last_seq(&path)?.saturating_add(1);
            (repaired, next_seq)
        };
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
            epoch: None,
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

    /// Stamp the inherited step epoch on every record this writer produces
    ///. Used by subagent sessions; the main session leaves it `None`.
    pub fn set_epoch(&mut self, epoch: Option<u64>) {
        self.epoch = epoch;
    }

    #[allow(dead_code)]
    pub fn attribution(&self) -> (Option<String>, Option<String>) {
        (self.step.clone(), self.plan.clone())
    }

    /// Append a host-owned record and, when a step is in progress, atomically
    /// attach it to that step's evidence.
    ///
    /// The record is written either way. With nothing in progress it carries
    /// `step: null` and counts as a session fact — for the recap and
    /// the compaction anchor — but never as evidence.
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
        let session = self
            .path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string();
        // Under the plan lock and re-read inside it (audit C3): a store
        // from a copy loaded before a concurrent commit would regress that
        // commit's content and cursors.
        let _lock = crate::plan::store::PlanLock::acquire(root, &plan_id)?;
        let mut active = match crate::plan::open_active_for_session(root, Some(&session))? {
            Some(plan) if plan.id == plan_id => plan,
            _ => return Ok(seq),
        };
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

    /// Re-read the tail counter from disk. `plan::commit` appends through a
    /// short-lived handle, so a long-lived writer must call this after any
    /// plan tool call before appending again — otherwise it would reuse a
    /// sequence number.
    ///
    /// #189: failures are returned, never swallowed — a silent stale
    /// counter reuses sequence numbers and corrupts evidence refs.
    pub fn resync(&mut self) -> Result<()> {
        let last = last_seq(&self.path)?;
        self.next_seq = last.saturating_add(1);
        Ok(())
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
            let session = entry
                .path()
                .file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_string();
            let file = File::open(entry.path()).context("opening journal for evidence")?;
            for line in BufReader::new(file).lines() {
                let line = line.context("reading journal for evidence")?;
                if line.trim().is_empty() {
                    continue;
                }
                // Degrade, don't refuse (audit H18): one malformed line —
                // a crash mid-write, a disk error, a hand edit — used to
                // fail this whole read, and every consumer (undo, blob
                // purge, evidence validation) died with it. Unreadable
                // lines are skipped; the readable history survives.
                let Ok(mut record) = serde_json::from_str::<Record>(&line) else {
                    continue;
                };
                record.session = session.clone();
                records.push(record);
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
                    // same degrade-don't-refuse rule as `records` (audit
                    // H18): a corrupt line is skipped, not fatal. An
                    // evidence ref pointing at it simply finds no record
                    // and is rejected — fail-closed where it matters.
                    serde_json::from_str::<Record>(&line)
                        .ok()
                        .map(|mut record| {
                            record.session = session_id.to_string();
                            Ok(record)
                        })
                }
                Ok(_) => None,
                Err(error) => Some(Err(error.into())),
            })
            .collect()
    }

    /// What happened in this project over the last `days` days, per session
    ///. Every number comes from a record the host wrote and every plan
    /// line from the plan file itself: there is no model prose in a recap, so
    /// it cannot report an outcome it never saw — which is exactly how the
    /// diary it replaces differed.
    pub fn recap(root: &Path, days: i64) -> Result<Vec<Recap>> {
        let dir = root.join(".sqwai").join("journal");
        let Ok(entries) = fs::read_dir(&dir) else {
            return Ok(Vec::new());
        };
        let now = chrono::Local::now();
        let first_day = (now - chrono::Duration::days(days.clamp(1, 7) - 1))
            .naive_local()
            .date();
        // A file's mtime is its last write, so a journal whose newest byte is
        // older than the window holds no record inside it. With thousands of
        // session files in one project this stat is the difference between a
        // millisecond and a rescan of all of them.
        let cutoff_secs = first_day
            .and_hms_opt(0, 0, 0)
            .and_then(|t| t.and_local_timezone(chrono::Local).latest())
            .map(|t| t.timestamp());
        let plans = crate::plan::list(root);
        let mut out: Vec<Recap> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            if let (Some(cutoff), Ok(meta)) = (cutoff_secs, entry.metadata())
                && let Ok(modified) = meta.modified()
                && chrono::DateTime::<chrono::Utc>::from(modified).timestamp() < cutoff
            {
                continue;
            }
            let Some(session) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let mut records = Self::records_for(root, session)?;
            records.retain(|r| {
                chrono::DateTime::parse_from_rfc3339(&r.ts)
                    .map(|t| t.date_naive() >= first_day)
                    .unwrap_or(false)
            });
            if records.is_empty() {
                continue;
            }
            records.sort_by_key(|record| record.seq);
            out.push(Recap::from_records(
                session.to_string(),
                &records,
                &plans,
                root,
            )?);
        }
        out.sort_by(|a, b| a.day.cmp(&b.day).then(a.from.cmp(&b.from)));
        Ok(out)
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
    /// reverting a single step needs layer 1 only — *"restore
    /// `hash_before` from the blob store if the file has not been touched by
    /// any other steps since (verified via the journal's `file_diff` chain)"*.
    /// That proviso is the whole difficulty: a file the step wrote and a later
    /// step then rewrote cannot be reverted in isolation, because putting the
    /// old bytes back would silently undo the later step as well. So the result
    /// carries the revertible pre-images and, separately, the paths a later step
    /// has since written, so the caller can refuse and say which.
    ///
    /// The scan is scoped to one plan, because step numbers restart per plan
    /// ("1" exists in every plan) and an unscoped scan merges same-numbered
    /// steps of every plan in the project — undoing one step could put back
    /// another plan's bytes. Records stamped before plan attribution existed
    /// (`plan: None`) still count: they cannot be attributed anywhere else.
    ///
    /// `plan: None` keeps that unscoped scan. It is a query, not a command: the
    /// only thing allowed to run it is a test fixture. `/undo step` refuses
    /// without a plan, because a revert it cannot attribute is a revert it
    /// cannot reopen.
    pub fn step_pre_images_in(root: &Path, plan: Option<&str>, step: &str) -> Result<StepRevert> {
        let records = Self::records(root)?;
        let in_scope = |record: &Record| {
            plan.is_none_or(|wanted| {
                record.plan.as_deref() == Some(wanted) || record.plan.is_none()
            })
        };
        let mut revert = StepRevert::default();
        // `seq` orders one session file only; across sessions the wall
        // clock decides. Tracked per path this step wrote, because only a
        // later write to the *same* path stands in the way: a global step
        // maximum hides a clobber whenever the step's own writes to other
        // paths land after the foreign write. A busy session's high
        // counters neither hide such a write (silent clobber) nor block a
        // safe revert (false refusal).
        let mut last_write: std::collections::HashMap<
            String,
            (String, std::collections::HashMap<String, u64>),
        > = std::collections::HashMap::new();

        for record in &records {
            if record.kind != "file_diff"
                || !in_scope(record)
                || record.step.as_deref() != Some(step)
            {
                continue;
            }
            let Some(path) = record.fields.get("path").and_then(Value::as_str) else {
                continue;
            };
            last_write
                .entry(path.to_string())
                .and_modify(|slot| {
                    if record.ts.as_str() > slot.0.as_str() {
                        slot.0 = record.ts.clone();
                    }
                    slot.1
                        .entry(record.session.clone())
                        .and_modify(|last| *last = (*last).max(record.seq))
                        .or_insert(record.seq);
                })
                .or_insert_with(|| {
                    let mut sessions = std::collections::HashMap::new();
                    sessions.insert(record.session.clone(), record.seq);
                    (record.ts.clone(), sessions)
                });
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

        // Anything written after this step's last write to the same path,
        // by a different step of the same plan, makes that path
        // un-revertible on its own. Writes from other plans do not count:
        // their step numbers live in a different namespace. "After" is the
        // wall clock across sessions (RFC3339 strings compare
        // chronologically) and the file order within one session; anything
        // not provably earlier stands in the way, because a silent revert
        // would undo that later work too.
        for record in &records {
            if record.kind != "file_diff" || !in_scope(record) {
                continue;
            }
            let Some(path) = record.fields.get("path").and_then(Value::as_str) else {
                continue;
            };
            if record.step.as_deref() == Some(step) {
                continue;
            }
            // paths this step never wrote need no protection
            let Some((step_ts, step_sessions)) = last_write.get(path) else {
                continue;
            };
            let provably_earlier = record.ts.as_str() < step_ts.as_str()
                || step_sessions
                    .get(record.session.as_str())
                    .is_some_and(|last| record.seq <= *last);
            if provably_earlier {
                continue;
            }
            if revert.files.iter().any(|item| item.path == path)
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

    /// Record a per-step revert.
    #[cfg(test)]
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
    /// under it.
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
    /// what means by blobs being *"purged together with the session
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

    /// Assumption notes that nothing has closed yet.
    ///
    /// A `note { note: "assumption" }` opens one; a later note carrying
    /// `resolves: <seq>` closes the note with that sequence number. Without
    /// this the `assumption` kind can be written but never settled — the model
    /// states an assumption, the step finishes, and nobody ever learns whether
    /// it held.
    ///
    /// `step` narrows the result to one plan step, which is what `finish`
    /// needs; `None` returns the session's open assumptions, which is what the
    /// anchor and the diary need.
    ///
    /// Scoped to one session journal: sequence numbers restart per file, and
    /// step numbers restart per plan, so an unscoped scan would validate a
    /// `resolves` against another session's note and warn about foreign
    /// plans' assumptions.
    pub fn open_assumptions_in(
        root: &Path,
        session_id: &str,
        step: Option<&str>,
    ) -> Result<Vec<Assumption>> {
        Ok(Self::assumptions_in(
            &Self::records_for(root, session_id)?,
            step,
        ))
    }

    fn assumptions_in(records: &[Record], step: Option<&str>) -> Vec<Assumption> {
        let resolved: std::collections::HashSet<u64> = records
            .iter()
            .filter(|record| record.kind == "note")
            .filter_map(|record| record.fields.get("resolves").and_then(Value::as_u64))
            .collect();
        records
            .iter()
            .filter(|record| {
                record.kind == "note"
                    && record.fields.get("note").and_then(Value::as_str) == Some("assumption")
                    && !resolved.contains(&record.seq)
                    // a note that resolves another is a closure, not a new
                    // assumption, even when it carries the same kind
                    && record.fields.get("resolves").is_none()
                    && step.is_none_or(|wanted| record.step.as_deref() == Some(wanted))
            })
            .map(|record| Assumption {
                seq: record.seq,
                step: record.step.clone(),
                text: record
                    .fields
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
            .collect()
    }

    /// Decision-like notes for the anchor: `decision` (why this way),
    /// `lesson` (learned the hard way), `rejected` (considered and dropped).
    /// Newest last — the anchor takes the tail. Same record shape as
    /// assumptions: kind `note`, the note kind in `fields.note`, content in
    /// `fields.text`.
    pub fn decision_notes_in(root: &Path, session_id: &str) -> Result<Vec<String>> {
        Ok(Self::records_for(root, session_id)?
            .iter()
            .filter(|record| {
                record.kind == "note"
                    && record
                        .fields
                        .get("note")
                        .and_then(Value::as_str)
                        .is_some_and(|kind| matches!(kind, "decision" | "lesson" | "rejected"))
            })
            .filter_map(|record| record.fields.get("text").and_then(Value::as_str))
            .map(str::to_string)
            .collect())
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

    /// Per-session timestamp check as **warn** (not `invalid_evidence`).
    ///
    /// If evidence predates the earliest `plan op start` for this step, it is
    /// still accepted, but the host notes it: the
    /// evidence was recorded before the step existed in any session, so it
    /// may be stale or from a reused `seq`. One line per stale ref, capped.
    pub fn stale_evidence_warnings(
        root: &Path,
        plan: &str,
        step: &str,
        evidence: &[crate::plan::EvidenceRef],
    ) -> Vec<String> {
        let records = match Self::records(root) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        // earliest start timestamp for this plan/step across all sessions
        let earliest = records
            .iter()
            .filter(|r| {
                r.plan.as_deref() == Some(plan)
                    && (r.step.as_deref() == Some(step)
                        || r.fields.get("id").and_then(Value::as_str) == Some(step))
                    && r.kind == "plan"
                    && r.fields.get("op").and_then(Value::as_str) == Some("start")
                    && r.fields.get("ok").and_then(Value::as_bool) != Some(false)
            })
            .filter_map(|r| chrono::DateTime::parse_from_rfc3339(&r.ts).ok())
            .min();
        let Some(start) = earliest else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for reference in evidence {
            let session_records = if reference.session.is_empty() {
                records.clone()
            } else {
                Self::records_for(root, &reference.session).unwrap_or_default()
            };
            let start_seq = session_records
                .iter()
                .filter(|r| {
                    (r.plan.as_deref() == Some(plan) || r.plan.is_none())
                        && (r.step.as_deref() == Some(step)
                            || r.fields.get("id").and_then(Value::as_str) == Some(step))
                        && r.kind == "plan"
                        && r.fields.get("op").and_then(Value::as_str) == Some("start")
                        && r.fields.get("ok").and_then(Value::as_bool) != Some(false)
                })
                .map(|r| r.seq)
                .min();
            if let Some(start_seq) = start_seq {
                if reference.seq < start_seq {
                    let precise = Self::evidence(root, plan, Some(step), reference, None)
                        .ok()
                        .flatten();
                    let rec_ts = precise.as_ref().map(|r| r.ts.as_str()).unwrap_or("unknown");
                    let start_ts = session_records
                        .iter()
                        .find(|r| r.seq == start_seq)
                        .map(|r| r.ts.as_str())
                        .unwrap_or("unknown");
                    out.push(format!(
                        "evidence {}:{} predates step {} start ({} < {})",
                        reference.session, reference.seq, step, rec_ts, start_ts
                    ));
                    if out.len() >= 3 {
                        break;
                    }
                }
                continue;
            }
            // precise session lookup to avoid seq collision across files
            let precise = Self::evidence(root, plan, Some(step), reference, None)
                .ok()
                .flatten();
            let Some(rec) = precise else {
                continue;
            };
            if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&rec.ts)
                && ts < start
            {
                out.push(format!(
                    "evidence {}:{} predates step {} start ({} < {})",
                    reference.session,
                    reference.seq,
                    step,
                    rec.ts,
                    start.to_rfc3339()
                ));
                if out.len() >= 3 {
                    break;
                }
            }
        }
        out
    }

    /// Return a non-blocking reminder when a step has accumulated actions
    /// since its last plan operation. Scoped to the calling session so one
    /// session's unfinished step never nags another session's turn.
    pub fn nudge(
        root: &Path,
        session_id: Option<&str>,
        threshold: usize,
    ) -> Result<Option<String>> {
        let Some(sid) = session_id else {
            return Ok(None);
        };
        let records = Self::records_for(root, sid)?;
        let active = crate::plan::open_active_for_session(root, session_id)?;
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
                "plan: step {} has {actions} actions since its last plan operation.",
                step.id
            )))
        } else {
            Ok(None)
        }
    }

    /// Append one host-owned record and flush it before returning.
    pub fn append(&mut self, kind: &str, fields: Value) -> Result<u64> {
        // Serialize with every other handle on this file: the tail check
        // below races without it (same tail read twice → duplicate seq).
        let file_lock = lock_for(&self.path);
        let _guard = file_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !fields.is_object() {
            bail!("journal fields must be a JSON object");
        }
        let mut fields = fields.as_object().cloned().unwrap_or_default();
        // "no secrets (the same screening as applies to
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
        // Self-healing counter: several handles append to one file (the
        // loop writer, fresh opens in plan commits, receipts, undo), each
        // with its own counter. Without this, two file-advancing appends
        // from another handle leave this one behind, and its next record
        // lands BELOW the file tail — poisoning every later open/resync
        // ("142 after 143", which then blocks plan ops). A cheap look at
        // the tail (no full scan) keeps every handle monotonic.
        if let Some(tail) = tail_seq(&self.path) {
            self.next_seq = self.next_seq.max(tail.saturating_add(1));
        }
        let seq = self.next_seq;
        let record = Record {
            seq,
            ts: timestamp(),
            session: self
                .path
                .file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_string(),
            step: self.step.clone(),
            plan: self.plan.clone(),
            agent: self.agent.clone(),
            epoch: self.epoch,
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

/// Sequence number of the last complete record, read from the file tail
/// without scanning the whole file. Returns `None` when the tail cannot
/// be decoded (missing/empty file, partial trailing line) — the caller
/// keeps its own counter then. Bounded read: records are small (summaries
/// are capped), so the last 8 KiB always hold several complete lines.
fn tail_seq(path: &Path) -> Option<u64> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len == 0 {
        return None;
    }
    use std::io::{Read, Seek, SeekFrom};
    let window = len.min(8192);
    file.seek(SeekFrom::End(-(window as i64))).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    text.lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .find_map(|line| {
            serde_json::from_str::<Record>(line)
                .ok()
                .map(|record| record.seq)
        })
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
/// A crash mid-append leaves half a record; has the reader drop it. It
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
mod tests;
