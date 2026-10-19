//! Menu row builders (`build_menu_rows` and per-surface builders).

#![allow(unused_imports)]
use super::menus::*;
use super::*;

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::config::{EffortLevel, WireFormat};
use crate::providers::Role;
use crate::tui::theme::Theme;

type MenuRow = (Line<'static>, MenuAction);

fn row(l: Line<'static>, a: MenuAction) -> MenuRow {
    (l, a)
}

impl App {
    pub(super) fn build_menu_rows(&mut self) {
        self.menu_rows.clear();
        self.menu_sticky_footer.clear();
        self.menu_table_header = None;
        self.menu_footer_text = None;
        let Some(menu) = self.cur_menu().cloned() else {
            return;
        };
        match menu {
            Menu::Settings => self.build_settings_rows(),
            Menu::UndoShadow => self.build_undo_shadow_rows(),
            Menu::UndoList => self.build_undo_list_rows(),
            Menu::EditScalar(..)
            | Menu::EditSessionTitle { .. }
            | Menu::EditProviderField { .. }
            | Menu::EditModelField { .. } => {
                self.menu_rows.push(row(Line::default(), MenuAction::None));
            }
            Menu::Help => self.build_help_rows(),
            Menu::Controls => self.build_controls_rows(),
            Menu::Debug => self.build_debug_rows(),
            Menu::Sessions => self.build_sessions_rows(),
            Menu::Providers => self.build_providers_rows(),
            Menu::EditProvider { name } => self.build_edit_provider_rows(name),
            Menu::ProviderFormat { name } => self.build_provider_format_rows(name),
            Menu::Models { provider } => self.build_models_rows(&provider),
            Menu::EditModel { key, .. } => self.build_edit_model_rows(key),
            Menu::ConfirmDelete { label, action } => self.build_confirm_delete_rows(label, action),
            Menu::Effort { model } => self.build_effort_rows(model),
            Menu::Context => self.build_context_rows(),
            Menu::TestAnim => self.build_test_anim_rows(),
            Menu::TestArt => self.build_test_art_rows(),
            Menu::TestColors => self.build_test_colors_rows(),
            Menu::TestMd => self.build_test_md_rows(),
            Menu::ContextView => self.build_context_view_rows(),
            Menu::AskUser { questions, .. } => self.build_ask_user_rows(&questions),
            Menu::Approval {
                command, reason, ..
            } => self.build_approval_rows(command, &reason),
            Menu::AskFree { .. } => {}
            Menu::Plan => self.build_plan_rows(),
        }
        if self.menu_sel >= self.menu_len() {
            self.menu_sel = self.menu_len().saturating_sub(1);
        }
    }

    fn build_settings_rows(&mut self) {
        self.menu_rows.push(row(
            Line::from(vec![Span::styled("Search", Theme::dim())]),
            MenuAction::FocusSearch,
        ));
        let q = self.config_search.lines().join("").to_lowercase();
        let entries: Vec<(&str, String, MenuAction)> = vec![
            (
                "typewriter",
                on_off(self.cfg.ui.typewriter),
                MenuAction::ToggleTypewriter,
            ),
            (
                "http debug log",
                on_off(self.cfg.ui.http_log),
                MenuAction::ToggleHttpLog,
            ),
            (
                "perf frame log",
                on_off(self.perf.enabled()),
                MenuAction::TogglePerfLog,
            ),
            (
                "experimental test commands",
                on_off(self.cfg.ui.experimental_test),
                MenuAction::ToggleExperimentalTest,
            ),
            (
                "skills auto-load",
                on_off(self.cfg.skills.auto_load),
                MenuAction::ToggleSkillsAutoLoad,
            ),
            (
                "plan max steps",
                ScalarSetting::PlanMaxSteps.current(&self.cfg),
                MenuAction::EditScalar(ScalarSetting::PlanMaxSteps),
            ),
            (
                "plan nudge after",
                ScalarSetting::PlanNudgeAfter.current(&self.cfg),
                MenuAction::EditScalar(ScalarSetting::PlanNudgeAfter),
            ),
            (
                "memory load budget ratio",
                ScalarSetting::MemoryLoadBudgetRatio.current(&self.cfg),
                MenuAction::EditScalar(ScalarSetting::MemoryLoadBudgetRatio),
            ),
            (
                "memory max tokens",
                ScalarSetting::MemoryMaxTokens.current(&self.cfg),
                MenuAction::EditScalar(ScalarSetting::MemoryMaxTokens),
            ),
            (
                "memory max proposals",
                ScalarSetting::MemoryMaxProposals.current(&self.cfg),
                MenuAction::EditScalar(ScalarSetting::MemoryMaxProposals),
            ),
            (
                "compaction threshold",
                ScalarSetting::CompactionThreshold.current(&self.cfg),
                MenuAction::EditScalar(ScalarSetting::CompactionThreshold),
            ),
            (
                "compaction keep turns",
                ScalarSetting::CompactionKeepTurns.current(&self.cfg),
                MenuAction::EditScalar(ScalarSetting::CompactionKeepTurns),
            ),
            (
                "compaction anchor ratio",
                ScalarSetting::CompactionAnchorRatio.current(&self.cfg),
                MenuAction::EditScalar(ScalarSetting::CompactionAnchorRatio),
            ),
            (
                "undo keep per session",
                ScalarSetting::UndoKeepPerSession.current(&self.cfg),
                MenuAction::EditScalar(ScalarSetting::UndoKeepPerSession),
            ),
            (
                "undo blob grace secs",
                ScalarSetting::UndoBlobGraceSecs.current(&self.cfg),
                MenuAction::EditScalar(ScalarSetting::UndoBlobGraceSecs),
            ),
            (
                "undo shadow",
                self.cfg.undo.shadow.as_str().to_string(),
                MenuAction::OpenUndoShadow,
            ),
            (
                "undo shadow max bytes",
                ScalarSetting::UndoShadowMaxBytes.current(&self.cfg),
                MenuAction::EditScalar(ScalarSetting::UndoShadowMaxBytes),
            ),
        ];
        let sel_before = self.menu_sel;
        for (label, val, action) in entries {
            if !q.is_empty()
                && !label.to_lowercase().contains(&q)
                && !val.to_lowercase().contains(&q)
            {
                continue;
            }
            self.menu_rows.push(row(
                Line::from(vec![
                    Span::styled(format!("{label:<28}"), Style::new().fg(Theme::FG())),
                    Span::styled(val, Theme::dim()),
                ]),
                action,
            ));
        }
        self.menu_sel = sel_before;
    }

    fn build_undo_shadow_rows(&mut self) {
        let current = self.cfg.undo.shadow;
        for s in crate::config::ShadowStore::ALL {
            let style = if s == current {
                Theme::brand_ink()
            } else {
                Style::new().fg(Theme::FG())
            };
            self.menu_rows.push(row(
                Line::from(vec![Span::styled(s.as_str(), style)]),
                MenuAction::SetUndoShadow(s),
            ));
        }
    }

    fn build_undo_list_rows(&mut self) {
        for (i, entry) in self.session.redo_stack.iter().rev().enumerate() {
            let n = i + 1;
            self.menu_rows.push(row(
                Line::from(vec![
                    Span::styled(format!("redo {n:<3}"), Theme::ok()),
                    Span::styled(entry.label.clone(), Style::new().fg(Theme::FG())),
                    Span::styled(format!(" ({} file(s))", entry.files.len()), Theme::dim()),
                ]),
                MenuAction::RedoCount(n),
            ));
        }
        let total = self.session.checkpoints.len();
        for (i, (sha, label)) in self.session.checkpoints.iter().rev().enumerate() {
            let n = i + 1;
            let idx = total - n;
            let in_latest_turn = idx >= self.session.turn_checkpoint_start;
            let short_sha: String = sha.chars().take(12).collect();
            let mut spans = vec![
                Span::styled(
                    format!("undo {n:<3}"),
                    if in_latest_turn {
                        Theme::brand_ink()
                    } else {
                        Theme::meta()
                    },
                ),
                Span::styled(label.clone(), Style::new().fg(Theme::FG())),
                Span::styled(format!(" ({short_sha})"), Theme::dim()),
            ];
            if in_latest_turn {
                spans.push(Span::styled(" · this turn", Theme::dim()));
            }
            self.menu_rows
                .push(row(Line::from(spans), MenuAction::UndoCount(n)));
        }
    }

    fn build_help_rows(&mut self) {
        const SHORTCUTS: &[(&str, &str)] = &[
            ("enter", "send message"),
            ("ctrl+j", "newline"),
            ("tab", "switch plan / act mode"),
            ("esc", "stop active turn"),
            ("ctrl+c", "copy selection / stop turn"),
            ("ctrl+d", "exit when input is empty"),
            ("ctrl+g", "models"),
            ("ctrl+p", "providers"),
            ("ctrl+e", "reasoning effort"),
            ("ctrl+s", "sessions"),
            ("ctrl+l", "plan overview"),
            ("ctrl+o", "config"),
        ];
        for (combo, what) in SHORTCUTS {
            self.menu_rows.push(row(
                Line::from(vec![
                    Span::styled(format!("{combo:<12}"), Style::new().fg(Theme::FG())),
                    Span::styled((*what).to_string(), Theme::dim()),
                ]),
                MenuAction::None,
            ));
        }
    }

    fn build_controls_rows(&mut self) {
        const CONTROLS: &[(&str, &str)] = &[
            ("enter", "submit · confirm"),
            ("tab", "plan / act mode"),
            ("esc", "cancel · back"),
            ("ctrl+c", "stop · copy"),
            ("ctrl+d", "quit if empty"),
            ("ctrl+j", "newline"),
            ("ctrl+v", "paste"),
            ("ctrl+s", "sessions"),
            ("ctrl+p", "providers"),
            ("ctrl+l", "plan"),
            ("ctrl+o", "settings"),
            ("ctrl+e", "effort"),
            ("ctrl+g", "switch model"),
            ("y / n", "confirm · cancel"),
            ("a / d", "allow always · deny"),
            ("1-9", "pick inline option"),
        ];
        for (combo, what) in CONTROLS {
            self.menu_rows.push(row(
                Line::from(vec![
                    Span::styled(format!("  {combo:<10}"), Theme::accent_bold()),
                    Span::styled((*what).to_string(), Theme::dim()),
                ]),
                MenuAction::None,
            ));
        }
        self.menu_footer_text = Some("esc: close".into());
    }

    fn build_debug_rows(&mut self) {
        let act = |l: &str, val: String, a: MenuAction| {
            row(
                Line::from(vec![
                    Span::styled(format!("{l:<24}"), Style::new().fg(Theme::FG())),
                    Span::styled(val, Theme::dim()),
                ]),
                a,
            )
        };
        let info = |l: &str, v: String| {
            row(
                Line::from(vec![
                    Span::styled(format!("{l:<24}"), Theme::dim()),
                    Span::styled(v, Theme::base()),
                ]),
                MenuAction::None,
            )
        };
        let log_path_buf = crate::config::data_dir()
            .map(|d| d.join("debug.log"))
            .ok();
        let log_size = log_path_buf
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .unwrap_or(0);
        let log_size_str = if log_size == 0 {
            "empty".to_string()
        } else {
            format!("{} B", log_size)
        };
        let cat = crate::config::BuiltinCatalog::current();
        let cat_date = cat
            .updated_at
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("undated");
        self.menu_rows.push(act(
            "clear debug log",
            log_size_str,
            MenuAction::ClearDebugLog,
        ));
        self.menu_rows.push(act(
            "copy diagnostics",
            "to clipboard".to_string(),
            MenuAction::CopyDiagnostics,
        ));
        self.menu_rows.push(act(
            "check catalog update",
            format!("#{} · {cat_date}", cat.serial),
            MenuAction::UpdateCatalog,
        ));
        self.menu_rows.push(row(Line::default(), MenuAction::None));

        let wire = self
            .cfg
            .providers
            .get(&self.model_cfg.provider)
            .map(|p| p.format)
            .unwrap_or(WireFormat::Openai);
        let support = self.model_cfg.effort_support(wire);
        let key_state = self
            .cfg
            .providers
            .get(&self.model_cfg.provider)
            .and_then(|pc| {
                pc.key_env_name(&self.model_cfg.provider)
                    .map(|n| format!("env ${n}"))
                    .or_else(|| {
                        pc.api_key
                            .as_deref()
                            .is_some_and(|k| !k.is_empty())
                            .then(|| "inline".to_string())
                    })
            })
            .unwrap_or_else(|| "none".into());
        let cont_state = if !self.continuation_enabled() {
            "off".to_string()
        } else if let Some(prev) = &self.session.last_response_id {
            let short: String = prev.chars().take(16).collect();
            format!("on ({short}…)")
        } else {
            "on (no response id yet)".to_string()
        };
        let pct = self.session.context_percent();
        let cache_note = if self.session.cache_confirmed {
            "cache confirmed"
        } else {
            "no cache hits yet"
        };
        let ctx_info = format!(
            "{} / {} ({pct}%) · {cache_note}",
            fmt_k(self.session.context_tokens_used()),
            fmt_ctx(self.session.context_limit),
        );
        let undo_info = format!(
            "shadow {} · {} checkpoints",
            self.cfg.undo.shadow.as_str(),
            self.session.checkpoints.len()
        );
        let data_dir = crate::config::data_dir()
            .map(|d| d.display().to_string())
            .unwrap_or_else(|_| "unknown".into());
        let log_path = log_path_buf
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "debug.log".into());
        let cfg_dir = crate::config::config_path()
            .map(|p| p.display().to_string())
            .unwrap_or_default();

        self.menu_rows.push(info(
            "session",
            format!("{} ({} msgs)", self.session.id, self.session.messages.len()),
        ));
        self.menu_rows.push(info(
            "model",
            format!(
                "{} (id: {} · effort: {})",
                self.session.model_key,
                self.model_cfg.id,
                self.model_cfg.effort.as_str()
            ),
        ));
        self.menu_rows.push(info(
            "provider",
            format!(
                "{} ({} · control: {})",
                self.model_cfg.provider,
                wire.as_str(),
                support.control.as_str()
            ),
        ));
        self.menu_rows.push(info("api key", key_state));
        self.menu_rows.push(info("continuation", cont_state));
        self.menu_rows.push(info("context", ctx_info));
        self.menu_rows.push(info("undo", undo_info));
        self.menu_rows
            .push(info("project root", self.project_root.display().to_string()));
        self.menu_rows.push(info("data dir", data_dir));
        self.menu_rows.push(info("log file", log_path));
        self.menu_rows.push(info("config", cfg_dir));
    }

    fn build_sessions_rows(&mut self) {
        self.menu_rows.push(row(
            Line::from(vec![Span::styled("Search", Theme::dim())]),
            MenuAction::FocusSearch,
        ));
        let q = self.session_search.lines().join("").to_lowercase();
        let visible: Vec<&SessionHeader> = self
            .sessions
            .iter()
            .filter(|s| {
                q.is_empty()
                    || s.title.to_lowercase().contains(&q)
                    || s.model_key.to_lowercase().contains(&q)
            })
            .collect();
        let cur_id = self.session.id.to_string();
        let pinned: Vec<&SessionHeader> = visible.iter().filter(|s| s.pinned).copied().collect();
        let (mine, foreign): (Vec<&&SessionHeader>, Vec<&&SessionHeader>) =
            visible.iter().partition(|s| {
                s.pinned || crate::session::Session::project_is_here(&s.project, &self.project_root)
            });
        if !pinned.is_empty() {
            self.menu_rows.push(row(
                Line::from(vec![Span::styled(
                    "Pinned".to_string(),
                    day_section_style(),
                )]),
                MenuAction::None,
            ));
            for s in &pinned {
                self.menu_rows
                    .push(session_row(s, s.id.to_string() == cur_id));
            }
        }
        let mut day = String::new();
        for s in mine {
            if s.pinned {
                continue;
            }
            let section = day_section(s.last_activity());
            if section != day {
                day = section.clone();
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(section, day_section_style())]),
                    MenuAction::None,
                ));
            }
            self.menu_rows
                .push(session_row(s, s.id.to_string() == cur_id));
        }
        if !foreign.is_empty() {
            self.menu_rows.push(row(
                Line::from(vec![Span::styled(
                    "Other projects".to_string(),
                    day_section_style(),
                )]),
                MenuAction::None,
            ));
            for s in foreign {
                self.menu_rows
                    .push(session_row(s, s.id.to_string() == cur_id));
            }
        }
        if visible.is_empty() {
            let note = if q.is_empty() {
                "(no saved sessions yet)"
            } else {
                "(no matches)"
            };
            self.menu_rows.push(row(
                Line::from(vec![Span::styled(note.to_string(), Theme::dim())]),
                MenuAction::None,
            ));
        }
    }

    fn build_providers_rows(&mut self) {
        self.menu_rows.push(row(
            Line::from(vec![Span::styled("Search", Theme::dim())]),
            MenuAction::FocusSearch,
        ));
        let applied = self.model_cfg.provider.clone();
        let q = self.prov_search.lines().join("").to_lowercase();
        for (name, _pc) in &self.cfg.providers {
            if !q.is_empty() && !name.to_lowercase().contains(&q) {
                continue;
            }
            let style = if *name == applied {
                Theme::brand_ink()
            } else {
                Style::new().fg(Theme::FG())
            };
            self.menu_rows.push(row(
                Line::from(vec![Span::styled(name.clone(), style)]),
                MenuAction::OpenModels(name.clone()),
            ));
        }
    }

    fn build_edit_provider_rows(&mut self, name: Option<String>) {
        let is_builtin = name
            .as_ref()
            .is_some_and(|n| self.cfg.is_builtin_provider(n));
        let (name_val, fmt_val, url_val, key_val, env_val) =
            match name.as_ref().and_then(|n| self.cfg.providers.get(n)) {
                Some(pc) => (
                    name.clone().unwrap_or_default(),
                    pc.format.as_str().to_string(),
                    pc.base_url.clone(),
                    super::forms::FormField::mask_value(pc.api_key.as_deref().unwrap_or("")),
                    pc.api_key_env.clone().unwrap_or_default(),
                ),
                None => (
                    name.clone().unwrap_or_else(|| self.provider_draft.name.clone()),
                    self.provider_draft.format.as_str().to_string(),
                    self.provider_draft.base_url.clone(),
                    super::forms::FormField::mask_value(&self.provider_draft.api_key),
                    self.provider_draft.api_key_env.clone(),
                ),
            };
        let field_row = |label: &'static str, val: String, action: MenuAction| {
            let mut spans = vec![Span::styled(label, Style::new().fg(Theme::FG()))];
            if !val.is_empty() {
                spans.push(Span::styled(format!(" {val}"), Theme::dim()));
            }
            row(Line::from(spans), action)
        };
        if !is_builtin {
            self.menu_rows.push(field_row(
                "name",
                name_val,
                MenuAction::OpenProviderField(ProviderField::Name),
            ));
            self.menu_rows.push(field_row(
                "format",
                fmt_val,
                MenuAction::OpenProviderFormat,
            ));
            self.menu_rows.push(field_row(
                "base url",
                url_val,
                MenuAction::OpenProviderField(ProviderField::BaseUrl),
            ));
        }
        self.menu_rows.push(field_row(
            "api key",
            key_val,
            MenuAction::OpenProviderField(ProviderField::ApiKey),
        ));
        self.menu_rows.push(field_row(
            "key env var",
            env_val,
            MenuAction::OpenProviderField(ProviderField::KeyEnvVar),
        ));
    }

    fn build_provider_format_rows(&mut self, name: Option<String>) {
        let current = name
            .as_ref()
            .and_then(|n| self.cfg.providers.get(n))
            .map(|pc| pc.format)
            .unwrap_or(self.provider_draft.format);
        for fmt in WireFormat::ALL {
            let style = if fmt == current {
                Theme::brand_ink()
            } else {
                Style::new().fg(Theme::FG())
            };
            self.menu_rows.push(row(
                Line::from(vec![Span::styled(fmt.as_str(), style)]),
                MenuAction::SetProviderFormat(fmt),
            ));
        }
    }

    fn build_models_rows(&mut self, provider: &str) {
        self.menu_rows.push(row(
            Line::from(vec![Span::styled("Search", Theme::dim())]),
            MenuAction::FocusSearch,
        ));
        let current = self.session.model_key.clone();
        let q = self.model_search.lines().join("").to_lowercase();
        for (k, m) in &self.cfg.models {
            if m.provider != provider || !m.status.visible_in_picker() {
                continue;
            }
            if !q.is_empty() && !k.to_lowercase().contains(&q) {
                continue;
            }
            let label = crate::providers::effort::plan(
                m.effort,
                m.effort_support(
                    self.cfg
                        .providers
                        .get(&m.provider)
                        .map(|p| p.format)
                        .unwrap_or(WireFormat::Openai),
                ),
            )
            .short_label();
            let effort = label.strip_prefix("ef:").unwrap_or(&label);
            let tail = if effort == "off" {
                format!(" ({})", fmt_ctx(m.context))
            } else {
                format!(" ({} · {effort})", fmt_ctx(m.context))
            };
            let style = if *k == current {
                Theme::brand_ink()
            } else {
                Style::new().fg(Theme::FG())
            };
            self.menu_rows.push(row(
                Line::from(vec![
                    Span::styled(k.clone(), style),
                    Span::styled(tail, Theme::dim()),
                ]),
                MenuAction::UseModel(k.clone()),
            ));
        }
    }

    fn build_edit_model_rows(&mut self, key: Option<String>) {
        let (k_val, id_val, ctx_val, ef_val) =
            match key.as_ref().and_then(|k| self.cfg.models.get(k)) {
                Some(mc) => (
                    key.clone().unwrap_or_default(),
                    mc.id.clone(),
                    mc.context.to_string(),
                    mc.effort.as_str().to_string(),
                ),
                None => (
                    key.clone().unwrap_or_else(|| self.model_draft.key.clone()),
                    self.model_draft.id.clone(),
                    self.model_draft.context.to_string(),
                    self.model_draft.effort.as_str().to_string(),
                ),
            };
        let field_row = |label: &'static str, val: String, action: MenuAction| {
            let mut spans = vec![Span::styled(label, Style::new().fg(Theme::FG()))];
            if !val.is_empty() {
                spans.push(Span::styled(format!(" {val}"), Theme::dim()));
            }
            row(Line::from(spans), action)
        };
        self.menu_rows.push(field_row(
            "key",
            k_val,
            MenuAction::OpenModelField(ModelField::Key),
        ));
        self.menu_rows.push(field_row(
            "request id",
            id_val,
            MenuAction::OpenModelField(ModelField::RequestId),
        ));
        self.menu_rows.push(field_row(
            "context",
            ctx_val,
            MenuAction::OpenModelField(ModelField::Context),
        ));
        self.menu_rows.push(field_row(
            "effort",
            ef_val,
            MenuAction::OpenModelEffort(key),
        ));
    }

    fn build_confirm_delete_rows(&mut self, label: String, action: MenuAction) {
        self.menu_rows.push(row(
            Line::from(vec![Span::styled(label, Theme::WARN())]),
            MenuAction::None,
        ));
        self.menu_rows.push(row(Line::default(), MenuAction::None));
        self.menu_rows.push(row(
            Line::from(vec![Span::styled("cancel", Style::new().fg(Theme::FG()))]),
            MenuAction::Back,
        ));
        self.menu_rows.push(row(
            Line::from(vec![Span::styled("delete", Theme::ERR())]),
            MenuAction::Confirm(Box::new(action)),
        ));
    }

    fn build_effort_rows(&mut self, model: Option<String>) {
        let target = model.unwrap_or_else(|| self.session.model_key.clone());
        let target_effort = self
            .cfg
            .models
            .get(&target)
            .map(|m| m.effort)
            .unwrap_or(self.model_cfg.effort);
        let support = self.effort_support_for(&target);
        let plans: Vec<(EffortLevel, crate::providers::effort::Plan)> = EffortLevel::SELECTABLE
            .iter()
            .map(|lvl| (*lvl, crate::providers::effort::plan(*lvl, support)))
            .collect();
        for (lvl, plan) in plans {
            let current = lvl == target_effort;
            let note = match plan.status {
                crate::providers::effort::Status::Applied => String::new(),
                crate::providers::effort::Status::Clamped { to } => format!("  → sent as {to}"),
                crate::providers::effort::Status::Ignored { why } => {
                    format!("  → ignored by model ({why})")
                }
            };
            let name_style = if plan.is_honoured() {
                Theme::base()
            } else {
                Theme::dim()
            };
            let lead = if current { "*" } else { " " };
            self.menu_rows.push(row(
                Line::from(vec![
                    Span::styled(lead.to_string(), Theme::accent()),
                    Span::styled(format!(" {}", lvl.as_str()), name_style),
                    Span::styled(note, Theme::dim()),
                ]),
                MenuAction::SetEffort(lvl),
            ));
        }
    }

    fn build_context_rows(&mut self) {
        let session = &self.session;
        let b = session.last_breakdown;
        let mut ranked: Vec<(&str, u64)> = vec![
            ("system", b.map(|b| b.system_tokens).unwrap_or(0)),
            ("memory", b.map(|b| b.memory_tokens).unwrap_or(0)),
            ("skills", b.map(|b| b.skills_tokens).unwrap_or(0)),
            ("history", b.map(|b| b.history_tokens).unwrap_or(0)),
            ("tools", b.map(|b| b.tool_output_tokens).unwrap_or(0)),
            ("reasoning", b.map(|b| b.reasoning_tokens).unwrap_or(0)),
            ("host block", b.map(|b| b.host_tokens).unwrap_or(0)),
        ];
        ranked.sort_by_key(|a| std::cmp::Reverse(a.1));
        let parts: Vec<(&str, u64, ratatui::style::Color)> = ranked
            .iter()
            .enumerate()
            .map(|(i, (name, tokens))| {
                let (r, g, b) = crate::tui::art::coral_at(i, ranked.len());
                (*name, *tokens, ratatui::style::Color::Rgb(r, g, b))
            })
            .collect();

        let used = session.context_tokens_used();
        let limit = session.context_limit.max(1);
        self.menu_rows.push(row(
            Line::from(vec![
                Span::styled(session.model_key.clone(), Theme::FG()),
                Span::styled(
                    format!(" · used {} ({:.1}%)", fmt_k(used), session.context_percent()),
                    Theme::dim(),
                ),
            ]),
            MenuAction::None,
        ));
        self.menu_rows
            .push(row(super::view::blank(), MenuAction::None));

        let width = (self.menu_rect.width as usize)
            .saturating_sub(4)
            .clamp(20, 120);
        let mut widths: Vec<usize> = parts
            .iter()
            .map(|(_, tokens, _)| ((*tokens as f64 / limit as f64) * width as f64) as usize)
            .collect();
        for (w, (_, tokens, _)) in widths.iter_mut().zip(&parts) {
            if *tokens > 0 {
                *w = (*w).max(1);
            }
        }
        while widths.iter().sum::<usize>() > width {
            let (i, _) = widths
                .iter()
                .enumerate()
                .max_by_key(|(_, w)| *w)
                .expect("parts is non-empty");
            widths[i] -= 1;
        }
        let free_cells = width - widths.iter().sum::<usize>();
        let mut meter = Vec::new();
        for ((_, _, color), cells) in parts.iter().zip(&widths) {
            if *cells == 0 {
                continue;
            }
            meter.push(Span::styled(" ".repeat(*cells), Style::new().bg(*color)));
        }
        if free_cells > 0 {
            meter.push(Span::styled(
                " ".repeat(free_cells),
                Style::new().bg(Theme::INPUT_BG()),
            ));
        }
        self.menu_rows
            .push(row(Line::from(meter), MenuAction::None));
        self.menu_rows
            .push(row(super::view::blank(), MenuAction::None));

        let used_pct = session.context_percent();
        let covered: u64 = parts.iter().map(|p| p.1).sum();
        let unaccounted = session
            .last_usage
            .as_ref()
            .map(|u| u.prompt_tokens.saturating_sub(covered))
            .unwrap_or(0);
        let scale = used_pct / (covered + unaccounted).max(1) as f64;
        let part_pct = |tokens: u64| tokens as f64 * scale;
        let unaccounted_pct = unaccounted as f64 * scale;
        for (name, tokens, color) in &parts {
            self.menu_rows.push(row(
                Line::from(vec![
                    Span::styled("● ".to_string(), Style::new().fg(*color)),
                    Span::styled(format!("{:<11}", name), Theme::FG()),
                    Span::styled(format!("{:>7}", fmt_k(*tokens)), Theme::meta()),
                    Span::styled(format!("{:>6.1}%", part_pct(*tokens)), Theme::dim()),
                ]),
                MenuAction::None,
            ));
        }
        let free = limit.saturating_sub(used);
        let free_pct = 100.0 - used_pct;
        self.menu_rows.push(row(
            Line::from(vec![
                Span::styled("○ ".to_string(), Theme::dim()),
                Span::styled(format!("{:<11}", "free"), Theme::FG()),
                Span::styled(format!("{:>7}", fmt_k(free)), Theme::meta()),
                Span::styled(format!("{:>6.1}%", free_pct), Theme::dim()),
            ]),
            MenuAction::None,
        ));
        if unaccounted > 0 {
            self.menu_rows.push(row(
                Line::from(vec![
                    Span::styled("◌ ".to_string(), Theme::dim()),
                    Span::styled(format!("{:<11}", "unaccounted"), Theme::FG()),
                    Span::styled(format!("{:>7}", fmt_k(unaccounted)), Theme::meta()),
                    Span::styled(format!("{:>6.1}%", unaccounted_pct), Theme::dim()),
                ]),
                MenuAction::None,
            ));
        }
    }

    fn build_test_anim_rows(&mut self) {
        const STRIP: usize = 24;
        let tick = self.spinner_tick / 2;
        let loud = {
            let (r, g, b) = crate::tui::art::CORAL_START;
            Style::new().fg(ratatui::style::Color::Rgb(r, g, b))
        };
        for set in crate::tui::anim::sets() {
            let all: Vec<char> = set.frames.chars().collect();
            let rest = all
                .len()
                .checked_sub(STRIP)
                .map(|n| format!(" +{n}"))
                .unwrap_or_default();
            let shown: String = all.iter().take(STRIP).collect();
            self.menu_rows.push(row(
                Line::from(vec![
                    Span::styled(
                        format!("{} ", crate::tui::anim::frame_at(set, tick)),
                        loud,
                    ),
                    Span::styled(format!("{:<15}", set.name), Theme::dim()),
                    Span::styled(format!("{shown}{rest}"), Theme::meta()),
                ]),
                MenuAction::None,
            ));
        }
    }

    fn build_test_art_rows(&mut self) {
        for (idx, (name, lines)) in crate::tui::art::variants().into_iter().enumerate() {
            self.menu_rows.push(row(
                Line::from(vec![Span::styled(
                    format!(" {:02} · {name}", idx + 1),
                    Theme::dim(),
                )]),
                MenuAction::None,
            ));
            for line in lines {
                self.menu_rows.push(row(line, MenuAction::None));
            }
            self.menu_rows.push(row(
                Line::from(vec![Span::styled(String::new(), Theme::base())]),
                MenuAction::None,
            ));
        }
    }

    fn build_test_colors_rows(&mut self) {
        for (name, style) in crate::tui::theme::Theme::palette() {
            self.menu_rows.push(row(
                Line::from(vec![
                    Span::styled(format!(" {name:<14}"), Theme::dim()),
                    Span::styled("AaBbCc 0123".to_string(), style),
                ]),
                MenuAction::None,
            ));
        }
    }

    fn build_test_md_rows(&mut self) {
        for line in crate::tui::markdown::render(
            crate::tui::markdown::SHOWCASE,
            self.md_built_w.max(20),
            &self.hl,
        ) {
            self.menu_rows.push(row(line, MenuAction::None));
        }
    }

    fn build_context_view_rows(&mut self) {
        let md = self.context_view_markdown();
        let w = self.md_built_w.max(20);
        for line in crate::tui::markdown::render(&md, w, &self.hl) {
            self.menu_rows.push(row(line, MenuAction::None));
        }
    }

    fn build_ask_user_rows(&mut self, questions: &[crate::agent::loop_task::AskQuestion]) {
        for (q_idx, q) in questions.iter().enumerate() {
            let is_focused = q_idx == self.ask_focus;
            let header_style = if is_focused {
                Theme::accent_bold()
            } else {
                Theme::dim()
            };
            if !q.header.is_empty() {
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(format!(" {} ", q.header), header_style)]),
                    MenuAction::None,
                ));
            }
            self.menu_rows.push(row(
                Line::from(vec![Span::styled(format!(" {}", q.question), Theme::FG())]),
                MenuAction::None,
            ));
            for (o_idx, opt) in q.options.iter().enumerate() {
                let is_recommended = opt.recommended;
                let label = if is_recommended {
                    format!("{} (Recommended)", opt.label)
                } else {
                    opt.label.clone()
                };
                let picked = self
                    .ask_picked
                    .get(q_idx)
                    .and_then(|v| v.get(o_idx).copied())
                    .unwrap_or(false);
                let checked = if q.multiple {
                    if picked { " [x] " } else { " [ ] " }
                } else if picked {
                    " ● "
                } else {
                    " ○ "
                };
                let mut spans = vec![
                    Span::styled(format!("{checked}{}. ", o_idx + 1), Theme::marker()),
                    Span::styled(
                        label,
                        if is_recommended {
                            Theme::accent()
                        } else {
                            Theme::base()
                        },
                    ),
                ];
                if let Some(d) = &opt.description {
                    spans.push(Span::styled(format!(" — {d}"), Theme::dim()));
                }
                if is_recommended {
                    spans.push(Span::styled(" (Recommended)".to_string(), Theme::accent()));
                }
                let action = if q.multiple {
                    MenuAction::AskToggle {
                        q: q_idx,
                        idx: o_idx,
                    }
                } else {
                    MenuAction::AskSelect {
                        q: q_idx,
                        idx: o_idx,
                    }
                };
                self.menu_rows.push(row(Line::from(spans), action));
            }
            if q.allow_free {
                let custom_text = self
                    .ask_custom
                    .get(q_idx)
                    .map(|s| s.as_str())
                    .unwrap_or("");
                let is_custom_focused = self.ask_custom_focus == Some(q_idx);
                if is_custom_focused {
                    self.menu_rows.push(row(
                        Line::from(vec![Span::styled(
                            format!(
                                " ✎ {}",
                                if custom_text.is_empty() {
                                    "Type your answer…"
                                } else {
                                    custom_text
                                }
                            ),
                            Theme::accent(),
                        )]),
                        MenuAction::AskCustom { q: q_idx },
                    ));
                } else {
                    let display = if custom_text.is_empty() {
                        " ✎ Type your own answer…".to_string()
                    } else {
                        format!(" ✎ {}", custom_text)
                    };
                    self.menu_rows.push(row(
                        Line::from(vec![Span::styled(display, Theme::FG())]),
                        MenuAction::AskCustom { q: q_idx },
                    ));
                }
            }
            if q_idx + 1 < questions.len() {
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(" ──".to_string(), Theme::dim())]),
                    MenuAction::None,
                ));
            }
        }
        self.menu_rows.push(row(
            Line::from(vec![Span::styled(
                " confirm".to_string(),
                Theme::ACCENT_SOFT(),
            )]),
            MenuAction::AskConfirm,
        ));
        self.menu_footer_text =
            Some("enter: confirm · click: select · tab: next question · esc: skip".into());
    }

    fn build_approval_rows(&mut self, command: String, reason: &str) {
        self.menu_rows.push(row(
            Line::from(vec![
                Span::styled(command, Theme::WARN()),
                Span::styled(format!(" ({reason})"), Theme::dim()),
            ]),
            MenuAction::None,
        ));
        self.menu_rows.push(row(Line::default(), MenuAction::None));
        self.menu_rows.push(row(
            Line::from(vec![Span::styled("deny".to_string(), Theme::ERR())]),
            MenuAction::DecideApproval(ApprovalDecision::Deny),
        ));
        self.menu_rows.push(row(
            Line::from(vec![Span::styled("run once".to_string(), Theme::base())]),
            MenuAction::DecideApproval(ApprovalDecision::RunOnce),
        ));
        self.menu_rows.push(row(
            Line::from(vec![Span::styled(
                "always allow this session".to_string(),
                Theme::base(),
            )]),
            MenuAction::DecideApproval(ApprovalDecision::AlwaysSession),
        ));
        self.menu_footer_text = Some("enter: confirm · esc: deny".into());
    }

    fn build_plan_rows(&mut self) {
        if let Some(plan) = self.session_plan() {
            self.menu_rows.extend(plan_rows(&plan));
        } else if !self.todos.is_empty() {
            for (i, item) in self.todos.iter().enumerate() {
                self.menu_rows.push(row(
                    Line::from(vec![
                        Span::styled(format!("[ ] {}  ", i + 1), Theme::dim()),
                        Span::styled(item.clone(), Theme::base()),
                    ]),
                    MenuAction::None,
                ));
            }
        } else {
            self.menu_rows.push(row(
                Line::from(vec![Span::styled("no active plan", Theme::dim())]),
                MenuAction::None,
            ));
        }
    }

    pub(super) fn diagnostics_summary(&self) -> String {
        let log_path = crate::config::data_dir()
            .map(|d| d.join("debug.log").display().to_string())
            .unwrap_or_else(|_| "debug.log".into());
        let cfg_dir = crate::config::config_path()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        format!(
            "session: {}\nmodel: {} ({})\nprovider: {}\ncontext: {} / {}\nproject: {}\nlog: {}\nconfig: {}",
            self.session.id,
            self.session.model_key,
            self.model_cfg.id,
            self.model_cfg.provider,
            self.session.context_tokens_used(),
            self.session.context_limit,
            self.project_root.display(),
            log_path,
            cfg_dir,
        )
    }

    pub(super) fn context_view_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("# System\n\n");
        for part in &self.stable_parts {
            if !part.text.trim().is_empty() {
                out.push_str(part.text.trim());
                out.push_str("\n\n");
            }
        }
        if !self.session_environment.trim().is_empty() {
            out.push_str(self.session_environment.trim());
            out.push_str("\n\n");
        } else {
            let env = crate::prompts::env::session_block(&self.project_root);
            if !env.trim().is_empty() {
                out.push_str(env.trim());
                out.push_str("\n\n");
            }
        }

        out.push_str("# Host Block\n\n");
        out.push_str(&format!(
            "Mode: {}\n\n",
            match self.mode {
                Mode::Plan => "plan",
                Mode::Act => "act",
            }
        ));
        let facts = crate::prompts::env::turn_facts(&self.project_root);
        if !facts.trim().is_empty() {
            out.push_str(facts.trim());
            out.push_str("\n\n");
        }
        if let Some(block) = crate::prompts::plan_status_block(
            &self.project_root,
            Some(&self.session.id.to_string()),
        ) {
            out.push_str(&block);
            out.push_str("\n\n");
        }

        out.push_str(&format!("# Messages ({})\n\n", self.session.messages.len()));
        if let Some(summary) = &self.session.summary {
            out.push_str("## Compaction Summary\n\n");
            out.push_str(summary.trim());
            out.push_str("\n\n");
        }
        if self.session.messages.is_empty() {
            out.push_str("_(no messages in session yet)_\n\n");
        } else {
            for (i, msg) in self.session.messages.iter().enumerate() {
                let role = match msg.role {
                    Role::System => "system",
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::Tool => "tool",
                };
                out.push_str(&format!("## [{}] {}\n\n", i + 1, role));
                if !msg.thinking.trim().is_empty() {
                    out.push_str(&format!("> **thinking:** {}\n\n", msg.thinking.trim()));
                }
                if !msg.content.trim().is_empty() {
                    out.push_str(msg.content.trim());
                    out.push_str("\n\n");
                }
                if !msg.tool_calls.is_empty() {
                    for call in &msg.tool_calls {
                        out.push_str(&format!("- `{}` `{}`\n", call.name, call.args));
                    }
                    out.push('\n');
                }
            }
        }

        let specs = crate::agent::tools::tool_specs(matches!(self.mode, Mode::Plan));
        out.push_str(&format!("# Tools ({})\n\n", specs.len()));
        for spec in specs {
            let first_line = spec.description.lines().next().unwrap_or("");
            out.push_str(&format!("- **{}** — {}\n", spec.name, first_line));
        }
        out
    }
}

fn plan_rows(plan: &crate::plan::Plan) -> Vec<MenuRow> {
    let mut rows: Vec<MenuRow> = Vec::new();
    const WRAP: usize = 72;
    let push_wrapped = |rows: &mut Vec<MenuRow>,
                        prefix: &str,
                        text: &str,
                        prefix_style: Style,
                        text_style: Style| {
        let prefix_w = unicode_width::UnicodeWidthStr::width(prefix);
        let avail = WRAP.saturating_sub(prefix_w).max(20);
        let mut line = String::new();
        let mut first = true;
        for word in text.split_whitespace() {
            let w = unicode_width::UnicodeWidthStr::width(word);
            let need = if line.is_empty() { w } else { 1 + w };
            if unicode_width::UnicodeWidthStr::width(line.as_str()) + need > avail {
                let p = if first { prefix } else { &" ".repeat(prefix_w) };
                rows.push(row(
                    Line::from(vec![
                        Span::styled(p.to_string(), prefix_style),
                        Span::styled(line.clone(), text_style),
                    ]),
                    MenuAction::None,
                ));
                line.clear();
                first = false;
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        if !line.is_empty() || first {
            let p = if first { prefix } else { &" ".repeat(prefix_w) };
            rows.push(row(
                Line::from(vec![
                    Span::styled(p.to_string(), prefix_style),
                    Span::styled(line, text_style),
                ]),
                MenuAction::None,
            ));
        }
    };
    rows.push(row(
        Line::from(vec![
            Span::styled("plan         ", Theme::dim()),
            Span::styled(plan.id.clone(), Theme::dim()),
        ]),
        MenuAction::None,
    ));
    push_wrapped(
        &mut rows,
        "goal         ",
        &plan.goal.text,
        Theme::dim(),
        Theme::base(),
    );
    if !plan.constraints.is_empty() {
        push_wrapped(
            &mut rows,
            "constraints  ",
            &plan.constraints.join(" · "),
            Theme::dim(),
            Theme::dim(),
        );
    }
    if !plan.criteria.is_empty() {
        push_wrapped(
            &mut rows,
            "criteria     ",
            &plan.criteria.join(" · "),
            Theme::dim(),
            Theme::base(),
        );
    }
    if !plan.steps.is_empty() {
        rows.push(row(Line::default(), MenuAction::None));
    }
    for s in &plan.steps {
        let (marker, style) = match s.status {
            plan::StepStatus::Done => ("[x]", Theme::ok()),
            plan::StepStatus::InProgress => ("[>]", Theme::brand_ink()),
            plan::StepStatus::Blocked => ("[!]", Theme::err()),
            plan::StepStatus::Cancelled => ("[-]", Theme::dim()),
            plan::StepStatus::Pending | plan::StepStatus::Reopened => ("[ ]", Theme::dim()),
        };
        let prefix = format!("{marker} {:<2} ", s.id);
        let mut title = s.title.clone();
        if s.stale_goal {
            title.push_str("  [stale goal]");
        }
        push_wrapped(&mut rows, &prefix, &title, style, style);
        if let Some(reason) = &s.reason {
            push_wrapped(&mut rows, "       reason: ", reason, Theme::dim(), Theme::dim());
        }
        if let Some(summary) = &s.summary {
            push_wrapped(&mut rows, "       ", summary, Theme::dim(), Theme::dim());
        }
    }
    rows
}

fn session_row(s: &SessionHeader, is_current: bool) -> MenuRow {
    let style = if is_current {
        Theme::brand_ink()
    } else {
        Style::new().fg(Theme::FG())
    };
    let tail = format!(
        " ({} · {} · {})",
        fmt_time(s.last_activity()),
        s.model_key,
        fmt_k(s.context_tokens)
    );
    (
        Line::from(vec![
            Span::styled(s.title.clone(), style),
            Span::styled(tail, Theme::dim()),
        ]),
        MenuAction::OpenSession(s.id.to_string()),
    )
}

fn day_section(last: chrono::DateTime<chrono::Utc>) -> String {
    let day = last.with_timezone(&chrono::Local).date_naive();
    let today = chrono::Local::now().date_naive();
    if day == today {
        "Today".to_string()
    } else if day == today - chrono::Duration::days(1) {
        "Yesterday".to_string()
    } else {
        day.format("%d.%m").to_string()
    }
}

fn day_section_style() -> Style {
    crate::tui::art::h2_style()
}
