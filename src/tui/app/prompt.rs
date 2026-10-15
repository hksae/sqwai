//! The prompt this session sends: the cached layers, the session environment,
//! and the per-request system block — assembled so that nothing which moves sits
//! in front of the history.

#![allow(unused_imports)]
use super::*;

impl App {
    /// Stable part of the system block: built-in markdown (overridable by
    /// config_dir/system.md), AGENTS.md, the static environment, durable
    /// memory, then the loaded skills — one cacheable part per layer.
    pub(super) fn stable_parts(&self) -> Vec<crate::providers::SystemPart> {
        use crate::providers::PrefixSection;
        let mut layers = crate::prompts::stable_prefix_layers();
        let root = self.project_root.clone();
        let mut loaded = crate::prompts::skills::load(&self.cfg.skills, &root);
        for selected in &self.active_skills {
            if !loaded.iter().any(|skill| skill.name == selected.name) {
                loaded.push(selected.clone());
            }
        }
        if let Some(skills) = crate::prompts::skills::prompt(&loaded) {
            layers.push((PrefixSection::Skills, skills));
        }
        layers
            .into_iter()
            .map(|(section, text)| crate::providers::SystemPart::cached(text).in_section(section))
            .collect()
    }

    pub(super) fn rebuild_session_environment(&mut self) {
        let root = self.project_root.clone();
        self.session_environment = crate::prompts::env::session_block(&root);
    }

    /// Assemble the system block for one request.
    ///
    /// Order matters: the stable prefix comes first, and everything that moves
    /// while the agent works goes last — a key change in front of the
    /// conversation re-reads the whole conversation.
    pub(super) fn system_block(&mut self) -> Vec<crate::providers::SystemPart> {
        use crate::providers::SystemPart;
        let mut parts = self.stable_parts.clone();
        if !self.session_environment.is_empty() {
            parts.push(SystemPart::cached(self.session_environment.clone()));
        }
        // Plan state rides every request (built in the turn loop, not
        // here): a turn-start snapshot would predate this turn's plan
        // mutations, leaving the model two plans — one live, one stale.
        let root = self.project_root.clone();
        // The model is never told its mode elsewhere: without this line it
        // learns Plan vs Act from the first refusal, burning a turn.
        parts.push(SystemPart::volatile(format!(
            "Mode: {}",
            match self.mode {
                Mode::Plan => "plan",
                Mode::Act => "act",
            }
        )));
        // Verify-command names from .sqwai/config.toml ([verify]): the model
        // needs them to write cmd: acceptance (/init collects them, nothing
        // else shows them).
        let checks = crate::config::Config::project_verify_commands(&root);
        if !checks.is_empty() {
            parts.push(SystemPart::volatile(format!(
                "Checks: {}",
                checks
                    .iter()
                    .map(|(name, command)| format!("{name} = {command}"))
                    .collect::<Vec<_>>()
                    .join("; ")
            )));
        }
        // `turn_facts` (clock, git status, directory tree) is populated off the
        // UI thread at the start of `run_agent` so pressing Enter never blocks
        // on git subprocesses.
        // The anchor is host-generated from this session's journal, and it
        // rides only where the transcript has actually lost detail: once a
        // compaction has dropped messages. Rebuilt fresh each turn so
        // resume/compaction never relies on a model-written summary.
        //
        // It used to ride every request while system.md called it the source of
        // truth "because earlier history may be gone" — a standing order to
        // distrust the conversation in front of the model, and the engine of a
        // live session that re-answered its own previous turn and ran five
        // identical `git_diff` calls (§18.1.5).
        if self.session.summary.is_some() {
            parts.push(SystemPart::volatile(crate::agent::context::anchor(
                &root,
                &self.session.id.to_string(),
            )));
        }
        // One-shot resume notice: only the first request after a genuine
        // restore (session loaded with history, or a compaction that changed
        // something) may claim anything was resumed. Injecting it every turn
        // while a step is merely open taught the model that context is
        // restored constantly — and it re-verified the plan before each step.
        if self.resume_notice_armed {
            self.resume_notice_armed = false;
            if !self.session.messages.is_empty()
                && let Some(notice) =
                    crate::agent::context::resume_notice(&root, &self.session.id.to_string())
            {
                parts.push(SystemPart::volatile(notice));
            }
        }
        // One-shot stopped-turn notice: the previous turn was killed by the
        // user mid-work. The transcript survived via live syncs and the plan
        // rides live, so no details here — just the rule that matters: done
        // steps are settled, continue from the pending one or report.
        if self.session.prev_turn_aborted {
            self.session.prev_turn_aborted = false;
            self.session.save().ok();
            let state =
                crate::plan::open_active_for_session(&root, Some(&self.session.id.to_string()))
                    .ok()
                    .flatten()
                    .map(|plan| {
                        let done: Vec<String> = plan
                            .steps
                            .iter()
                            .filter(|s| {
                                matches!(
                                    s.status,
                                    crate::plan::StepStatus::Done
                                        | crate::plan::StepStatus::Cancelled
                                )
                            })
                            .map(|s| s.id.clone())
                            .collect();
                        let pending: Vec<String> = plan
                            .steps
                            .iter()
                            .filter(|s| {
                                matches!(
                                    s.status,
                                    crate::plan::StepStatus::InProgress
                                        | crate::plan::StepStatus::Pending
                                        | crate::plan::StepStatus::Blocked
                                        | crate::plan::StepStatus::Reopened
                                )
                            })
                            .map(|s| s.id.clone())
                            .collect();
                        format!("done=[{}] pending=[{}]", done.join(","), pending.join(","))
                    })
                    .unwrap_or_else(|| "no active plan".to_string());
            parts.push(SystemPart::volatile(format!(
                "The previous turn was stopped by the user mid-work; the transcript above is complete. \
Steps marked done are settled — do not redo them, do not re-verify them. \
Continue from the pending step, or report to the user if the settled work looks wrong. ({state})"
            )));
        }
        // One-shot undo notice: the transcript still carries the writes that
        // were rolled back, so the host says out loud what is no longer in the
        // tree. Without it the model reads its own reverted diffs as current
        // state and keeps building on files the user just took back.
        if let Some(note) = self.session.pending_undo_note.take() {
            self.session.save().ok();
            parts.push(SystemPart::volatile(format!(
                "The user reverted work since your last turn — {note}. Those bytes are \
                 not in the tree: check a file before continuing over it, and do not \
                 re-apply a reverted edit on your own."
            )));
        }
        let runtime = crate::prompts::runtime_context();
        if !runtime.is_empty() {
            parts.push(SystemPart::volatile(runtime));
        }
        parts
    }
}
