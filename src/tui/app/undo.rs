//! `/undo`: rolling this session's writes back, and saying what was rolled
//! back. Two layers (recorded pre-images, shadow snapshots), one honest
//! toast about which of them covered the window.

#![allow(unused_imports)]
use super::*;

pub(super) fn is_boundary_checkpoint(label: &str) -> bool {
    label.starts_with("step_") && (label.ends_with("_start") || label.ends_with("_finish"))
}

fn reopen_undone_steps(
    root: &std::path::Path,
    session_id: &str,
    files: &[String],
    checkpoint: &str,
    undone_checkpoints: &[String],
) -> Vec<String> {
    let active_opt = plan::open_active_for_session(root, Some(session_id))
        .ok()
        .flatten();
    let mut reopened = Vec::new();
    let mut active_id = None;
    if let Some(mut active) = active_opt {
        active_id = Some(active.id.clone());
        let records =
            crate::agent::journal::Journal::records_for(root, session_id).unwrap_or_default();
        reopened = reopened_step_ids(&active, &records, session_id, files);
        for step_id in &reopened {
            let _ = plan::reopen_for_undo(
                &mut active,
                step_id,
                format!("reopened by undo to {checkpoint}"),
            );
        }
        if !reopened.is_empty() {
            let args = serde_json::json!({
                "ids": reopened,
                "reason": format!("reopened by undo to {checkpoint}"),
            });
            let _ = plan::commit(root, session_id, &mut active, "reopen", "host", true, args);
        }
    }
    if let Ok(mut journal) = crate::agent::journal::Journal::open(root, session_id) {
        journal.set_attribution(None, active_id, "host");
        let _ = journal.append_undo_window(checkpoint, undone_checkpoints, files, &reopened);
    }
    reopened
}

fn restore_redone_steps(
    root: &std::path::Path,
    session_id: &str,
    reopened_steps: &[String],
    checkpoints: &[String],
    files: &[String],
) -> Vec<String> {
    let mut restored = Vec::new();
    let mut active_id = None;
    if !reopened_steps.is_empty()
        && let Some(mut active) = plan::open_active_for_session(root, Some(session_id))
            .ok()
            .flatten()
    {
        active_id = Some(active.id.clone());
        for step_id in reopened_steps {
            if let Some(step) = active.step_mut(step_id)
                && matches!(
                    step.status,
                    plan::StepStatus::Reopened
                        | plan::StepStatus::Pending
                        | plan::StepStatus::InProgress
                )
            {
                step.status = plan::StepStatus::Done;
                step.reason = None;
                step.finished = Some(chrono::Local::now().to_rfc3339());
                restored.push(step_id.clone());
            }
        }
        if !restored.is_empty() {
            let args = serde_json::json!({
                "ids": restored,
            });
            let _ = plan::commit(root, session_id, &mut active, "redo_steps", "host", true, args);
        }
    }
    if let Ok(mut journal) = crate::agent::journal::Journal::open(root, session_id) {
        journal.set_attribution(None, active_id, "host");
        let _ = journal.append_redo(checkpoints, files, &restored);
    }
    restored
}

/// Coverage context for an undo toast: the mode tag plus a one-shot
/// transition note. Built once per /undo so the blob path does not
/// re-probe git on its own (and so `undo_from_blobs` stays within the
/// argument limit).
pub(super) struct UndoCoverage {
    tag: String,
    transition: Option<String>,
}

impl App {
    /// Retention for both checkpoint layers. Reports only when it did
    /// something: a maintenance pass that announces "nothing to do" on every
    /// `/new` is noise.
    pub(super) fn run_undo_maintenance(&mut self) {
        if self.maintain_rx.is_some() {
            return;
        }
        let root = self.project_root.clone();
        let session = self.session.id.to_string();
        let cfg = self.cfg.undo.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.maintain_rx = Some(rx);
        std::thread::spawn(move || {
            let res = crate::agent::checkpoints::maintain(&root, &cfg, &session);
            let _ = tx.send(res);
        });
    }

    pub(super) fn poll_maintain(&mut self) {
        let Some(rx) = self.maintain_rx.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(report)) if report.did_anything() => {
                let mut note = String::from("undo maintenance:");
                if report.blobs_removed > 0 {
                    note.push_str(&format!(
                        " {} pre-image(s) freed {}",
                        report.blobs_removed,
                        fmt_bytes(report.blobs_freed_bytes)
                    ));
                }
                if !report.chains_dropped.is_empty() {
                    note.push_str(&format!(
                        " {} finished session chain(s) dropped",
                        report.chains_dropped.len()
                    ));
                }
                if report.collected {
                    note.push_str(" shadow repository collected");
                }
                self.status(&note, StatusKind::Info);
                self.dirty = true;
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                crate::providers::log_http(&format!("undo maintenance failed: {e:#}"));
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                self.maintain_rx = Some(rx);
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                crate::providers::log_http("undo maintenance thread ended");
            }
        }
    }

    /// S1 preflight shared by every undo entry: cancel still-registered
    /// children (normally none — they join inside their `task` call),
    /// refuse while background shells are alive (the writer lock stops
    /// dispatch, not an already-running process), and take the restore
    /// lock. `None` means blocked — the caller returns without touching
    /// the tree.
    pub(super) fn undo_preflight(&mut self) -> Option<crate::agent::undo_guard::RestoreGuard> {
        let cancelled = crate::agent::undo_guard::cancel_running_children();
        if cancelled > 0 {
            self.status(
                &format!("cancelled {cancelled} running subagent(s) before undo"),
                StatusKind::Info,
            );
        }
        let running = crate::agent::tools::bg_running_commands();
        if !running.is_empty() {
            let ids: Vec<String> = running
                .iter()
                .map(|(id, session, cmd)| format!("job {id} ({session}): {cmd}"))
                .collect();
            self.status(
                &format!(
                    "undo refused: {} background job(s) still running — \
                     kill them with bash_kill or wait for completion: {}",
                    running.len(),
                    ids.join("; "),
                ),
                StatusKind::Warn,
            );
            return None;
        }
        Some(crate::agent::undo_guard::hold_restore())
    }

    /// Tell the next request what was rolled back. The transcript still carries
    /// the reverted writes, so without this line the model reads its own old
    /// claims as the current state of the tree.
    pub(super) fn arm_undo_note(&mut self, what: &str, touched: &[String]) {
        const NAMED: usize = 24;
        let mut list = touched
            .iter()
            .take(NAMED)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        if let Some(extra) = touched.len().checked_sub(NAMED) {
            list.push_str(&format!(", {extra} more"));
        }
        self.session.pending_undo_note = Some(format!(
            "the user reverted {} file(s) by {what}: {list}",
            touched.len()
        ));
    }

    /// A restore that stopped mid-window. The files named in the report really
    /// were reverted, so saying only "undo failed" would hide half the truth —
    /// and the checkpoint stays, which lets another `/undo` finish the job.
    pub(super) fn report_partial_undo(
        &mut self,
        what: &str,
        report: &crate::agent::checkpoints::RestoreReport,
    ) {
        let touched = report.touched();
        self.arm_undo_note(what, &touched);
        self.context_bootstrap_pending = true;
        self.session.save().ok();
        self.status(
            &format!(
                "undo stopped: {}; {} file(s) already reverted, {} left alone — the \
                 checkpoint is kept, /undo again continues",
                report.failed.clone().unwrap_or_default(),
                touched.len(),
                report.skipped.len(),
            ),
            StatusKind::Err,
        );
        self.dirty = true;
    }

    /// Undo coverage tag for toast headers: empty in full mode (shadow
    /// snapshots available), ` (files-only, …)` with the reason when layer 2
    /// is down. Full mode stays silent — it is the default, not news. The tag
    /// leads the toast text so it survives width truncation on narrow
    /// terminals, which cuts the tail.
    pub(super) fn undo_coverage_tag(&self, layer2: bool) -> String {
        if layer2 {
            String::new()
        } else {
            format!(" (files-only, {})", self.undo_degraded_why())
        }
    }

    /// Why layer 2 (shadow tree snapshots) is down. The probe names it: with
    /// `[undo].shadow = "off"` the cause is a setting, and a toast that says
    /// "no git" sends the user to install something they already have.
    pub(super) fn undo_degraded_why(&self) -> &'static str {
        crate::agent::checkpoints::unavailable_reason(&self.project_root, self.cfg.undo.shadow)
            .unwrap_or("cause unknown")
    }

    /// Transition note when layer-2 availability changed since the previous
    /// /undo call. The first call only sets the baseline silently — a degraded
    /// mode at startup is reported by the coverage tag, not by a transition
    /// that has no baseline to compare against. The baseline is saved, so a
    /// restart does not turn every /undo back into a first call.
    pub(super) fn undo_transition(&mut self, layer2: bool) -> Option<String> {
        let prev = self.session.undo_layer2.replace(layer2);
        if prev != Some(layer2) {
            self.session.save().ok();
        }
        match prev {
            Some(old) if old != layer2 => Some(if layer2 {
                "undo mode changed to full".to_string()
            } else {
                format!(
                    "undo mode changed to files-only ({})",
                    self.undo_degraded_why()
                )
            }),
            _ => None,
        }
    }

    pub(super) fn undo_turn(&mut self) {
        if self.session.checkpoints.is_empty() {
            self.status("nothing to undo", StatusKind::Info);
            return;
        }
        let start = self
            .session
            .turn_checkpoint_start
            .min(self.session.checkpoints.len());
        let count = self.session.checkpoints.len().saturating_sub(start);
        if count == 0 {
            self.status(
                "undo turn: no checkpoints recorded in the latest turn (use /undo <n> or /undo list)",
                StatusKind::Info,
            );
            return;
        }
        self.undo(count);
    }

    pub(super) fn undo(&mut self, n: usize) {
        let n = n.max(1);
        if self.session.checkpoints.is_empty() {
            self.status("nothing to undo", StatusKind::Info);
            return;
        }
        let idx = self.session.checkpoints.len().saturating_sub(n);
        let (sha, label) = self.session.checkpoints[idx].clone();
        let root = self.project_root.clone();
        let git_snapshots = crate::agent::checkpoints::available_in(&root, self.cfg.undo.shadow);
        // Coverage header + transition note for the toasts below. Computed
        // once: the blob path below must not re-probe git on its own.
        let cov = self.undo_coverage_tag(git_snapshots);
        let transition = self.undo_transition(git_snapshots);
        // Scope the restore to what the host recorded as its own writes across
        // the checkpoints being undone. Without this, undo reverts the whole
        // tree to the snapshot and silently discards anything the user edited
        // in their own editor while the agent was working.
        let undone_pairs: Vec<(String, String)> = self.session.checkpoints[idx..].to_vec();
        let undone: Vec<String> = undone_pairs.iter().map(|(sha, _)| sha.clone()).collect();
        let recorded =
            crate::agent::journal::Journal::recorded_writes(&root, &undone).unwrap_or_default();
        let scoped = !recorded.is_empty();
        // A window can mix file edits with a `bash` call. Scoping to the
        // journal then silently leaves the shell command's effects in place,
        // so count the non-boundary checkpoints that contributed nothing.
        let unrecorded = if scoped {
            let with_records =
                crate::agent::journal::Journal::checkpoints_with_writes(&root, &undone)
                    .unwrap_or_default();
            undone_pairs
                .iter()
                .filter(|(c_sha, c_label)| {
                    !is_boundary_checkpoint(c_label) && !with_records.contains(c_sha)
                })
                .count()
        } else {
            0
        };
        // Layer 1: the pre-images the host stored before each write.
        // No git needed, and it is the only path that works in a project that
        // is not a repository at all.
        let pre_images =
            crate::agent::journal::Journal::recorded_pre_images(&root, &undone).unwrap_or_default();
        // Layer 1 covers this window only when every item is either backed by a
        // blob that is still there or is a pure creation with nothing to put
        // back. Retention or a failed blob write can leave a window half
        // covered; taking the blob path then reports the rest as "no stored
        // pre-image" while the shadow snapshot could have restored all of them.
        let covered = crate::agent::checkpoints::blobs_cover(&root, &pre_images);
        let is_blob_only_sha = sha.starts_with("blob:");
        if (!git_snapshots || is_blob_only_sha) && pre_images.is_empty() {
            self.status(
                "nothing to undo from: no shadow snapshot and no stored pre-images",
                StatusKind::Warn,
            );
            return;
        }
        // S1: nothing mutates under the restore below — not dispatch
        // (writer lock), not running children (cancelled), not live
        // shells (refused above while any survive).
        let _restore = match self.undo_preflight() {
            Some(guard) => guard,
            None => return,
        };
        // Prefer the blob store when it covers the whole window (including
        // when no unrecorded bash checkpoint sits in the window, or when Layer 2
        // is unavailable / this checkpoint is a Layer-1 blob id).
        if !git_snapshots || is_blob_only_sha || (scoped && covered && unrecorded == 0) {
            self.undo_from_blobs(
                &root,
                idx,
                &label,
                &pre_images,
                unrecorded,
                UndoCoverage {
                    tag: cov,
                    transition,
                },
            );
            return;
        }
        let targets: Vec<crate::agent::checkpoints::Target> = if scoped {
            let mut list: Vec<crate::agent::checkpoints::Target> = recorded
                .into_iter()
                .map(|(path, agent_hash)| crate::agent::checkpoints::Target { path, agent_hash })
                .collect();
            // Hybrid window: if the window also contains an unrecorded `bash`
            // checkpoint and `sha` is a real shadow commit, include any extra
            // paths that changed since `sha` so the bash effects are reverted too.
            if unrecorded > 0
                && let Ok(extra) =
                    crate::agent::checkpoints::changed_files(&root, self.cfg.undo.shadow, &sha)
            {
                for path in extra {
                    if !list.iter().any(|t| t.path == path) {
                        list.push(crate::agent::checkpoints::Target {
                            path,
                            agent_hash: None,
                        });
                    }
                }
            }
            list
        } else {
            // Nothing recorded — a `bash` mutation, whose effects the host
            // cannot enumerate. Fall back to the snapshot-vs-worktree diff and
            // say so, rather than pretending the scope is known.
            crate::agent::checkpoints::changed_files(&root, self.cfg.undo.shadow, &sha)
                .unwrap_or_default()
                .into_iter()
                .map(|path| crate::agent::checkpoints::Target {
                    path,
                    agent_hash: None,
                })
                .collect()
        };

        let candidate_paths: Vec<String> = targets.iter().map(|t| t.path.clone()).collect();
        let pre_undo = crate::agent::checkpoints::capture_pre_undo_blobs(&root, &candidate_paths);

        match crate::agent::checkpoints::restore_paths_in(
            &root,
            self.cfg.undo.shadow,
            &sha,
            &targets,
        ) {
            Ok(report) => {
                if report.failed.is_some() {
                    self.report_partial_undo(&format!("undo to '{label}'"), &report);
                    return;
                }
                let touched = report.touched();
                let reopened_steps = reopen_undone_steps(
                    &root,
                    &self.session.id.to_string(),
                    &touched,
                    &sha,
                    &undone,
                );
                let redo_files =
                    crate::agent::checkpoints::finalize_redo_images(&root, &touched, &pre_undo);
                self.session.redo_stack.push(crate::session::RedoEntry {
                    label: label.clone(),
                    checkpoints: undone_pairs,
                    files: redo_files,
                    reopened_steps: reopened_steps.clone(),
                });
                self.session.checkpoints.truncate(idx);
                self.session.turn_checkpoint_start = self
                    .session
                    .turn_checkpoint_start
                    .min(self.session.checkpoints.len());
                self.arm_undo_note(&format!("undo to '{label}'"), &touched);
                // The provider's copy of the conversation still contains the
                // work that was just reverted, and a continuation reference
                // would carry the model straight back to it. The next request
                // sends the transcript the host owns instead.
                self.context_bootstrap_pending = true;
                self.session.save().ok();

                let mut note = format!("undo{cov}: reverted '{label}' ({} file(s)", touched.len());
                if !report.deleted.is_empty() {
                    note.push_str(&format!(", {} removed", report.deleted.len()));
                }
                note.push(')');
                if !reopened_steps.is_empty() {
                    note.push_str(&format!("; reopened {} step(s)", reopened_steps.len()));
                }
                let kind = if !report.skipped.is_empty() {
                    note.push_str(&format!(
                        "; left alone, changed outside sqwai: {}",
                        report.skipped.join(", ")
                    ));
                    StatusKind::Warn
                } else if !scoped {
                    note.push_str("; scope not narrowed (no file records for this checkpoint)");
                    StatusKind::Warn
                } else if unrecorded > 0 {
                    note.push_str(&format!(
                        "; {unrecorded} checkpoint(s) had no file records, their effects remain"
                    ));
                    StatusKind::Warn
                } else {
                    StatusKind::Ok
                };
                if let Some(t) = transition {
                    note = format!("{t}; {note}");
                }
                self.status(&note, kind);
                self.dirty = true;
            }
            Err(e) => self.status(&format!("undo failed: {e:#}"), StatusKind::Err),
        }
    }

    /// Revert one plan step in the current project root.
    pub(super) fn undo_step(&mut self, step: &str) {
        let root = self.project_root.clone();
        self.undo_step_in(&root, step);
    }

    /// Revert one plan step.
    ///
    /// Only layer 1 is involved: the journal knows which `file_diff` records
    /// belong to `step` against an explicit root.
    pub(super) fn undo_step_in(&mut self, root: &std::path::Path, step: &str) {
        // Step numbers restart per plan, so "3" names nothing without one — and
        // the reopen half of this command cannot run at all, which would leave
        // the tree reverted while the plan still says the step is done. Refuse
        // rather than scan the project for a number that happens to match.
        let Some(plan_id) =
            crate::plan::open_active_for_session(root, Some(&self.session.id.to_string()))
                .ok()
                .flatten()
                .map(|plan| plan.id)
        else {
            self.status(
                &format!(
                    "/undo step {step} needs an active plan: without one the host cannot tell \
                     whose step {step} this is, nor reopen it. /undo <n> rolls back the last \
                     checkpoints instead."
                ),
                StatusKind::Warn,
            );
            return;
        };
        let revert =
            match crate::agent::journal::Journal::step_pre_images_in(root, Some(&plan_id), step) {
                Ok(revert) => revert,
                Err(e) => {
                    self.status(&format!("undo step {step}: {e:#}"), StatusKind::Err);
                    return;
                }
            };
        if revert.files.is_empty() {
            let note = if revert.written_since.is_empty() {
                format!("undo step {step}: this step recorded no file writes")
            } else {
                format!(
                    "undo step {step}: nothing revertible — later steps rewrote {}",
                    revert.written_since.join(", ")
                )
            };
            self.status(&note, StatusKind::Warn);
            return;
        }
        // S1: same preflight as the full undo — the blob restore below is
        // a tree mutation like any other.
        let _restore = match self.undo_preflight() {
            Some(guard) => guard,
            None => return,
        };
        // Step reverts are always layer 1, but the toast still names the
        // mode: with layer 2 down, bash-written bytes of the window stay.
        let layer2 = crate::agent::checkpoints::available_in(root, self.cfg.undo.shadow);
        let cov = self.undo_coverage_tag(layer2);
        let transition = self.undo_transition(layer2);

        let candidate_paths: Vec<String> = revert.files.iter().map(|f| f.path.clone()).collect();
        let pre_undo = crate::agent::checkpoints::capture_pre_undo_blobs(root, &candidate_paths);

        match crate::agent::checkpoints::restore_from_blobs(root, &revert.files) {
            Ok(report) => {
                if report.failed.is_some() {
                    self.report_partial_undo(&format!("undo of step {step}"), &report);
                    return;
                }
                let touched = report.touched();
                self.arm_undo_note(&format!("undo of step {step}"), &touched);
                let mut reopened: Vec<String> = Vec::new();
                // the same plan the revert was scoped to: reopening anything
                // else would attach this session's undo to foreign work
                if let Ok(mut active) = crate::plan::open(root, &plan_id)
                    && crate::plan::reopen_for_undo(
                        &mut active,
                        step,
                        format!("reopened by undo of step {step}"),
                    )
                    .is_ok()
                {
                    let args = serde_json::json!({
                        "ids": [step],
                        "reason": format!("reopened by undo of step {step}"),
                    });
                    let sid = self.session.id.to_string();
                    if crate::plan::commit(root, &sid, &mut active, "reopen", "host", true, args)
                        .is_ok()
                    {
                        reopened.push(step.to_string());
                    }
                }
                if let Ok(mut journal) =
                    crate::agent::journal::Journal::open(root, &self.session.id.to_string())
                {
                    // host-written, like every other undo record
                    journal.set_attribution(None, Some(plan_id.clone()), "host");
                    let _ = journal.append_undo_step(step, &touched, &reopened);
                }
                let redo_files =
                    crate::agent::checkpoints::finalize_redo_images(root, &touched, &pre_undo);
                self.session.redo_stack.push(crate::session::RedoEntry {
                    label: format!("step {step}"),
                    checkpoints: Vec::new(),
                    files: redo_files,
                    reopened_steps: reopened.clone(),
                });
                // The provider's copy of the conversation still contains the
                // reverted work.
                self.context_bootstrap_pending = true;
                self.session.save().ok();

                let mut note = format!("undo step {step}{cov}: {} file(s)", touched.len());
                if !report.deleted.is_empty() {
                    note.push_str(&format!(", {} removed", report.deleted.len()));
                }
                if !reopened.is_empty() {
                    note.push_str("; step reopened");
                }
                let kind = if !revert.written_since.is_empty() {
                    note.push_str(&format!(
                        "; left alone, rewritten by a later step: {}",
                        revert.written_since.join(", ")
                    ));
                    StatusKind::Warn
                } else if !report.skipped.is_empty() {
                    note.push_str(&format!(
                        "; left alone, changed outside sqwai: {}",
                        report.skipped.join(", ")
                    ));
                    StatusKind::Warn
                } else if !report.no_pre_image.is_empty() {
                    note.push_str(&format!(
                        "; not revertible, no stored pre-image: {}",
                        report.no_pre_image.join(", ")
                    ));
                    StatusKind::Warn
                } else {
                    StatusKind::Ok
                };
                if let Some(t) = transition {
                    note = format!("{t}; {note}");
                }
                self.status(&note, kind);
                self.dirty = true;
            }
            Err(e) => self.status(&format!("undo step {step} failed: {e:#}"), StatusKind::Err),
        }
    }

    /// Undo through layer 1: write back the pre-images the host stored, remove
    /// what the window created, and leave anything edited outside sqwai alone.
    pub(super) fn undo_from_blobs(
        &mut self,
        root: &std::path::Path,
        idx: usize,
        label: &str,
        pre_images: &[crate::agent::journal::PreImage],
        unrecorded: usize,
        coverage: UndoCoverage,
    ) {
        let UndoCoverage {
            tag: cov,
            transition,
        } = coverage;
        // Called from `undo` after its own preflight; re-checking is cheap
        // and keeps this entry safe on its own.
        let _restore = match self.undo_preflight() {
            Some(guard) => guard,
            None => return,
        };
        let candidate_paths: Vec<String> = pre_images.iter().map(|p| p.path.clone()).collect();
        let pre_undo = crate::agent::checkpoints::capture_pre_undo_blobs(root, &candidate_paths);
        match crate::agent::checkpoints::restore_from_blobs(root, pre_images) {
            Ok(report) => {
                if report.failed.is_some() {
                    self.report_partial_undo(&format!("undo to '{label}'"), &report);
                    return;
                }
                let touched = report.touched();
                let undone_pairs: Vec<(String, String)> =
                    self.session.checkpoints[idx..].to_vec();
                let undone_shas: Vec<String> =
                    undone_pairs.iter().map(|(sha, _)| sha.clone()).collect();
                let sha = undone_shas.first().cloned().unwrap_or_default();
                let reopened_steps = reopen_undone_steps(
                    root,
                    &self.session.id.to_string(),
                    &touched,
                    &sha,
                    &undone_shas,
                );
                let redo_files =
                    crate::agent::checkpoints::finalize_redo_images(root, &touched, &pre_undo);
                self.session.redo_stack.push(crate::session::RedoEntry {
                    label: label.to_string(),
                    checkpoints: undone_pairs,
                    files: redo_files,
                    reopened_steps: reopened_steps.clone(),
                });
                self.session.checkpoints.truncate(idx);
                self.session.turn_checkpoint_start = self
                    .session
                    .turn_checkpoint_start
                    .min(self.session.checkpoints.len());
                self.arm_undo_note(&format!("undo to '{label}'"), &touched);
                self.context_bootstrap_pending = true;
                self.session.save().ok();

                let mut note = format!(
                    "undo{cov}: reverted '{label}' from stored pre-images ({} file(s)",
                    touched.len()
                );
                if !report.deleted.is_empty() {
                    note.push_str(&format!(", {} removed", report.deleted.len()));
                }
                note.push(')');
                if !reopened_steps.is_empty() {
                    note.push_str(&format!("; reopened {} step(s)", reopened_steps.len()));
                }
                let kind = if !report.skipped.is_empty() {
                    note.push_str(&format!(
                        "; left alone, changed outside sqwai: {}",
                        report.skipped.join(", ")
                    ));
                    StatusKind::Warn
                } else if !report.no_pre_image.is_empty() {
                    // written by the host but with nothing stored to put back
                    note.push_str(&format!(
                        "; not revertible, no stored pre-image: {}",
                        report.no_pre_image.join(", ")
                    ));
                    StatusKind::Warn
                } else if unrecorded > 0 {
                    note.push_str(&format!(
                        "; {unrecorded} checkpoint(s) had no file records, their effects remain"
                    ));
                    StatusKind::Warn
                } else {
                    StatusKind::Ok
                };
                if let Some(t) = transition {
                    note = format!("{t}; {note}");
                }
                self.status(&note, kind);
                self.dirty = true;
            }
            Err(e) => self.status(&format!("undo failed: {e:#}"), StatusKind::Err),
        }
    }

    /// Re-apply the most recent `n` undone operations from `session.redo_stack`.
    pub(super) fn redo(&mut self, n: usize) {
        let root = self.project_root.clone();
        self.redo_in(&root, n);
    }

    pub(super) fn redo_in(&mut self, root: &std::path::Path, n: usize) {
        let n = n.max(1);
        if self.session.redo_stack.is_empty() {
            self.status("nothing to redo", StatusKind::Info);
            return;
        }
        let _restore = match self.undo_preflight() {
            Some(guard) => guard,
            None => return,
        };
        let steps_to_redo = n.min(self.session.redo_stack.len());
        let mut total_touched: Vec<String> = Vec::new();
        let mut total_deleted: usize = 0;
        let mut total_skipped: Vec<String> = Vec::new();
        let mut total_restored_steps: Vec<String> = Vec::new();
        let mut last_label = String::new();

        for _ in 0..steps_to_redo {
            let Some(entry) = self.session.redo_stack.pop() else {
                break;
            };
            last_label = entry.label.clone();
            match crate::agent::checkpoints::restore_from_blobs(root, &entry.files) {
                Ok(report) => {
                    if let Some(err) = report.failed {
                        self.status(
                            &format!("redo stopped: {err}; {} file(s) re-applied", total_touched.len()),
                            StatusKind::Err,
                        );
                        self.dirty = true;
                        return;
                    }
                    let touched = report.touched();
                    total_deleted += report.deleted.len();
                    for f in &touched {
                        if !total_touched.contains(f) {
                            total_touched.push(f.clone());
                        }
                    }
                    for s in report.skipped {
                        if !total_skipped.contains(&s) {
                            total_skipped.push(s);
                        }
                    }
                    let cp_shas: Vec<String> =
                        entry.checkpoints.iter().map(|(s, _)| s.clone()).collect();
                    self.session.checkpoints.extend(entry.checkpoints);
                    if !touched.is_empty() || entry.files.is_empty() {
                        let restored = restore_redone_steps(
                            root,
                            &self.session.id.to_string(),
                            &entry.reopened_steps,
                            &cp_shas,
                            &touched,
                        );
                        for st in restored {
                            if !total_restored_steps.contains(&st) {
                                total_restored_steps.push(st);
                            }
                        }
                    }
                }
                Err(e) => {
                    self.status(&format!("redo failed: {e:#}"), StatusKind::Err);
                    return;
                }
            }
        }

        if self.session.redo_stack.is_empty() {
            self.session.pending_undo_note = None;
        }
        self.context_bootstrap_pending = true;
        self.session.save().ok();

        let mut note = format!(
            "redo: re-applied '{last_label}' ({} file(s)",
            total_touched.len()
        );
        if total_deleted > 0 {
            note.push_str(&format!(", {total_deleted} removed"));
        }
        note.push(')');
        if !total_restored_steps.is_empty() {
            note.push_str(&format!("; restored {} step(s)", total_restored_steps.len()));
        }
        let kind = if !total_skipped.is_empty() {
            note.push_str(&format!(
                "; left alone, changed outside sqwai: {}",
                total_skipped.join(", ")
            ));
            StatusKind::Warn
        } else {
            StatusKind::Ok
        };
        self.status(&note, kind);
        self.dirty = true;
    }
}
