//! Agent event polling, streaming deltas, activity group folding, and turn completion.

#![allow(unused_imports)]
use super::menus::Menu;
use super::view::SegMeta;
use super::*;

use crate::agent::loop_task::{AgentEvent, AgentOutcome};
use crate::providers::Role;
use crate::session::{ActivitySummary, TurnNote};

impl App {
    pub(super) fn poll_agent(&mut self) {
        const MAX_EVENTS_PER_TICK: usize = 16;
        let mut processed = 0;
        loop {
            if processed >= MAX_EVENTS_PER_TICK {
                return;
            }
            let ev = match self.agent.as_mut() {
                Some(handle) => match handle.rx.try_recv() {
                    Ok(ev) => ev,
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => return,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        crate::tui::event_log::log(
                            "TURN",
                            format!("agent gone without Completed aborted={}", self.aborted),
                        );
                        let result = if self.aborted {
                            Err("aborted".to_string())
                        } else {
                            Ok(())
                        };
                        self.agent = None;
                        self.finish_turn_inner(result, false);
                        return;
                    }
                },
                None => return,
            };
            if !matches!(ev, AgentEvent::TextDelta(_)) {
                processed += 1;
            }
            if self.handle_agent_event(ev) {
                return;
            }
        }
    }

    pub(super) fn handle_agent_event(&mut self, ev: AgentEvent) -> bool {
        match ev {
            AgentEvent::TextDelta(t) => {
                self.clear_retry_state();
                self.handle_text_delta(t);
            }
            AgentEvent::ThinkingDelta(t) => {
                self.handle_thinking_delta(t);
            }
            AgentEvent::Usage(u) => {
                self.session.add_usage(&u);
                self.session.context_peak_tokens =
                    self.session.context_peak_tokens.max(u.prompt_tokens);
                self.dirty = true;
            }
            AgentEvent::EffortIgnored { level, why } => {
                self.status(
                    &format!("effort: {level} (ignored by model — {why})"),
                    StatusKind::Warn,
                );
                self.effort_observed_ignored =
                    Some((self.model_cfg.id.clone(), self.model_cfg.effort, why));
                self.dirty = true;
            }
            AgentEvent::EffortHonoured => self.note_effort_honoured(),
            AgentEvent::ResponseId(id) => {
                self.session.last_response_id = Some(id);
                self.session.last_response_model = Some(self.session.model_key.clone());
                self.dirty = true;
            }
            AgentEvent::Compaction {
                summarized,
                before,
                after,
            } => self.note_compaction(summarized, before, after),
            AgentEvent::TranscriptSync { messages, summary } => {
                self.persist_transcript(messages, summary)
            }
            AgentEvent::RequestBreakdown(b) => {
                self.session.last_breakdown = Some(b);
                let fixed = b.system_tokens + b.memory_tokens + b.skills_tokens + b.host_tokens;
                crate::providers::log_http(&format!(
                    "request breakdown: system={}B memory={}B skills={}B history={}B \
                     tool_output={}B host={}B reasoning={}B total={}B fixed={}t",
                    b.system_bytes,
                    b.memory_bytes,
                    b.skills_bytes,
                    b.history_bytes,
                    b.tool_output_bytes,
                    b.host_bytes,
                    b.reasoning_bytes,
                    b.total_bytes,
                    fixed,
                ));
            }
            AgentEvent::SubagentStart { id, task } => {
                self.handle_subagent_start(id, task);
                self.dirty = true;
            }
            AgentEvent::SubagentThinking { id, text } => {
                if let Some(chat) = self.subagent_chats.get_mut(&id)
                    && let Some(Segment::Thinking { text: current, .. }) = chat
                        .iter_mut()
                        .find(|segment| matches!(segment, Segment::Thinking { live: true, .. }))
                {
                    current.push_str(&text);
                }
                self.sub_touch_all(id);
                self.dirty = true;
            }
            AgentEvent::SubagentText { id, text } => {
                if let Some(chat) = self.subagent_chats.get_mut(&id)
                    && let Some(Segment::Assistant { text: current, .. }) =
                        chat.iter_mut().rev().find(|segment| {
                            matches!(segment, Segment::Assistant { live: true, .. })
                        })
                {
                    current.push_str(&text);
                }
                if let Some(pos) = self
                    .segments
                    .iter()
                    .rposition(|s| matches!(s, Segment::Subagent { id: sid, .. } if *sid == id))
                    && let Some(Segment::Subagent { output, .. }) = self.segments.get_mut(pos)
                {
                    // Ignore pure whitespace deltas when output is empty or ends with a newline
                    // so empty lines never accumulate before or between tool calls.
                    if !(text.trim().is_empty() && (output.is_empty() || output.ends_with('\n'))) {
                        output.push_str(&text);
                        self.touch_segment(pos);
                    }
                }
                self.sub_touch_all(id);
                self.dirty = true;
            }
            AgentEvent::SubagentToolStart { id, name, summary } => {
                if let Some(pos) = self
                    .segments
                    .iter()
                    .rposition(|s| matches!(s, Segment::Subagent { id: sid, .. } if *sid == id))
                    && let Some(Segment::Subagent { output, .. }) = self.segments.get_mut(pos)
                {
                    let trimmed_len = output.trim_end().len();
                    output.truncate(trimmed_len);
                    if !output.is_empty() {
                        output.push('\n');
                    }
                    let clean_sum = summary.split_whitespace().collect::<Vec<_>>().join(" ");
                    let step_line = if clean_sum.is_empty() {
                        format!("• {name}\n")
                    } else {
                        format!("• {name} ({clean_sum})\n")
                    };
                    output.push_str(&step_line);
                    self.touch_segment(pos);
                }
                let preamble: Option<(usize, String)> = if let Some(chat) =
                    self.subagent_chats.get_mut(&id)
                {
                    chat.iter()
                        .rposition(|segment| {
                            matches!(segment, Segment::Assistant { live: true, .. })
                        })
                        .and_then(|pos| {
                            if let Some(Segment::Assistant { text, .. }) = chat.get_mut(pos) {
                                let t = text.trim();
                                if !t.is_empty() {
                                    Some((pos, std::mem::take(text)))
                                } else {
                                    text.clear();
                                    None
                                }
                            } else {
                                None
                            }
                        })
                } else {
                    None
                };
                if let Some((pos, p)) = preamble {
                    let trimmed = p.trim();
                    if !trimmed.is_empty() {
                        self.sub_insert(id, pos, Segment::Commentary(trimmed.to_string()));
                    }
                }
                self.sub_touch_all(id);
                let tool_pos = self
                    .subagent_chats
                    .get(&id)
                    .and_then(|chat| {
                        chat.iter().rposition(|segment| {
                            matches!(segment, Segment::Assistant { live: true, .. })
                        })
                    })
                    .unwrap_or_else(|| {
                        self.subagent_chats.get(&id).map(|c| c.len()).unwrap_or(0)
                    });
                self.sub_insert(
                    id,
                    tool_pos,
                    Segment::Tool {
                        name,
                        args: summary,
                        call_id: None,
                        ok: None,
                        output: String::new(),
                        diff: None,
                        preview: Vec::new(),
                        preview_total: 0,
                        expanded: false,
                        flash: None,
                        ask: None,
                    },
                );
                self.dirty = true;
            }
            AgentEvent::SubagentToolDone {
                id,
                name,
                summary,
                ok,
                diff,
            } => {
                if let Some(chat) = self.subagent_chats.get_mut(&id)
                    && let Some(Segment::Tool {
                        ok: state,
                        output,
                        diff: current_diff,
                        preview,
                        preview_total,
                        flash,
                        ..
                    }) = chat.iter_mut().rev().find(|segment| matches!(segment, Segment::Tool { name: current, ok: None, .. } if current == &name))
                {
                    *state = Some(ok);
                    *output = summary;
                    *current_diff = diff;
                    (*preview, *preview_total) =
                        view::tool_preview(current_diff.as_deref(), output.as_str());
                    *flash = Some(std::time::Instant::now());
                }
                self.sub_touch_all(id);
                self.dirty = true;
            }
            AgentEvent::SubagentDone { id, ok, output } => {
                crate::tui::event_log::log(
                    "SUBAGENT",
                    format!("done id={id} ok={ok} output_len={}", output.chars().count()),
                );
                if let Some((_, _, status, current, _)) = self
                    .subagents
                    .iter_mut()
                    .find(|(sid, _, _, _, _)| *sid == id)
                {
                    *status = if ok {
                        "completed".into()
                    } else {
                        "failed".into()
                    };
                    *current = output.clone();
                }
                if let Some(pos) = self
                    .segments
                    .iter()
                    .rposition(|s| matches!(s, Segment::Subagent { id: sid, .. } if *sid == id))
                    && let Some(Segment::Subagent {
                        status,
                        output: current,
                        ..
                    }) = self.segments.get_mut(pos)
                {
                    *status = if ok {
                        "completed".into()
                    } else {
                        "failed".into()
                    };
                    let tool_steps: Vec<String> = current
                        .lines()
                        .filter(|l| l.trim_start().starts_with("• "))
                        .map(str::to_string)
                        .collect();
                    if tool_steps.is_empty() {
                        *current = output;
                    } else if output.trim().is_empty() {
                        *current = tool_steps.join("\n");
                    } else {
                        *current = format!("{}\n{}", tool_steps.join("\n"), output.trim());
                    }
                    self.touch_segment(pos);
                }
                if let Some(chat) = self.subagent_chats.get_mut(&id) {
                    for segment in chat {
                        match segment {
                            Segment::Thinking { live, .. }
                            | Segment::Assistant { live, .. } => *live = false,
                            _ => {}
                        }
                    }
                }
                self.sub_touch_all(id);
                self.dirty = true;
            }
            AgentEvent::ToolStart {
                name,
                summary,
                call_id,
            } => {
                self.clear_retry_state();
                self.handle_tool_start(name, summary, Some(call_id));
            }
            AgentEvent::ToolNotice {
                name,
                summary,
                ok,
                diff,
                call_id,
            } => {
                if name == "skill" && ok {
                    // When the model loads a skill (`skill(name: "...")`), pin it
                    // into `self.active_skills` so `/skills` shows it active and it
                    // survives compaction.
                    let skill_name = self
                        .segments
                        .iter()
                        .rev()
                        .find(|seg| {
                            matches!(seg, Segment::Tool { call_id: Some(cid), .. } if cid == &call_id)
                        })
                        .and_then(|seg| match seg {
                            Segment::Tool { args, .. } => {
                                Some(args.split(" / ").next().unwrap_or(args.as_str()).trim().to_string())
                            }
                            _ => None,
                        })
                        .unwrap_or_default();
                    if !skill_name.is_empty()
                        && !skill_name.eq_ignore_ascii_case("list")
                        && !self
                            .active_skills
                            .iter()
                            .any(|s| s.name.eq_ignore_ascii_case(&skill_name))
                    {
                        self.refresh_skills();
                        if let Some(sk) = self
                            .discovered_skills
                            .iter()
                            .find(|s| s.name.eq_ignore_ascii_case(&skill_name))
                            .cloned()
                        {
                            self.active_skills.push(sk);
                            self.sync_active_skills_to_session();
                        }
                    }
                }
                self.handle_tool_notice(name, summary, ok, diff, Some(call_id));
            }
            AgentEvent::Checkpoint { sha, label } => {
                if !undo::is_boundary_checkpoint(&label) {
                    self.session.redo_stack.clear();
                }
                let entry = (sha, label.clone());
                if self.session.checkpoints.last() != Some(&entry) {
                    self.session.checkpoints.push(entry);
                }
                self.last_checkpoint = Some(label);
                self.dirty = true;
            }
            AgentEvent::Todos(items) => {
                self.todos = items;
                self.dirty = true;
            }
            AgentEvent::AskUser {
                id,
                call_id,
                questions,
            } => {
                self.open_ask(&call_id, id, questions);
            }
            AgentEvent::StepCurrent { step } => {
                self.session.current_step_id = step;
                self.session.save().ok();
            }
            AgentEvent::Approval {
                id,
                subject,
                reason,
                grant,
            } => {
                self.open_menu(Menu::Approval {
                    id,
                    subject,
                    reason,
                    grant,
                });
                self.dirty = true;
            }
            AgentEvent::Retry {
                attempt,
                delay_secs,
                error,
            } => {
                if !self.retry_notified {
                    self.retry_notified = true;
                    self.push_segment(Segment::Status {
                        text: format!("request failed — retrying with backoff: {error}"),
                        kind: StatusKind::Err,
                        expanded: false,
                        transient: true,
                    });
                    if self.prev_turn_ok {
                        crate::agent::notify::windows_toast(
                            "sqwai",
                            &format!(
                                "request failed — retrying for up to 1h (esc stops): {}",
                                truncate_chars(&error, 90)
                            ),
                        );
                    }
                }
                self.retry_line = Some(format!("retry #{attempt} in {delay_secs}s — {error}"));
                self.dirty = true;
            }
            AgentEvent::FallbackSwitched { from, to } => {
                self.push_segment(Segment::Status {
                    text: format!("primary model '{from}' failed; switched to fallback '{to}'"),
                    kind: StatusKind::Warn,
                    expanded: false,
                    transient: false,
                });
                self.session.model_key = to.clone();
                if let Some(mc) = self.cfg.models.get(&to) {
                    self.model_cfg = mc.clone();
                    self.session.context_limit = mc.context;
                }
                self.retry_line = None;
                self.retry_notified = false;
                self.status(
                    &format!("switched to fallback model: {to}"),
                    StatusKind::Warn,
                );
                self.dirty = true;
            }
            AgentEvent::Completed(res) => {
                crate::tui::event_log::log(
                    "TURN",
                    format!("Completed ok={} aborted={}", res.is_ok(), self.aborted),
                );
                if self.aborted {
                    self.finish_turn(Err("aborted".into()));
                    return true;
                }
                match res {
                    Ok(outcome) => self.finish_turn_ok(outcome),
                    Err(e) => self.finish_turn(Err(e)),
                }
                return true;
            }
        }
        false
    }

    pub(super) fn retire_unfinished_rows(&mut self) {
        for i in 0..self.segments.len() {
            let orphaned = matches!(&self.segments[i], Segment::Tool { ok: None, .. })
                || matches!(
                    &self.segments[i],
                    Segment::Subagent { status, .. }
                        if status.as_str() != "completed" && status.as_str() != "failed"
                );
            if !orphaned {
                continue;
            }
            match self.segments.get_mut(i) {
                Some(Segment::Tool { ok, .. }) => *ok = Some(false),
                Some(Segment::Subagent { status, .. }) => *status = "failed".into(),
                _ => {}
            }
            self.touch_segment(i);
        }
    }

    pub(super) fn freeze_thinking(&mut self, index: usize) {
        if let Some(Segment::Thinking {
            started,
            duration_ms,
            live,
            ..
        }) = self.segments.get_mut(index)
        {
            if let Some(started_at) = *started {
                *duration_ms = started_at.elapsed().as_millis() as u64;
            }
            *live = false;
        }
        self.touch_segment(index);
    }

    pub(super) fn handle_thinking_delta(&mut self, t: String) {
        if !self.thinking_open {
            self.flush_assistant_preamble_to_commentary();
            self.thinking_open = true;
            let pos = self
                .segments
                .iter()
                .rposition(|s| matches!(s, Segment::Assistant { live: true, .. }))
                .unwrap_or(self.segments.len());
            self.insert_segment(
                pos,
                Segment::Thinking {
                    text: String::new(),
                    expanded: false,
                    started: Some(std::time::Instant::now()),
                    duration_ms: 0,
                    live: true,
                },
            );
            self.thinking_idx = Some(pos);
        }
        if let Some(i) = self.thinking_idx
            && let Some(Segment::Thinking { text, started, .. }) = self.segments.get_mut(i)
        {
            if started.is_none() {
                *started = Some(std::time::Instant::now());
            }
            text.push_str(&t);
            self.touch_segment(i);
        }
        self.dirty = true;
    }

    pub(super) fn flush_assistant_preamble_to_commentary(&mut self) {
        self.reveal_chars(usize::MAX);
        let mut text = std::mem::take(&mut self.assistant_buf);
        let pos = self
            .segments
            .iter()
            .rposition(|s| matches!(s, Segment::Assistant { live: true, .. }));
        if let Some(pos) = pos {
            if text.is_empty()
                && let Some(Segment::Assistant { text: seg_text, .. }) = self.segments.get(pos)
            {
                text = seg_text.clone();
            }
            if let Some(Segment::Assistant { text: seg_text, .. }) = self.segments.get_mut(pos) {
                seg_text.clear();
            }
            self.touch_segment(pos);
        }
        let trimmed = text.trim();
        if !trimmed.is_empty()
            && let Some(pos) = pos
        {
            self.insert_segment(pos, Segment::Commentary(trimmed.to_string()));
            self.dirty = true;
        }
    }

    pub(super) fn handle_tool_start(
        &mut self,
        name: String,
        summary: String,
        call_id: Option<String>,
    ) {
        if self.thinking_open {
            if let Some(i) = self.thinking_idx.take() {
                self.freeze_thinking(i);
                let empty = matches!(
                    self.segments.get(i),
                    Some(Segment::Thinking { text, .. }) if text.is_empty()
                );
                if empty {
                    self.remove_segment(i);
                }
            }
            self.thinking_open = false;
        }
        self.flush_assistant_preamble_to_commentary();
        if name == "subagent" {
            return;
        }
        self.perf.event(&format!("tool_start {name}"));
        let tool = Segment::Tool {
            name,
            args: summary,
            call_id,
            ok: None,
            output: String::new(),
            diff: None,
            preview: Vec::new(),
            preview_total: 0,
            expanded: false,
            flash: None,
            ask: None,
        };
        let pos = self
            .segments
            .iter()
            .rposition(|s| matches!(s, Segment::Assistant { live: true, .. }))
            .unwrap_or(self.segments.len());
        self.insert_segment(pos, tool);
        self.dirty = true;
    }

    pub(super) fn handle_subagent_start(&mut self, id: u64, task: String) {
        let task = task.split_whitespace().collect::<Vec<_>>().join(" ");
        crate::tui::event_log::log(
            "SUBAGENT",
            format!("start id={id} task_len={}", task.chars().count()),
        );
        self.subagents
            .push((id, task.clone(), "running".into(), String::new(), false));
        self.subagent_chats.insert(
            id,
            vec![
                Segment::User(task.clone()),
                Segment::Thinking {
                    text: String::new(),
                    expanded: false,
                    started: None,
                    duration_ms: 0,
                    live: true,
                },
                Segment::Assistant {
                    text: String::new(),
                    live: true,
                },
            ],
        );
        let mut meta = Vec::new();
        for _ in 0..3 {
            meta.push(SegMeta {
                id: self.alloc_seg_id(),
                rev: 0,
            });
        }
        self.subagent_meta.insert(id, meta);
        let pos = self
            .segments
            .iter()
            .rposition(|s| matches!(s, Segment::Assistant { live: true, .. }))
            .unwrap_or(self.segments.len());
        self.insert_segment(
            pos,
            Segment::Subagent {
                id,
                task,
                status: "running".into(),
                output: String::new(),
                expanded: false,
            },
        );
    }

    pub(super) fn handle_tool_notice(
        &mut self,
        name: String,
        summary: String,
        ok: bool,
        diff: Option<String>,
        call_id: Option<String>,
    ) {
        if name == "subagent" {
            return;
        }
        self.perf.event(&format!("tool_done {name} ok={ok}"));
        let hit = call_id
            .as_ref()
            .and_then(|wanted| {
                self.segments.iter().position(|s| {
                    matches!(s, Segment::Tool { call_id: row_id, ok: None, .. } if row_id.as_ref() == Some(wanted))
                })
            })
            .or_else(|| {
                self.segments.iter().rposition(|s| {
                    matches!(s, Segment::Tool { name: n, ok: None, .. } if *n == name)
                })
            });
        match hit {
            Some(i) => {
                if let Some(Segment::Tool {
                    ok: slot,
                    output,
                    diff: dslot,
                    preview,
                    preview_total,
                    flash,
                    ask,
                    expanded,
                    ..
                }) = self.segments.get_mut(i)
                {
                    *slot = Some(ok);
                    *output = summary;
                    *dslot = diff;
                    (*preview, *preview_total) =
                        view::tool_preview(dslot.as_deref(), output.as_str());
                    *flash = Some(std::time::Instant::now());
                    if ask.take().is_some() {
                        *expanded = false;
                    }
                }
                self.touch_segment(i);
            }
            None => {
                self.flush_assistant_preamble_to_commentary();
                let (preview, preview_total) =
                    view::tool_preview(diff.as_deref(), summary.as_str());
                let tool = Segment::Tool {
                    name,
                    args: String::new(),
                    call_id,
                    ok: Some(ok),
                    output: summary,
                    diff,
                    preview,
                    preview_total,
                    expanded: false,
                    flash: Some(std::time::Instant::now()),
                    ask: None,
                };
                let pos = self
                    .segments
                    .iter()
                    .rposition(|s| matches!(s, Segment::Assistant { live: true, .. }))
                    .unwrap_or(self.segments.len());
                self.insert_segment(pos, tool);
            }
        }
        self.dirty = true;
    }

    pub(super) fn handle_text_delta(&mut self, t: String) {
        if self.thinking_open {
            if let Some(i) = self.thinking_idx.take() {
                self.freeze_thinking(i);
                let empty = matches!(
                    self.segments.get(i),
                    Some(Segment::Thinking { text, .. }) if text.is_empty()
                );
                if empty {
                    self.remove_segment(i);
                }
            }
            self.thinking_open = false;
        }
        self.pending_reveal.push_str(&t);
        self.dirty = true;
    }

    pub(super) fn finish_turn_ok(&mut self, outcome: AgentOutcome) {
        // Grants earned this turn belong to the session, not the run: keep
        // them for the next submit ("always allow this session").
        self.approval_grants = outcome.approval_grants;
        if outcome.messages.len() < self.session.messages.len() {
            self.session
                .rebase_turn_attachments(self.session.messages.len() - outcome.messages.len());
        }
        let prev_plain = self
            .session
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant && m.tool_calls.is_empty())
            .count();
        let mut ord = 0;
        let mut last_ord = 0;
        for m in &outcome.messages {
            if m.role == Role::Assistant && m.tool_calls.is_empty() {
                ord += 1;
                last_ord = ord;
            }
        }
        let advanced = last_ord > prev_plain;
        self.session.messages = outcome.messages;
        self.session.summary = outcome.summary;
        self.session.refresh_estimate();
        self.todos = outcome.todos;
        if !outcome.plan_todos.is_empty() {
            self.todos = outcome.plan_todos;
        }
        let start = self
            .session
            .turn_checkpoint_start
            .min(self.session.checkpoints.len());
        for entry in outcome.journal {
            if !self.session.checkpoints[start..].contains(&entry) {
                self.session.checkpoints.push(entry);
            }
        }
        self.finish_turn_inner(Ok(()), advanced);
    }

    pub(super) fn is_subagent_row(segment: &Segment) -> bool {
        matches!(segment, Segment::Subagent { .. })
            || matches!(segment, Segment::Tool { name, .. } if name == "subagent")
    }

    pub(super) fn tool_running(&self) -> bool {
        self.segments
            .iter()
            .rev()
            .any(|s| matches!(s, Segment::Tool { ok: None, .. }))
    }

    pub(super) fn animating_terms(&self) -> (bool, u32) {
        let stream = self.streaming;
        let tools = self.tool_running();
        let brand = self.last_chat.height > 1
            && self.active_subagent.is_none()
            && self.segments.is_empty()
            && self.chat_top(self.last_chat.height) <= 1;
        let gallery = brand
            || matches!(
                self.cur_menu(),
                Some(
                    Menu::TestAnim
                        | Menu::Approval { .. }
                        | Menu::ConfirmDelete { .. }
                        | Menu::ConfirmFullAccess,
                )
            );
        let mode = self
            .mode_blend
            .is_some_and(|(_, t0)| t0.elapsed().as_millis() < MODE_BLEND_MS as u128);
        let effort = self
            .effort_blend
            .is_some_and(|(_, t0)| t0.elapsed().as_millis() < MODE_BLEND_MS as u128);
        let wave = self.segments.iter().any(|s| {
            matches!(s, Segment::Tool { flash: Some(t0), ok: Some(done), .. } if t0.elapsed().as_millis() < crate::tui::shimmer::flash_ms(*done) as u128)
        });
        let animating = stream || tools || gallery || mode || effort || wave;
        let why = (u32::from(stream)
            | (u32::from(tools) << 1)
            | (u32::from(gallery) << 2)
            | (u32::from(mode) << 3)
            | (u32::from(effort) << 4)
            | (u32::from(wave) << 5))
            | if animating { 0 } else { 1 << 6 };
        (animating, why)
    }

    pub(super) fn clear_busy_statuses(&mut self) {
        if self
            .toast
            .as_ref()
            .is_some_and(|t| t.text == Self::BUSY_STATUS)
        {
            self.toast = None;
            self.dirty = true;
        }
    }

    pub(super) fn clear_subagent_ui_on_stop(&mut self) {
        self.subagents.clear();
        let removed: Vec<usize> = self
            .segments
            .iter()
            .enumerate()
            .filter(|(_, s)| Self::is_subagent_row(s))
            .map(|(i, _)| i)
            .collect();
        if !removed.is_empty() {
            self.retain_segments(|s| !Self::is_subagent_row(s));
            for g in &mut self.activity_groups {
                let before = removed.iter().filter(|i| **i < g.seg_start).count();
                let inside = removed
                    .iter()
                    .filter(|i| **i >= g.seg_start && **i < g.seg_end)
                    .count();
                g.seg_start -= before;
                g.seg_end -= before + inside;
            }
            for g in &mut self.activity_groups {
                let run = self
                    .segments
                    .get(g.seg_start..g.seg_end)
                    .unwrap_or_default();
                (g.calls, g.thinking, g.errors) = Self::tally_activity(run);
            }
        }
        self.turn_started = None;
        self.dirty = true;
    }

    pub(super) fn trailing_work_run(&self) -> Option<(usize, usize)> {
        let floor = self
            .activity_groups
            .iter()
            .map(|g| g.seg_end)
            .max()
            .unwrap_or(0)
            .min(self.segments.len());
        Self::trailing_work_run_in(&self.segments, floor)
    }

    pub(super) fn trailing_work_run_in(segs: &[Segment], floor: usize) -> Option<(usize, usize)> {
        let floor = floor.min(segs.len());
        let mut end = segs.len();
        while end > floor && matches!(segs[end - 1], Segment::Status { .. }) {
            end -= 1;
        }
        if end > floor && matches!(segs[end - 1], Segment::Assistant { .. }) {
            end -= 1;
        }
        let mut start = end;
        while start > floor
            && matches!(
                segs[start - 1],
                Segment::Thinking { .. }
                    | Segment::Tool { .. }
                    | Segment::Commentary(_)
                    | Segment::Subagent { .. }
            )
        {
            start -= 1;
        }
        if start == end {
            return None;
        }
        let has_work = segs[start..end].iter().any(|s| {
            matches!(
                s,
                Segment::Thinking { .. } | Segment::Tool { .. } | Segment::Subagent { .. }
            )
        });
        if !has_work {
            return None;
        }
        Some((start, end))
    }

    pub(super) fn build_activity_group(&self, (seg_start, seg_end): (usize, usize)) -> ActivityGroup {
        let duration_ms = self
            .turn_started
            .map(|t| t.elapsed().as_millis() as u64)
            .unwrap_or(0);
        Self::build_activity_group_in(
            &self.segments,
            (seg_start, seg_end),
            duration_ms,
            self.turn_user_index,
        )
    }

    pub(super) fn tally_activity(segs: &[Segment]) -> (usize, usize, usize) {
        let (mut calls, mut thinking, mut errors) = (0usize, 0usize, 0usize);
        for seg in segs {
            match seg {
                Segment::Tool {
                    ok: Some(false), ..
                } => {
                    calls += 1;
                    errors += 1;
                }
                Segment::Tool { .. } | Segment::Subagent { .. } => calls += 1,
                Segment::Thinking { .. } => thinking += 1,
                Segment::Commentary(_) | _ => {}
            }
        }
        (calls, thinking, errors)
    }

    pub(super) fn build_activity_group_in(
        segs: &[Segment],
        (seg_start, seg_end): (usize, usize),
        duration_ms: u64,
        turn_user: Option<usize>,
    ) -> ActivityGroup {
        let (calls, thinking, errors) = Self::tally_activity(&segs[seg_start..seg_end]);
        ActivityGroup {
            seg_start,
            seg_end,
            calls,
            thinking,
            duration_ms,
            errors,
            rejected: 0,
            turn_user,
        }
    }

    pub(super) fn finalize_activity_group(&mut self) {
        let Some(run) = self.trailing_work_run() else {
            self.turn_started = None;
            return;
        };
        let group = self.build_activity_group(run);
        self.activity_groups.push(group);
        self.turn_started = None;
    }

    pub(super) fn finish_turn(&mut self, res: Result<(), String>) {
        self.finish_turn_inner(res, true);
    }

    pub(super) fn persist_transcript(
        &mut self,
        messages: Vec<crate::providers::Message>,
        summary: Option<String>,
    ) {
        if messages.len() < self.session.messages.len() {
            self.session
                .rebase_turn_attachments(self.session.messages.len() - messages.len());
        }
        self.session.messages = messages;
        if summary.is_some() {
            self.session.summary = summary;
        }
        self.session.refresh_estimate();
        self.session.save().ok();
    }

    pub(super) fn finish_turn_inner(&mut self, res: Result<(), String>, advanced: bool) {
        crate::tui::event_log::log(
            "TURN",
            format!(
                "finish streaming={} segments={} groups={} result={:?}",
                self.streaming,
                self.segments.len(),
                self.activity_groups.len(),
                res.as_ref().err()
            ),
        );
        self.clear_busy_statuses();
        if res.is_err() {
            let note = if res.as_ref().is_err_and(|e| e == "aborted") {
                "(no answer — stopped)"
            } else {
                "(no answer — turn failed)"
            };
            self.freeze_active_ask(note);
        } else if self.active_ask_seg().is_some() {
            self.freeze_active_ask("(no answer)");
        }
        if res.as_ref().is_err_and(|error| error == "aborted") {
            self.clear_subagent_ui_on_stop();
        }
        self.reveal_chars(usize::MAX);
        let text = std::mem::take(&mut self.assistant_buf);
        self.thinking_open = false;
        self.thinking_idx = None;
        for i in 0..self.segments.len() {
            if matches!(self.segments[i], Segment::Thinking { live: true, .. }) {
                self.freeze_thinking(i);
            }
        }
        self.retire_unfinished_rows();
        let empties: Vec<usize> = self
            .segments
            .iter()
            .enumerate()
            .filter(|(_, s)| matches!(s, Segment::Thinking { text, .. } if text.is_empty()))
            .map(|(i, _)| i)
            .collect();
        for i in empties.into_iter().rev() {
            self.remove_segment(i);
        }
        let final_text = if res.is_ok() && advanced {
            self.session
                .messages
                .iter()
                .rev()
                .find(|m| m.role == Role::Assistant && m.tool_calls.is_empty())
                .map(|m| m.content.clone())
        } else {
            None
        };
        if let Some(t) = final_text {
            if let Some(pos) = self
                .segments
                .iter()
                .rposition(|s| matches!(s, Segment::Assistant { live: true, .. }))
            {
                self.set_segment(
                    pos,
                    Segment::Assistant {
                        text: t.clone(),
                        live: false,
                    },
                );
            }
        } else if !text.is_empty() {
            if let Some(pos) = self
                .segments
                .iter()
                .rposition(|s| matches!(s, Segment::Assistant { live: true, .. }))
            {
                self.set_segment(
                    pos,
                    Segment::Assistant {
                        text: text.clone(),
                        live: false,
                    },
                );
            }
            self.session.push(Role::Assistant, text.clone());
        } else if let Some(pos) = self
            .segments
            .iter()
            .rposition(|s| matches!(s, Segment::Assistant { live: true, .. }))
        {
            self.remove_segment(pos);
        }
        self.streaming = false;
        self.aborted = false;
        self.agent = None;
        self.retry_line = None;
        // A turn may have created or removed skill packages on disk; pick the
        // new catalog up so `/skills` and the popup show them without a restart.
        self.refresh_skills();
        self.prev_turn_ok = matches!(res, Ok(()));
        if res.is_err() {
            self.context_bootstrap_pending = true;
        }
        let aborted_turn = matches!(&res, Err(e) if e == "aborted");
        if aborted_turn || res.is_ok() {
            self.session.prev_turn_aborted = aborted_turn;
            self.session.save().ok();
        }
        let turn_note = match &res {
            Ok(()) => None,
            Err(e) if e == "tui closed" => None,
            Err(e) if e == "aborted" => Some(("stopped".to_string(), false)),
            Err(e) => Some((format!("error: {e}"), true)),
        };
        if let Some((note, is_error)) = &turn_note {
            let kind = if *is_error {
                StatusKind::Err
            } else {
                StatusKind::Info
            };
            self.push_segment(Segment::Status {
                text: note.clone(),
                kind,
                expanded: false,
                transient: false,
            });
        }
        if res.is_ok() {
            self.retract_transient_status();
            self.retry_notified = false;
        }
        self.finalize_activity_group();
        if let Some((text, is_error)) = turn_note
            && let Some(user_index) = self.turn_user_index.take()
        {
            self.session.turn_notes.push(TurnNote {
                user_index,
                text,
                is_error,
            });
        }
        self.session.activity = self
            .activity_groups
            .iter()
            .map(|g| ActivitySummary {
                calls: g.calls,
                thinking: g.thinking,
                duration_ms: g.duration_ms,
                errors: g.errors,
                rejected: g.rejected,
                user_index: g.turn_user,
            })
            .collect();
        self.session.save().ok();
        self.dirty = true;
        let send_queued = !self.pending_queue.is_empty()
            && self.menu_stack.is_empty()
            && (matches!(&res, Ok(())) || matches!(&res, Err(e) if e == "aborted"));
        if send_queued {
            let next = self.pending_queue.remove(0);
            self.input = Self::fresh_input(next.text);
            // Restore the queued turn's images as draft state; submit resolves
            // them against the markers that are still in its text.
            self.image_epoch = self.image_epoch.wrapping_add(1);
            self.image_marker = next
                .images
                .iter()
                .filter_map(|image| image.marker)
                .max()
                .unwrap_or(0);
            self.pending_image_jobs.clear();
            self.pending_images = next.images;
            self.submit();
        }
    }

    pub(super) fn export_session(&mut self) {
        let session = self.session.id.to_string();
        let out = crate::agent::export::export_session(
            &self.project_root,
            &session,
            &self.model_cfg.id,
            &self.session.messages,
        );
        let (md, json) = crate::agent::export::export_paths(&self.project_root, &session);
        let write = || -> anyhow::Result<(std::path::PathBuf, std::path::PathBuf)> {
            if let Some(parent) = md.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&md, &out.markdown)?;
            std::fs::write(
                &json,
                serde_json::to_string_pretty(&out.json).unwrap_or_default(),
            )?;
            Ok((md, json))
        };
        match write() {
            Ok((md, json)) => self.status(
                &format!("exported: {} + {}", md.display(), json.display()),
                StatusKind::Ok,
            ),
            Err(error) => self.status(&format!("export failed: {error:#}"), StatusKind::Err),
        }
        self.dirty = true;
    }
}
