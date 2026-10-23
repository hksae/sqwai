//! Menu action execution (`run_action` and its focused handlers).

#![allow(unused_imports)]
use super::menus::*;
use super::*;

use crate::config::{Config, EffortLevel, WireFormat};
use crate::session::Session;

impl App {
    pub(super) fn run_action(&mut self, action: MenuAction) {
        match action {
            MenuAction::None => {}
            MenuAction::Back => self.menu_back(),
            MenuAction::DecideApproval(decision) => self.approval_decide(decision),
            MenuAction::EnableFullAccess => {
                // the dialog is the only road in, and its Esc/cancel path
                // never reaches here — enabling is the deliberate Enter
                self.close_interaction();
                self.set_approval_mode(crate::config::ApprovalMode::Full);
            }
            MenuAction::OpenModelEffort(model) => self.open_menu(Menu::Effort { model }),
            MenuAction::OpenModelField(field) => {
                if let Some(Menu::EditModel { provider, key }) = self.cur_menu().cloned() {
                    self.open_menu(Menu::EditModelField {
                        provider,
                        key,
                        field,
                    });
                }
            }
            MenuAction::OpenProviderField(field) => {
                if let Some(Menu::EditProvider { name }) = self.cur_menu().cloned() {
                    self.open_menu(Menu::EditProviderField { name, field });
                }
            }
            MenuAction::OpenProviderFormat => {
                if let Some(Menu::EditProvider { name }) = self.cur_menu().cloned() {
                    self.open_menu(Menu::ProviderFormat { name });
                }
            }
            MenuAction::SetProviderFormat(format) => self.set_provider_format(format),
            MenuAction::OpenControls => self.open_menu(Menu::Controls),
            MenuAction::CycleUndoShadow => self.cycle_undo_shadow(),
            MenuAction::OpenUndoShadow => self.open_menu(Menu::UndoShadow),
            MenuAction::SetUndoShadow(shadow) => {
                self.cfg.undo.shadow = shadow;
                self.cfg.save().ok();
                self.status(
                    &format!("undo shadow = {}", shadow.as_str()),
                    StatusKind::Ok,
                );
                self.menu_back();
            }
            MenuAction::UndoCount(n) => {
                self.menu_home();
                self.undo(n);
            }
            MenuAction::RedoCount(n) => {
                self.menu_home();
                self.redo(n);
            }
            MenuAction::ToggleSkillsAutoLoad => {
                self.cfg.skills.auto_load = !self.cfg.skills.auto_load;
                self.cfg.save().ok();
                self.refresh_skills();
                self.stable_parts = self.stable_parts();
                self.context_bootstrap_pending = true;
                let on = self.cfg.skills.auto_load;
                self.status(&format!("skills auto-load: {}", on_off(on)), StatusKind::Ok);
                self.build_menu_rows();
            }
            MenuAction::ToggleSkill(name) => {
                let keep_sel = self.menu_sel;
                if let Some(pos) = self
                    .active_skills
                    .iter()
                    .position(|s| s.name.eq_ignore_ascii_case(&name))
                {
                    let removed = self.active_skills.remove(pos);
                    self.context_bootstrap_pending = true;
                    self.status(
                        &format!("unpinned skill: {}", removed.name),
                        StatusKind::Ok,
                    );
                } else {
                    if let Some(skill) = self
                        .cached_skills()
                        .iter()
                        .find(|s| s.name.eq_ignore_ascii_case(&name))
                        .cloned()
                    {
                        let skill_name = skill.name.clone();
                        self.active_skills.push(skill);
                        self.context_bootstrap_pending = true;
                        self.status(&format!("pinned skill: {skill_name}"), StatusKind::Ok);
                    }
                }
                self.build_menu_rows();
                self.sync_active_skills_to_session();
                if keep_sel < self.menu_len() {
                    self.menu_sel = keep_sel;
                }
                self.dirty = true;
            }
            MenuAction::ViewSkill(name) => {
                self.open_menu(Menu::SkillView { name });
            }
            MenuAction::ClearPinnedSkills => {
                if self.active_skills.is_empty() {
                    self.status("no pinned skills to clear", StatusKind::Info);
                } else {
                    let count = self.active_skills.len();
                    self.active_skills.clear();
                    self.context_bootstrap_pending = true;
                    self.sync_active_skills_to_session();
                    self.status(
                        &format!("cleared {count} pinned skill(s)"),
                        StatusKind::Ok,
                    );
                    self.build_menu_rows();
                    self.dirty = true;
                }
            }
            MenuAction::EditScalar(setting) => self.open_menu(Menu::EditScalar(setting)),
            MenuAction::OpenModels(p) => self.open_menu(Menu::Models { provider: p }),
            MenuAction::FocusSearch => {
                self.surf_set_focus(true);
                self.dirty = true;
            }
            MenuAction::EditProvider(name) => {
                self.open_menu(Menu::EditProvider { name: Some(name) })
            }
            MenuAction::DeleteProvider(p) => {
                if self.cfg.is_builtin_provider(&p) {
                    self.status("built-in provider cannot be deleted", StatusKind::Warn);
                    return;
                }
                self.open_menu(Menu::ConfirmDelete {
                    label: format!("delete provider '{p}' and all its models?"),
                    action: MenuAction::DeleteProvider(p),
                });
            }
            MenuAction::CheckProvider(name) => self.check_provider_connection(name),
            MenuAction::AddModel(p) => self.open_menu(Menu::EditModel {
                provider: p,
                key: None,
            }),
            MenuAction::EditModel(p, k) => self.open_menu(Menu::EditModel {
                provider: p,
                key: Some(k),
            }),
            MenuAction::DeleteModel(p, k) => {
                if self.cfg.is_builtin_model(&k) {
                    self.status(
                        "built-in model cannot be deleted — the catalog refills it on load",
                        StatusKind::Warn,
                    );
                    return;
                }
                self.open_menu(Menu::ConfirmDelete {
                    label: format!("delete model '{k}'?"),
                    action: MenuAction::DeleteModel(p, k),
                });
            }
            MenuAction::OpenSession(id) => self.open_session_by_id(&id),
            MenuAction::RenameSession(id) => {
                self.open_menu(Menu::EditSessionTitle { id });
            }
            MenuAction::PinSession(id) => self.pin_session_by_id(&id),
            MenuAction::ToggleTypewriter => {
                self.cfg.ui.typewriter = !self.cfg.ui.typewriter;
                self.cfg.save().ok();
                let on = self.cfg.ui.typewriter;
                self.status(&format!("typewriter: {}", on_off(on)), StatusKind::Ok);
                self.build_menu_rows();
            }
            MenuAction::ToggleOpenAttachments => {
                self.cfg.ui.open_attachments = !self.cfg.ui.open_attachments;
                self.cfg.save().ok();
                let on = self.cfg.ui.open_attachments;
                self.status(
                    &format!("open attachments on click: {}", on_off(on)),
                    StatusKind::Ok,
                );
                self.build_menu_rows();
            }
            MenuAction::ToggleMouse => {
                self.cfg.ui.mouse = !self.cfg.ui.mouse;
                self.cfg.save().ok();
                let on = self.cfg.ui.mouse;
                self.status(
                    &format!("mouse capture: {} — restart to apply", on_off(on)),
                    StatusKind::Ok,
                );
                self.build_menu_rows();
            }
            MenuAction::ToggleHttpLog => {
                self.cfg.ui.http_log = !self.cfg.ui.http_log;
                self.cfg.save().ok();
                crate::providers::set_http_log(self.cfg.ui.http_log);
                let on = self.cfg.ui.http_log;
                self.status(&format!("http debug log: {}", on_off(on)), StatusKind::Ok);
                self.build_menu_rows();
            }
            MenuAction::ToggleExperimentalTest => {
                self.cfg.ui.experimental_test = !self.cfg.ui.experimental_test;
                self.cfg.save().ok();
                let on = self.cfg.ui.experimental_test;
                self.status(
                    &format!("experimental test commands: {}", on_off(on)),
                    StatusKind::Ok,
                );
                self.build_menu_rows();
            }
            MenuAction::TogglePerfLog => {
                let on = !self.perf.enabled();
                let renders = self.test_renders;
                let wraps = crate::tui::markdown::wrap_tagged_calls();
                match self.perf.set_enabled(on, renders, wraps) {
                    Ok(msg) => self.status(&msg, StatusKind::Ok),
                    Err(msg) => self.status(&msg, StatusKind::Err),
                }
                self.build_menu_rows();
            }
            MenuAction::ClearDebugLog => {
                if let Ok(dir) = crate::config::data_dir() {
                    let path = dir.join("debug.log");
                    let _ = std::fs::write(&path, b"");
                }
                self.status("debug log cleared", StatusKind::Ok);
                self.build_menu_rows();
            }
            MenuAction::CopyDiagnostics => {
                let report = self.diagnostics_summary();
                let _ = super::clip_set(&report);
                self.status("diagnostics copied to clipboard", StatusKind::Ok);
            }
            MenuAction::UpdateCatalog => {
                self.start_builtin_update(true);
                self.build_menu_rows();
            }
            MenuAction::DeleteSession(id) => {
                let title = self
                    .sessions
                    .iter()
                    .find(|s| s.id.to_string() == *id)
                    .map(|s| truncate_chars(&s.title.clone(), 30))
                    .unwrap_or_else(|| id.chars().take(8).collect());
                self.open_menu(Menu::ConfirmDelete {
                    label: format!("delete session '{title}'?"),
                    action: MenuAction::DeleteSession(id),
                });
            }
            MenuAction::UseModel(k) => {
                self.switch_model(&k);
                self.menu_home();
            }
            MenuAction::Confirm(inner) => self.run_confirmed_action(*inner),
            MenuAction::DeletePlan => {
                self.run_action(MenuAction::Confirm(Box::new(MenuAction::DeletePlan)));
            }
            MenuAction::SetEffort(level) => self.set_effort_level(level),
            MenuAction::AskSelect { q, idx } => {
                if let Some(picked) = self.ask_picked.get_mut(q) {
                    for (i, v) in picked.iter_mut().enumerate() {
                        *v = i == idx;
                    }
                }
                self.ask_focus = q;
                self.dirty = true;
            }
            MenuAction::AskToggle { q, idx } => {
                if let Some(picked) = self.ask_picked.get_mut(q).and_then(|v| v.get_mut(idx)) {
                    *picked = !*picked;
                }
                self.ask_focus = q;
                self.dirty = true;
            }
            MenuAction::AskConfirm => self.confirm_ask_user(),
            MenuAction::AskCustom { q } => {
                self.ask_custom_focus = Some(q);
                self.ask_focus = q;
                self.dirty = true;
            }
            MenuAction::AskNext => {
                if let Some(Menu::AskUser { questions, .. }) = self.cur_menu() {
                    self.ask_focus = (self.ask_focus + 1) % questions.len().max(1);
                    self.dirty = true;
                }
            }
            MenuAction::AskPrev => {
                if let Some(Menu::AskUser { questions, .. }) = self.cur_menu() {
                    if self.ask_focus == 0 {
                        self.ask_focus = questions.len().saturating_sub(1);
                    } else {
                        self.ask_focus -= 1;
                    }
                    self.dirty = true;
                }
            }
        }
        self.dirty = true;
    }

    fn set_provider_format(&mut self, format: WireFormat) {
        if let Some(Menu::ProviderFormat { name }) = self.cur_menu().cloned() {
            if let Some(n) = &name {
                if let Some(pc) = self.cfg.providers.get_mut(n) {
                    pc.format = format;
                    self.cfg.save().ok();
                }
            } else {
                self.provider_draft.format = format;
                self.try_commit_provider_draft();
            }
            self.menu_back();
        }
    }

    fn cycle_undo_shadow(&mut self) {
        let all = crate::config::ShadowStore::ALL;
        let cur = all
            .iter()
            .position(|s| s.as_str() == self.cfg.undo.shadow.as_str())
            .unwrap_or(0);
        self.cfg.undo.shadow = all[(cur + 1) % all.len()];
        self.cfg.save().ok();
        self.status(
            &format!("undo shadow = {}", self.cfg.undo.shadow.as_str()),
            StatusKind::Ok,
        );
        self.build_menu_rows();
    }

    fn check_provider_connection(&mut self, name: String) {
        let Some(pc) = self.cfg.providers.get(&name).cloned() else {
            self.status(&format!("unknown provider '{name}'"), StatusKind::Err);
            return;
        };
        if matches!(
            self.provider_checks.get(&name),
            Some(ProviderCheck::Checking)
        ) {
            return;
        }
        let resolved = crate::config::ResolvedProvider {
            name: name.clone(),
            format: pc.format,
            base_url: pc.base_url.clone(),
            api_key: pc.effective_api_key(&name),
        };
        let (tx, rx) = std::sync::mpsc::channel();
        self.provider_check_rx = Some((name.clone(), rx));
        self.provider_checks.insert(name, ProviderCheck::Checking);
        std::thread::spawn(move || {
            let outcome = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt.block_on(crate::providers::check_connection(&resolved)),
                Err(e) => Err(format!("runtime: {e}")),
            };
            let _ = tx.send(outcome);
        });
        self.build_menu_rows();
    }

    fn open_session_by_id(&mut self, id: &str) {
        if id == self.session.id.to_string() {
            self.menu_home();
            self.status("already in this session", StatusKind::Info);
        } else if self.streaming {
            self.show_busy_status();
        } else {
            match Session::load(id) {
                Ok(s) => self.apply_session(s),
                Err(e) => self.status(&format!("load session: {e:#}"), StatusKind::Err),
            }
        }
    }

    fn pin_session_by_id(&mut self, id: &str) {
        let mut pinned = false;
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id.to_string() == id) {
            s.pinned = !s.pinned;
            pinned = s.pinned;
        }
        if let Ok(mut s) = Session::load(id) {
            s.pinned = pinned;
            let _ = s.save();
        }
        let state = if pinned { "pinned" } else { "unpinned" };
        self.status(&format!("session {state}"), StatusKind::Ok);
        if self.session.id.to_string() == id {
            self.session.pinned = pinned;
        }
        SessionHeader::sort_sessions(&mut self.sessions);
        self.build_menu_rows();
    }

    fn run_confirmed_action(&mut self, inner: MenuAction) {
        if let MenuAction::DeleteProvider(p) = &inner {
            if self.cfg.is_builtin_provider(p) {
                self.status("built-in provider cannot be deleted", StatusKind::Warn);
                return;
            }
            let removed: Vec<String> = self
                .cfg
                .models
                .iter()
                .filter(|(_, m)| &m.provider == p)
                .map(|(k, _)| k.clone())
                .collect();
            for k in &removed {
                self.cfg.models.remove(k);
            }
            self.cfg.providers.remove(p);
            if self.model_cfg.provider == *p {
                self.status(
                    "active model removed — pick a new one (/providers)",
                    StatusKind::Warn,
                );
            }
        }
        if let MenuAction::DeleteModel(_, k) = &inner {
            if self.cfg.is_builtin_model(k) {
                self.status(
                    "built-in model cannot be deleted — the catalog refills it on load",
                    StatusKind::Warn,
                );
                return;
            }
            self.cfg.models.remove(k);
            if self.session.model_key == *k {
                self.status(
                    "active model removed — pick a new one (/providers)",
                    StatusKind::Warn,
                );
            }
        }
        if let MenuAction::DeleteSession(id) = &inner {
            match Session::delete(id) {
                Ok(()) => {
                    self.sessions.retain(|s| s.id.to_string() != *id);
                    self.status("session deleted", StatusKind::Ok);
                }
                Err(e) => self.status(&format!("delete session: {e:#}"), StatusKind::Err),
            }
        }
        if matches!(&inner, MenuAction::DeletePlan) {
            self.confirm_delete_plan();
        }
        self.cfg.save().ok();
        match &inner {
            MenuAction::DeleteProvider(_) => {
                self.menu_home();
                self.open_menu(Menu::Providers);
            }
            MenuAction::DeleteModel(p, _) => {
                self.menu_stack.pop();
                if matches!(self.cur_menu(), Some(Menu::Models { .. })) {
                    self.build_menu_rows();
                    self.dirty = true;
                } else {
                    self.open_menu(Menu::Models {
                        provider: p.clone(),
                    });
                }
            }
            MenuAction::DeleteSession(_) => {
                self.menu_stack.pop();
                if matches!(self.cur_menu(), Some(Menu::Sessions)) {
                    self.build_menu_rows();
                    self.dirty = true;
                } else {
                    self.open_menu(Menu::Sessions);
                }
            }
            _ => {}
        }
    }

    fn confirm_delete_plan(&mut self) {
        self.menu_home();
        let root = self.project_root.clone();
        match self.deletable_plan_id() {
            Some(id) => {
                let plan_path = crate::plan::plans_dir(&root).join(format!("{id}.json"));
                match std::fs::remove_file(&plan_path) {
                    Ok(()) if !plan_path.exists() => {
                        self.session.plan_id = None;
                        self.session.save().ok();
                        if let Ok(mut journal) = crate::agent::journal::Journal::open(
                            &root,
                            &self.session.id.to_string(),
                        ) {
                            let _ = journal.append(
                                "plan_deleted",
                                serde_json::json!({ "plan_id": id }),
                            );
                        }
                        let sid = self.session.id.to_string();
                        let note = match crate::plan::open_active_for_session(&root, Some(&sid))
                            .ok()
                            .flatten()
                        {
                            Some(next) => {
                                let goal: String = next.goal.text.chars().take(60).collect();
                                format!(
                                    "plan deleted; now following active plan '{goal}' (see /plan)"
                                )
                            }
                            None => "plan deleted; no active plan".to_string(),
                        };
                        self.status(&note, StatusKind::Ok);
                    }
                    Ok(()) => self.status(
                        &format!(
                            "plan delete failed: {} is still on disk",
                            plan_path.display()
                        ),
                        StatusKind::Err,
                    ),
                    Err(e) => self.status(&format!("plan delete failed: {e:#}"), StatusKind::Err),
                }
            }
            None => {
                self.session.plan_id = None;
                self.session.save().ok();
                self.status("no active plan", StatusKind::Info);
            }
        }
        self.dirty = true;
    }

    fn set_effort_level(&mut self, level: EffortLevel) {
        if self.parent_is_new_edit_model() {
            self.model_draft.effort = level;
            self.menu_back();
            return;
        }
        let target = self.effort_target_key();
        if let Some(m) = self.cfg.models.get_mut(&target) {
            m.effort = level;
        }
        if target == self.session.model_key {
            self.model_cfg.effort = level;
        }
        self.cfg.save().ok();
        self.menu_back();
        let plan = crate::providers::effort::plan(level, self.effort_support_for(&target));
        let kind = if plan.is_honoured() {
            StatusKind::Ok
        } else {
            StatusKind::Warn
        };
        self.status(&plan.label(), kind);
    }

    fn confirm_ask_user(&mut self) {
        let text = match self.cur_menu() {
            Some(Menu::AskUser { questions, .. }) => {
                let mut parts = Vec::new();
                let single_no_header = questions.len() == 1 && questions[0].header.is_empty();
                for (q_idx, q) in questions.iter().enumerate() {
                    let picked: Vec<String> = self
                        .ask_picked
                        .get(q_idx)
                        .map(|v| {
                            v.iter()
                                .enumerate()
                                .filter(|(_, p)| **p)
                                .filter_map(|(i, _)| q.options.get(i))
                                .map(|o| o.label.clone())
                                .collect()
                        })
                        .unwrap_or_default();
                    let custom = self
                        .ask_custom
                        .get(q_idx)
                        .map(|s| s.trim().to_string())
                        .unwrap_or_default();
                    let mut answer = String::new();
                    if !picked.is_empty() {
                        answer.push_str(&picked.join(", "));
                    }
                    if !custom.is_empty() {
                        if !answer.is_empty() {
                            answer.push_str("; ");
                        }
                        answer.push_str(&custom);
                    }
                    let header = if single_no_header {
                        String::new()
                    } else if q.header.is_empty() {
                        format!("Q{}", q_idx + 1)
                    } else {
                        q.header.clone()
                    };
                    if single_no_header {
                        parts.push(if answer.is_empty() {
                            "(no answer)".to_string()
                        } else {
                            answer
                        });
                    } else {
                        parts.push(format!(
                            "{}: {}",
                            header,
                            if answer.is_empty() {
                                "(no answer)".to_string()
                            } else {
                                answer
                            }
                        ));
                    }
                }
                parts.join(" | ")
            }
            _ => String::new(),
        };
        self.ask_answer(text);
    }
}
