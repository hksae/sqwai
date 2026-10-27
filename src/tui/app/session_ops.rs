//! Session submission, compaction trigger, session/model switching, and background polls.

#![allow(unused_imports)]
use super::menus::Menu;
use super::view::StoredView;
use super::*;

use std::time::Instant;

use crate::agent::loop_task::spawn_agent;
use crate::config::EffortLevel;
use crate::providers::{self, Message as PMessage, Role};
use crate::session::Session;

impl App {
    /// Write the in-memory pinned skill names back to the session record.
    pub(super) fn sync_active_skills_to_session(&mut self) {
        self.session.active_skills = self
            .active_skills
            .iter()
            .map(|skill| skill.name.clone())
            .collect();
    }

    /// Rebuild pinned skills from the session's names after a load/switch.
    /// Removed or renamed skills are dropped instead of carrying a stale body.
    fn load_active_skills_from_session(&mut self) {
        self.refresh_skills();
        let names = self.session.active_skills.clone();
        let mut loaded = Vec::new();
        for name in &names {
            if let Some(skill) = self
                .discovered_skills
                .iter()
                .find(|s| s.name.eq_ignore_ascii_case(name))
            {
                loaded.push(skill.clone());
            }
        }
        self.active_skills = loaded;
        self.sync_active_skills_to_session();
    }

    /// Files the session read or changed, newest first, for skill glob hints.
    pub(super) fn recent_touched_paths(&self) -> Vec<String> {
        const MAX_PATHS: usize = 32;
        let mut out = Vec::new();
        for message in self.session.messages.iter().rev() {
            for call in message.tool_calls.iter().rev() {
                let mut candidates: Vec<&str> = Vec::new();
                match call.name.as_str() {
                    "read" => {
                        if let Some(path) = call.args["file_path"].as_str() {
                            candidates.push(path);
                        }
                        if let Some(paths) = call.args["paths"].as_array() {
                            candidates.extend(paths.iter().filter_map(|p| p.as_str()));
                        }
                    }
                    "write" | "edit" | "outline" => {
                        if let Some(path) = call.args["file_path"]
                            .as_str()
                            .or_else(|| call.args["path"].as_str())
                        {
                            candidates.push(path);
                        }
                    }
                    _ => {}
                }
                for path in candidates {
                    let path = path.trim().replace('\\', "/");
                    if !path.is_empty() && !out.contains(&path) {
                        out.push(path);
                        if out.len() >= MAX_PATHS {
                            return out;
                        }
                    }
                }
            }
        }
        out
    }

    pub(super) fn submit(&mut self) {
        crate::tui::event_log::log(
            "SUBMIT",
            format!(
                "input_len={} input={:?}",
                self.input_text().chars().count(),
                self.input_text()
            ),
        );
        self.toast = None;
        self.retry_notified = false;
        self.retry_line = None;
        self.last_checkpoint = None;
        // Finish any image that was still decoding, so the submit either sees
        // the attachment or blocks instead of sending a bare marker.
        self.poll_image_jobs();
        // A drop delivered as typed characters may not have been scanned by
        // the tick yet when Enter arrives in the same batch.
        self.maybe_attach_dropped_path();
        // Final numbering is by text order, regardless of insert/delete history.
        self.renumber_markers();
        let mut text = self.input_text().trim().to_string();
        let mut queued_images: Option<Vec<crate::providers::ImageAttachment>> = None;
        if text.is_empty() {
            if self.startup {
                let active = self.session_plan();
                if let Some(active) = active {
                    text = if let Some(step) = active.steps.iter().find(|s| {
                        s.status == crate::plan::StepStatus::InProgress
                            || s.status == crate::plan::StepStatus::Pending
                    }) {
                        format!("Continue next plan step: {}", step.title)
                    } else {
                        "Continue next plan step".to_string()
                    };
                } else {
                    return;
                }
            } else if !self.streaming
                && !self.pending_queue.is_empty()
                && self.menu_stack.is_empty()
            {
                let queued = self.pending_queue.remove(0);
                text = queued.text;
                queued_images = Some(queued.images);
            } else {
                return;
            }
        }
        if queued_images.is_none() && self.has_pending_images_in(&text) {
            self.status("image still processing — try again", StatusKind::Info);
            return;
        }
        if self.streaming {
            if !text.starts_with('/') {
                let images = match queued_images {
                    Some(images) => images,
                    None => self.take_submitted_images(&text),
                };
                self.pending_queue
                    .push(super::attachments::QueuedPrompt { text, images });
                self.input = Self::fresh_input(String::new());
                self.reset_image_draft();
                self.status("queued — will send when the turn ends", StatusKind::Info);
                return;
            }
            let allowed = if let Some(rest) = text.strip_prefix('/') {
                let mut parts = rest.split_whitespace();
                match parts.next().unwrap_or("") {
                    "settings" | "debug" | "theme" | "mcp" | "skill" | "skills" | "providers"
                    | "models" | "sessions" | "exit" | "help" => true,
                    "plan" => parts.next().is_none(),
                    _ => false,
                }
            } else {
                false
            };
            if !allowed {
                self.show_busy_status();
                return;
            }
        }
        if !text.starts_with('/')
            && let Some(pc) = self.cfg.providers.get(&self.model_cfg.provider)
            && pc.effective_api_key(&self.model_cfg.provider).is_none()
        {
            self.status(
                &format!("provider '{}' has no api key", self.model_cfg.provider),
                StatusKind::Err,
            );
            return;
        }
        if let Some(rest) = text.strip_prefix('/') {
            self.input = Self::fresh_input(String::new());
            self.popup_dismiss = false;
            self.hover = None;
            self.reset_image_draft();
            self.command(rest);
            return;
        }
        let from_queue = queued_images.is_some();
        let images = match queued_images {
            Some(images) => images,
            None => {
                if self.model_cfg.vision == Some(false) && self.has_marked_images(&text) {
                    self.status("this model does not accept images", StatusKind::Err);
                    return;
                }
                self.take_submitted_images(&text)
            }
        };
        if !images.is_empty() && self.model_cfg.vision == Some(false) {
            let failure = "this model does not accept images";
            self.status(failure, StatusKind::Err);
            if from_queue {
                self.pending_queue
                    .insert(0, super::attachments::QueuedPrompt { text, images });
            }
            return;
        }
        self.input = Self::fresh_input(String::new());
        self.popup_dismiss = false;
        self.hover = None;
        self.reset_image_draft();
        self.startup = false;
        self.rebuild_provider();
        self.stable_parts = self.stable_parts();
        crate::providers::set_conversation_id(&self.session.id.to_string());
        self.push_segment(Segment::User(text.clone()));
        self.session.push_with_images(Role::User, &text, images);
        self.turn_user_index = Some(self.session.messages.len().saturating_sub(1));

        let root = self.project_root.clone();
        // Auto-pin any `$skill-name` mentioned inline in the user prompt, and
        // append a 1-line skill hint when the prompt matches an inactive skill's triggers.
        let mentions = {
            let discovered = self.cached_skills();
            if discovered.is_empty() {
                Vec::new()
            } else {
                crate::prompts::skills::extract_dollar_mentions(discovered, &text)
            }
        };
        for sk in mentions {
            if !self
                .active_skills
                .iter()
                .any(|s| s.name.eq_ignore_ascii_case(&sk.name))
            {
                self.active_skills.push(sk);
                self.context_bootstrap_pending = true;
            }
        }
        self.sync_active_skills_to_session();
        if self.cfg.skills.auto_load {
            let discovered = self.cached_skills();
            let active_names: Vec<String> =
                self.active_skills.iter().map(|s| s.name.clone()).collect();
            let touched_paths = self.recent_touched_paths();
            if let Some(hint) = crate::prompts::skills::match_hints(
                discovered,
                &active_names,
                &text,
                &touched_paths,
            ) && let Some(last) = self.session.messages.last_mut()
            {
                last.content.push('\n');
                last.content.push_str(&hint);
            }
        }

        let system = self.system_block();
        let msgs: Vec<PMessage> = self.session.messages.clone();
        let fallback_chain = self
            .cfg
            .resolve_fallback_chain(&self.session.model_key)
            .into_iter()
            .filter_map(|(key, mc, resolved)| {
                let provider = providers::create(&resolved).ok()?;
                let effort_support = mc.effort_support(resolved.format);
                Some(crate::agent::loop_task::FallbackCandidate {
                    key,
                    model_id: mc.id.clone(),
                    provider,
                    effort_support,
                    context_limit: mc.context,
                })
            })
            .collect();
        let input = crate::agent::loop_task::AgentInput {
            provider: self.provider.clone(),
            model_id: self.model_cfg.id.clone(),
            model_key: self.session.model_key.clone(),
            effort: if self.model_cfg.effort == EffortLevel::Off {
                None
            } else {
                Some(self.model_cfg.effort)
            },
            effort_support: self.effort_support(),
            max_tokens: None,
            system,
            messages: msgs,
            root,
            session_id: self.session.id.to_string(),
            blocked_patterns: self.cfg.safety.blocked_patterns.clone(),
            approval_mode: self.approval_mode,
            approval_grants: self.approval_grants.clone(),
            web_allow_hosts: self.cfg.web.allow_hosts.clone(),
            plan_mode: self.mode == Mode::Plan,
            context_limit: self.session.context_limit,
            vision: self.model_cfg.vision.unwrap_or(true),
            pdf: self.model_cfg.pdf.unwrap_or(false),
            tools: self.cfg.tools.clone(),
            limits: self.cfg.limits.clone(),
            active_skills: self.active_skills.iter().map(|s| s.name.clone()).collect(),
            read_ledger: self.session.read_ledger.clone(),
            enable_tools: true,
            background_baselines: true,
            read_only: self.read_only,
            mcp: self.cfg.mcp.clone(),
            skills: self.cfg.skills.clone(),
            previous_response_id: if self.context_bootstrap_pending || !self.continuation_enabled()
            {
                None
            } else {
                self.session.response_id_for(&self.session.model_key)
            },
            summary: self.session.summary.clone(),
            compact_only: false,
            compact_focus: String::new(),
            compaction: self.cfg.compaction.clone(),
            plan_limits: self.cfg.plan,
            shadow_store: self.cfg.undo.shadow,
            subagent_depth: 0,
            parent_step: None,
            parent_session: None,
            fallback_chain,
        };
        self.context_bootstrap_pending = false;
        self.session.turn_checkpoint_start = self.session.checkpoints.len();
        self.agent = Some(spawn_agent(input));
        self.streaming = true;
        self.aborted = false;
        self.assistant_buf.clear();
        self.pending_reveal.clear();
        self.turn_started = Some(Instant::now());
        self.thinking_open = false;
        self.thinking_idx = None;
        self.push_segment(Segment::Assistant {
            text: String::new(),
            live: true,
        });
        crate::tui::event_log::log(
            "TURN",
            format!(
                "start segments={} groups={}",
                self.segments.len(),
                self.activity_groups.len()
            ),
        );
        self.jump_to_bottom_on_typing();
        self.dirty = true;
    }

    pub(super) fn start_test_churn(&mut self) {
        if self.test_churn.is_some() {
            self.stop_test_churn();
            return;
        }
        if self.streaming {
            self.show_busy_status();
            return;
        }
        self.streaming = true;
        self.turn_started = Some(Instant::now());
        self.test_churn = Some(TestChurn {
            n: 0,
            last: Instant::now(),
            open: None,
        });
        self.status(
            "test churn running — type to queue, esc stops",
            StatusKind::Info,
        );
    }

    pub(super) fn tick_test_churn(&mut self) {
        const NAMES: [(&str, &str); 3] = [
            ("bash", "sleep 30"),
            ("read", "src/main.rs"),
            ("grep", "fn handle"),
        ];
        let (n, open) = match &self.test_churn {
            Some(ch) if ch.last.elapsed() >= std::time::Duration::from_millis(2500) => {
                (ch.n, ch.open.clone())
            }
            _ => return,
        };
        if let Some(name) = open {
            self.handle_tool_notice(name, String::new(), true, None, None);
        }
        let (name, args) = NAMES[n % NAMES.len()];
        self.handle_tool_start(name.into(), args.into(), None);
        if let Some(ch) = self.test_churn.as_mut() {
            ch.n = n + 1;
            ch.open = Some(name.into());
            ch.last = Instant::now();
        }
        self.dirty = true;
    }

    pub(super) fn stop_test_churn(&mut self) {
        let Some(ch) = self.test_churn.take() else {
            return;
        };
        if let Some(name) = ch.open {
            self.handle_tool_notice(name, String::new(), true, None, None);
        }
        self.streaming = false;
        self.finalize_activity_group();
        self.status(
            &format!("test churn stopped after {} fake calls", ch.n),
            StatusKind::Info,
        );
    }

    pub(super) fn start_compaction_with_focus(&mut self, focus: &str) {
        if self.streaming {
            self.show_busy_status();
            return;
        }
        if self.session.messages.is_empty() {
            self.status("nothing to compact yet", StatusKind::Warn);
            return;
        }
        self.rebuild_provider();
        self.session.strip_system_messages();
        self.stable_parts = self.stable_parts();
        let armed = self.resume_notice_armed;
        let aborted = self.session.prev_turn_aborted;
        let system = self.system_block();
        self.resume_notice_armed = armed;
        self.session.prev_turn_aborted = aborted;
        let input = crate::agent::loop_task::AgentInput {
            provider: self.provider.clone(),
            model_id: self.model_cfg.id.clone(),
            model_key: self.session.model_key.clone(),
            effort: None,
            effort_support: self.effort_support(),
            max_tokens: None,
            system,
            messages: self.session.messages.clone(),
            root: self.project_root.clone(),
            session_id: self.session.id.to_string(),
            blocked_patterns: Vec::new(),
            approval_mode: self.approval_mode,
            approval_grants: self.approval_grants.clone(),
            web_allow_hosts: Vec::new(),
            plan_mode: false,
            context_limit: self.session.context_limit,
            vision: self.model_cfg.vision.unwrap_or(true),
            pdf: self.model_cfg.pdf.unwrap_or(false),
            tools: self.cfg.tools.clone(),
            limits: self.cfg.limits.clone(),
            active_skills: self.active_skills.iter().map(|s| s.name.clone()).collect(),
            read_ledger: self.session.read_ledger.clone(),
            enable_tools: true,
            background_baselines: false,
            read_only: self.read_only,
            mcp: Default::default(),
            skills: self.cfg.skills.clone(),
            previous_response_id: None,
            summary: self.session.summary.clone(),
            compact_only: true,
            compact_focus: focus.to_string(),
            compaction: self.cfg.compaction.clone(),
            plan_limits: self.cfg.plan,
            shadow_store: self.cfg.undo.shadow,
            subagent_depth: 0,
            parent_step: None,
            parent_session: None,
            fallback_chain: Vec::new(),
        };
        self.agent = Some(spawn_agent(input));
        self.streaming = true;
        self.aborted = false;
        self.turn_user_index = None;
        self.assistant_buf.clear();
        self.status("compacting context…", StatusKind::Info);
    }

    pub(super) fn apply_session(&mut self, mut s: Session) {
        if self.session_has_messages() {
            self.merge_host_events();
            self.session.save().ok();
        }
        if !self.cfg.models.contains_key(&s.model_key) {
            s.model_key = self.cfg.last_model.clone();
        }
        if let Some(mc) = self.cfg.models.get(&s.model_key).cloned() {
            s.context_limit = mc.context;
            match self
                .cfg
                .resolve_provider(&mc)
                .and_then(|rp| providers::create(&rp))
            {
                Ok(p) => self.provider = p,
                Err(e) => self.status(&format!("model {}: {e:#}", mc.id), StatusKind::Warn),
            }
            self.model_cfg = mc;
        }
        self.context_bootstrap_pending = true;
        self.session = s;
        self.load_active_skills_from_session();
        self.resume_notice_armed = !self.session.messages.is_empty();
        crate::providers::set_conversation_id(&self.session.id.to_string());
        self.pending_queue.clear();
        self.reset_image_draft();
        if let Some(ref plan_id) = self.session.plan_id
            && crate::plan::open(&self.project_root, plan_id).is_err()
        {
            crate::tui::event_log::log(
                "PLAN",
                format!("plan {plan_id} not found or corrupted; resetting plan_id to None"),
            );
            self.session.plan_id = None;
        }
        if self.session.plan_id.is_none() {
            self.session.plan_id = crate::plan::open_active_for_session(
                &self.project_root,
                Some(&self.session.id.to_string()),
            )
            .ok()
            .flatten()
            .map(|plan| plan.id);
        }
        self.session.strip_system_messages();
        self.clear_segments();
        self.seg_cache.clear();
        self.activity_groups.clear();
        self.active_subagent = None;
        self.subagents.clear();
        self.subagent_chats.clear();
        self.subagent_meta.clear();
        self.sub_views.clear();
        self.cache_lines.clear();
        self.cache_rowseg.clear();
        self.asm_tags.clear();
        self.asm_lens.clear();
        self.main_store = StoredView::default();
        self.sub_stores.clear();
        self.stashed_main_scroll = None;
        self.todos.clear();
        self.turn_user_index = None;
        self.retry_line = None;
        self.last_checkpoint = None;
        self.prev_turn_ok = false;
        self.retry_notified = true;
        self.active_ask_id = None;
        self.ask_hover = None;
        self.ask_custom_focus = None;
        self.rebuild_session_environment();
        self.load_history_segments();
        self.startup = self.session.messages.is_empty();
        self.menu_home();
        self.follow = true;
        self.view_top = 0;
        self.sel = None;
        self.press = None;
        self.dragging = false;
        self.dirty = true;
        self.status(
            &format!(
                "session {} · {}",
                short_id(&self.session),
                truncate_chars(&self.session.title.clone(), 24)
            ),
            StatusKind::Ok,
        );
        if self.session.project.is_some()
            && !Session::project_is_here(&self.session.project, &self.project_root)
        {
            let where_born = self
                .session
                .project
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            self.status(
                &format!(
                    "session from another project ({where_born}): plan and undo links may dangle"
                ),
                StatusKind::Warn,
            );
        }
    }

    pub(super) fn start_new_session(&mut self) -> bool {
        if self.startup {
            return false;
        }
        if self.streaming {
            self.show_busy_status();
            return false;
        }
        if self.session_has_messages() {
            self.merge_host_events();
            self.session.save().ok();
        }
        self.run_undo_maintenance();
        let ctx = self.session.context_limit;
        self.session = Session::new(self.cfg.last_model.clone(), ctx);
        self.session.project = Some(self.project_root.clone());
        self.active_skills.clear();
        crate::providers::set_conversation_id(&self.session.id.to_string());
        self.pending_queue.clear();
        self.reset_image_draft();
        self.session.plan_id = crate::plan::open_active_for_session(
            &self.project_root,
            Some(&self.session.id.to_string()),
        )
        .ok()
        .flatten()
        .map(|plan| plan.id);
        self.context_bootstrap_pending = true;
        self.startup = true;
        self.clear_segments();
        self.seg_cache.clear();
        self.cache_lines.clear();
        self.cache_rowseg.clear();
        self.asm_tags.clear();
        self.asm_lens.clear();
        self.main_store = StoredView::default();
        self.sub_stores.clear();
        self.activity_groups.clear();
        self.active_subagent = None;
        self.subagents.clear();
        self.subagent_chats.clear();
        self.subagent_meta.clear();
        self.sub_views.clear();
        self.stashed_main_scroll = None;
        self.todos.clear();
        self.turn_user_index = None;
        self.retry_line = None;
        self.last_checkpoint = None;
        self.prev_turn_ok = false;
        self.retry_notified = true;
        self.active_ask_id = None;
        self.ask_hover = None;
        self.ask_custom_focus = None;
        self.rebuild_session_environment();
        self.follow = true;
        self.view_top = 0;
        self.sel = None;
        self.press = None;
        self.dragging = false;
        self.menu_home();
        self.dirty = true;
        self.status(
            &format!("session {} started", short_id(&self.session)),
            StatusKind::Ok,
        );
        true
    }

    pub(super) fn switch_model(&mut self, key: &str) {
        if self.streaming {
            self.show_busy_status();
            return;
        }
        let Some(mc) = self.cfg.models.get(key).cloned() else {
            return;
        };
        match self
            .cfg
            .resolve_provider(&mc)
            .and_then(|rp| providers::create(&rp))
        {
            Ok(p) => {
                self.model_cfg = mc;
                self.provider = p;
                self.context_bootstrap_pending = true;
                self.session.last_response_id = None;
                self.session.last_response_model = None;
                self.session.model_key = key.to_string();
                self.session.context_limit = self.model_cfg.context;
                self.cfg.last_model = key.to_string();
                self.cfg.save().ok();
                self.status(&format!("model: {key}"), StatusKind::Ok);
            }
            Err(e) => self.status(&format!("model {key}: {e:#}"), StatusKind::Err),
        }
        self.dirty = true;
    }

    pub(super) fn mode_chip_rgb(&self, now: Instant) -> (u8, u8, u8) {
        let to = self.mode.chip_rgb();
        match &self.mode_blend {
            Some((from, t0)) => {
                let el = now.duration_since(*t0).as_millis();
                if el >= MODE_BLEND_MS as u128 {
                    to
                } else {
                    crate::tui::shimmer::blend(*from, to, el as f64 / MODE_BLEND_MS as f64)
                }
            }
            None => to,
        }
    }

    /// The approval chip's sweep, re-anchored from the displayed color on
    /// a mid-sweep switch, exactly like `mode_chip_rgb`.
    pub(super) fn approval_chip_rgb(&self, now: Instant) -> (u8, u8, u8) {
        let to = self.approval_mode.chip_rgb();
        match &self.approval_blend {
            Some((from, t0)) => {
                let el = now.duration_since(*t0).as_millis();
                if el >= MODE_BLEND_MS as u128 {
                    to
                } else {
                    crate::tui::shimmer::blend(*from, to, el as f64 / MODE_BLEND_MS as f64)
                }
            }
            None => to,
        }
    }

    pub(super) const BUSY_STATUS: &'static str = "busy · esc to stop";

    pub(super) fn show_busy_status(&mut self) {
        self.status(Self::BUSY_STATUS, StatusKind::Warn);
    }

    pub(super) fn status(&mut self, text: &str, kind: StatusKind) {
        self.toast = Some(Toast {
            text: text.to_string(),
            kind,
            until: Instant::now() + TOAST_TTL,
        });
        self.dirty = true;
    }

    pub(super) fn live_toast(&mut self) -> Option<(String, StatusKind)> {
        if self
            .toast
            .as_ref()
            .is_some_and(|t| Instant::now() >= t.until)
        {
            self.toast = None;
            self.dirty = true;
        }
        self.toast.as_ref().map(|t| (t.text.clone(), t.kind))
    }

    pub(super) fn reveal_chars(&mut self, k: usize) -> bool {
        if self.pending_reveal.is_empty() {
            return false;
        }
        let end = self
            .pending_reveal
            .char_indices()
            .nth(k)
            .map(|(i, _)| i)
            .unwrap_or(self.pending_reveal.len());
        let chunk: String = self.pending_reveal.drain(..end).collect();
        self.assistant_buf.push_str(&chunk);
        if let Some(pos) = self
            .segments
            .iter()
            .rposition(|s| matches!(s, Segment::Assistant { live: true, .. }))
            && let Some(Segment::Assistant { text, .. }) = self.segments.get_mut(pos)
        {
            text.push_str(&chunk);
            self.touch_segment(pos);
        }
        !chunk.is_empty()
    }

    pub(super) fn poll_provider_check(&mut self) {
        let Some((name, rx)) = self.provider_check_rx.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(outcome) => {
                let state = match outcome {
                    Ok(detail) => ProviderCheck::Ok(detail),
                    Err(reason) => ProviderCheck::Err(reason),
                };
                self.provider_checks.insert(name, state);
                self.build_menu_rows();
                self.dirty = true;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                self.provider_check_rx = Some((name, rx));
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.provider_checks
                    .insert(name, ProviderCheck::Err("check task ended".to_string()));
                self.build_menu_rows();
                self.dirty = true;
            }
        }
    }

    pub(super) fn start_builtin_update(&mut self, manual: bool) {
        if self.builtin_update_rx.is_some() {
            if manual {
                self.status("update check already in progress…", StatusKind::Info);
            }
            return;
        }
        if manual {
            self.status("checking for built-in provider updates…", StatusKind::Info);
        }
        self.last_builtin_check = Instant::now();
        let (tx, rx) = std::sync::mpsc::channel();
        self.builtin_update_rx = Some((manual, rx));
        std::thread::spawn(move || {
            let outcome = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt
                    .block_on(crate::config::check_and_update_builtins(manual))
                    .map_err(|e| format!("{e:#}")),
                Err(e) => Err(format!("runtime: {e}")),
            };
            let _ = tx.send(outcome);
        });
    }

    pub(super) fn poll_builtin_update(&mut self) {
        let Some((manual, rx)) = self.builtin_update_rx.take() else {
            if self.last_builtin_check.elapsed()
                >= std::time::Duration::from_secs(crate::config::CATALOG_CHECK_INTERVAL_SECS)
            {
                self.start_builtin_update(false);
            }
            return;
        };
        match rx.try_recv() {
            Ok(outcome) => match outcome {
                Ok(Some(new_catalog)) => {
                    self.cfg.apply_builtins();
                    if let Ok(mc) = self.cfg.last_model_config().cloned()
                        && self.model_cfg.id == mc.id
                    {
                        self.model_cfg = mc;
                    }
                    self.build_menu_rows();
                    self.dirty = true;
                    if manual {
                        self.status(
                            &format!(
                                "built-in providers updated (#{} · {} models)",
                                new_catalog.serial,
                                new_catalog.models.len()
                            ),
                            StatusKind::Ok,
                        );
                    } else {
                        crate::tui::event_log::log(
                            "PROVIDERS",
                            format!(
                                "built-in providers updated in background (#{} · {} models)",
                                new_catalog.serial,
                                new_catalog.models.len()
                            ),
                        );
                    }
                }
                Ok(None) => {
                    if manual {
                        self.status("built-in providers are up to date", StatusKind::Info);
                    }
                }
                Err(e) => {
                    crate::tui::event_log::log(
                        "PROVIDERS",
                        format!("background update check failed: {e}"),
                    );
                    if manual {
                        self.status(&format!("update failed: {e}"), StatusKind::Err);
                    } else {
                        self.status(
                            &format!("provider catalog check failed: {e}"),
                            StatusKind::Warn,
                        );
                    }
                }
            },
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                self.builtin_update_rx = Some((manual, rx));
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {}
        }
    }
}
