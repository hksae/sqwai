#![allow(unused_imports)]
use super::*;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use tui_textarea::TextArea;

use crate::agent::loop_task::AgentEvent;
use crate::config::{Config, EffortLevel, ModelConfig, WireFormat};
use crate::providers::{self, ChatRequest, Message as PMessage, Role, SharedProvider};
use crate::session::Session;
use crate::tui::markdown::{Highlighter, render, wrap_tagged};
use crate::tui::theme::Theme;

pub(super) const COMMANDS: &[&str] = &[
    "/compact",
    "/config",
    "/context",
    "/context-view",
    "/debug",
    "/effort",
    "/exit",
    "/export",
    "/help",
    "/init",
    "/models",
    "/new",
    "/plan",
    "/providers",
    "/redo",
    "/sessions",
    "/skill",
    "/skills",
    "/test",
    "/undo",
];

/// Second-level completions: (command, subcommands), in the same order as
/// the dispatch arms. Commands absent here keep level-1-only behavior
/// (the popup hides after the first space).
pub(super) const SUBCOMMANDS: &[(&str, &[&str])] = &[
    ("/plan", &["delete"]),
    (
        "/test",
        &[
            "anim", "approval", "art", "ask", "churn", "colors", "md", "plan",
        ],
    ),
    ("/undo", &["list", "step", "turn"]),
];

/// Subcommand list for a top-level command, if it has second-level completions.
pub(super) fn subcommands_of(cmd: &str) -> Option<&'static [&'static str]> {
    SUBCOMMANDS
        .iter()
        .find(|(c, _)| *c == cmd)
        .map(|(_, subs)| *subs)
}

pub(super) const POPUP_MAX_ROWS: usize = 8;

/// Human-readable context size for menu rows: 1048576 -> "1m", 262144 -> "256k".
pub(super) fn fmt_ctx(n: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    const KIB: u64 = 1024;
    if n >= MIB && n.is_multiple_of(MIB) {
        return format!("{}m", n / MIB);
    }
    if n >= 1_000_000 {
        if n.is_multiple_of(1_000_000) {
            return format!("{}m", n / 1_000_000);
        }
        return format!("{}m", trim_num(n as f64 / 1_000_000.0, 2));
    }
    if n >= 1_000 && n.is_multiple_of(1_000) {
        return format!("{}k", n / 1_000);
    }
    if n >= KIB && n.is_multiple_of(KIB) {
        return format!("{}k", n / KIB);
    }
    if n >= 1_000 {
        return format!("{}k", trim_num(n as f64 / 1_000.0, 1));
    }
    n.to_string()
}

fn trim_num(v: f64, prec: usize) -> String {
    let s = format!("{v:.prec$}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ModelField {
    Key,
    RequestId,
    Context,
}

impl ModelField {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Key => "key",
            Self::RequestId => "request id",
            Self::Context => "context",
        }
    }

    pub(super) fn row_index(self) -> usize {
        match self {
            Self::Key => 0,
            Self::RequestId => 1,
            Self::Context => 2,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct ModelDraft {
    pub key: String,
    pub id: String,
    pub context: u64,
    pub effort: EffortLevel,
}

impl Default for ModelDraft {
    fn default() -> Self {
        Self {
            key: String::new(),
            id: String::new(),
            context: 128_000,
            effort: EffortLevel::Medium,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ProviderField {
    Name,
    BaseUrl,
    ApiKey,
    KeyEnvVar,
}

impl ProviderField {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::BaseUrl => "base url",
            Self::ApiKey => "api key",
            Self::KeyEnvVar => "key env var",
        }
    }

    pub(super) fn row_index(self, is_builtin: bool) -> usize {
        if is_builtin {
            match self {
                Self::ApiKey => 0,
                Self::KeyEnvVar => 1,
                _ => 0,
            }
        } else {
            match self {
                Self::Name => 0,
                Self::BaseUrl => 2,
                Self::ApiKey => 3,
                Self::KeyEnvVar => 4,
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct ProviderDraft {
    pub name: String,
    pub format: WireFormat,
    pub base_url: String,
    pub api_key: String,
    pub api_key_env: String,
}

impl Default for ProviderDraft {
    fn default() -> Self {
        Self {
            name: String::new(),
            format: WireFormat::Openai,
            base_url: String::new(),
            api_key: String::new(),
            api_key_env: String::new(),
        }
    }
}

#[derive(Clone)]
#[allow(clippy::large_enum_variant)]
pub(super) enum Menu {
    /// /config: unified searchable config list
    Settings,
    Providers,
    Models {
        provider: String,
    },
    Sessions,
    /// /debug: runtime toggles and diagnostics
    #[allow(dead_code)]
    Debug,
    /// /help: help sections
    Help,
    /// /help -> controls: key combo reference
    #[allow(dead_code)]
    Controls,
    /// single-field form editing one numeric host setting
    EditScalar(ScalarSetting),
    /// picker for `[undo].shadow`
    UndoShadow,
    /// `/undo list`: interactive checkpoint & redo history picker
    UndoList,
    /// single-field form to rename a session
    EditSessionTitle {
        id: String,
    },
    EditProvider {
        name: Option<String>,
    },
    EditProviderField {
        name: Option<String>,
        field: ProviderField,
    },
    ProviderFormat {
        name: Option<String>,
    },
    EditModel {
        provider: String,
        key: Option<String>,
    },
    EditModelField {
        provider: String,
        key: Option<String>,
        field: ModelField,
    },
    ConfirmDelete {
        label: String,
        action: MenuAction,
    },
    /// effort picker scoped to one model (`None` = the session model).
    Effort {
        model: Option<String>,
    },
    /// /context: the context window's composition
    Context,
    /// /context-view: full assembled request context rendered as Markdown
    ContextView,
    /// /test anim: every single-character frame set, spinning
    TestAnim,
    /// /test art: numbered static logo variants
    TestArt,
    /// /test colors: every Theme color in one list
    TestColors,
    /// /test md: the markdown showcase, rendered live
    TestMd,
    /// legacy modal ask_user path
    #[allow(dead_code)]
    AskUser {
        id: u64,
        questions: Vec<crate::agent::loop_task::AskQuestion>,
    },
    /// a dangerous command needs explicit approval
    Approval {
        id: u64,
        command: String,
        reason: String,
    },
    /// free-text answer for an open ask_user — legacy
    #[allow(dead_code)]
    AskFree {
        id: u64,
    },
    /// full plan overview, opened with /plan, Ctrl+L or Ctrl+T
    Plan,
    /// /skills: interactive skill catalog & session pin toggle
    Skills,
    /// /skills -> ctrl+o: rendered Markdown view of a single skill
    SkillView {
        name: String,
    },
}

#[derive(Clone)]
pub(super) enum MenuAction {
    None,
    Back,
    OpenModels(String),
    FocusSearch,
    EditProvider(String),
    OpenProviderField(ProviderField),
    OpenProviderFormat,
    SetProviderFormat(WireFormat),
    DeleteProvider(String),
    CheckProvider(String),
    AddModel(String),
    EditModel(String, String),
    OpenModelField(ModelField),
    OpenModelEffort(Option<String>),
    DeleteModel(String, String),
    UseModel(String),
    OpenSession(String),
    RenameSession(String),
    PinSession(String),
    DeleteSession(String),
    ToggleTypewriter,
    ToggleHttpLog,
    ToggleOpenAttachments,
    ToggleMouse,
    ToggleExperimentalTest,
    TogglePerfLog,
    ClearDebugLog,
    CopyDiagnostics,
    UpdateCatalog,
    Confirm(Box<MenuAction>),
    DeletePlan,
    SetEffort(EffortLevel),
    AskSelect {
        q: usize,
        idx: usize,
    },
    AskToggle {
        q: usize,
        idx: usize,
    },
    AskConfirm,
    AskCustom {
        q: usize,
    },
    #[allow(dead_code)]
    AskNext,
    #[allow(dead_code)]
    AskPrev,
    DecideApproval(crate::agent::loop_task::ApprovalDecision),
    #[allow(dead_code)]
    OpenControls,
    #[allow(dead_code)]
    CycleUndoShadow,
    OpenUndoShadow,
    SetUndoShadow(crate::config::ShadowStore),
    UndoCount(usize),
    RedoCount(usize),
    ToggleSkillsAutoLoad,
    ToggleSkill(String),
    ViewSkill(String),
    ClearPinnedSkills,
    EditScalar(ScalarSetting),
}

/// One numeric host setting editable from /settings. Enums (effort, undo
/// shadow) are cycle rows, not scalars.
#[derive(Clone, Copy)]
pub(super) enum ScalarSetting {
    BashTimeoutSecs,
    BashOutputBytes,
    BashBackgroundJobs,
    ReadWindow,
    ReadMaxFileBytes,
    OutlineMaxFileBytes,
    WebResults,
    WebTimeoutSecs,
    WebFetchBytes,
    LimitMaxToolCalls,
    LimitMaxTokens,
    LimitMaxParallelTools,
    PlanMaxSteps,
    PlanNudgeAfter,
    MemoryLoadBudgetRatio,
    MemoryMaxTokens,
    MemoryMaxProposals,
    CompactionThreshold,
    CompactionKeepTurns,
    CompactionAnchorRatio,
    UndoKeepPerSession,
    UndoBlobGraceSecs,
    UndoShadowMaxBytes,
}

/// Parse `raw` into `slot`; on failure reset to `default`. Returns a status line.
fn apply_num<T>(label: &str, raw: &str, slot: &mut T, default: T) -> String
where
    T: std::str::FromStr + std::fmt::Display + Clone,
{
    match raw.trim().parse::<T>() {
        Ok(v) => {
            *slot = v.clone();
            format!("{label} = {v}")
        }
        Err(_) => {
            *slot = default.clone();
            format!("invalid number, reset to default ({default})")
        }
    }
}

impl ScalarSetting {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::BashTimeoutSecs => "bash timeout secs",
            Self::BashOutputBytes => "bash output bytes",
            Self::BashBackgroundJobs => "bash background jobs",
            Self::ReadWindow => "read window lines",
            Self::ReadMaxFileBytes => "read max file bytes",
            Self::OutlineMaxFileBytes => "outline max file bytes",
            Self::WebResults => "web results",
            Self::WebTimeoutSecs => "web timeout secs",
            Self::WebFetchBytes => "web fetch bytes",
            Self::LimitMaxToolCalls => "max tool calls per turn",
            Self::LimitMaxTokens => "max tokens per turn",
            Self::LimitMaxParallelTools => "max parallel tools",
            Self::PlanMaxSteps => "max steps",
            Self::PlanNudgeAfter => "nudge after",
            Self::MemoryLoadBudgetRatio => "load budget ratio",
            Self::MemoryMaxTokens => "max tokens",
            Self::MemoryMaxProposals => "max proposals",
            Self::CompactionThreshold => "threshold",
            Self::CompactionKeepTurns => "keep turns",
            Self::CompactionAnchorRatio => "anchor ratio",
            Self::UndoKeepPerSession => "keep per session",
            Self::UndoBlobGraceSecs => "blob grace secs",
            Self::UndoShadowMaxBytes => "shadow max bytes",
        }
    }

    pub(super) fn current(self, cfg: &Config) -> String {
        match self {
            Self::BashTimeoutSecs => cfg.tools.bash.timeout_secs.to_string(),
            Self::BashOutputBytes => cfg.tools.bash.output_bytes.to_string(),
            Self::BashBackgroundJobs => cfg.tools.bash.background_jobs.to_string(),
            Self::ReadWindow => cfg.tools.read.window.to_string(),
            Self::ReadMaxFileBytes => cfg.tools.read.max_file_bytes.to_string(),
            Self::OutlineMaxFileBytes => cfg.tools.outline.max_file_bytes.to_string(),
            Self::WebResults => cfg.tools.web.results.to_string(),
            Self::WebTimeoutSecs => cfg.tools.web.timeout_secs.to_string(),
            Self::WebFetchBytes => cfg.tools.web.fetch_bytes.to_string(),
            Self::LimitMaxToolCalls => cfg.limits.max_tool_calls_per_turn.to_string(),
            Self::LimitMaxTokens => cfg.limits.max_tokens_per_turn.to_string(),
            Self::LimitMaxParallelTools => cfg.limits.max_parallel_tools.to_string(),
            Self::PlanMaxSteps => cfg.plan.max_steps.to_string(),
            Self::PlanNudgeAfter => cfg.plan.nudge_after.to_string(),
            Self::MemoryLoadBudgetRatio => cfg.memory.load_budget_ratio.to_string(),
            Self::MemoryMaxTokens => cfg.memory.max_tokens.to_string(),
            Self::MemoryMaxProposals => cfg.memory.max_writes_per_turn.to_string(),
            Self::CompactionThreshold => cfg.compaction.threshold.to_string(),
            Self::CompactionKeepTurns => cfg.compaction.keep_turns.to_string(),
            Self::CompactionAnchorRatio => cfg.compaction.anchor_ratio.to_string(),
            Self::UndoKeepPerSession => cfg.undo.keep_per_session.to_string(),
            Self::UndoBlobGraceSecs => cfg.undo.blob_grace_secs.to_string(),
            Self::UndoShadowMaxBytes => cfg.undo.shadow_max_bytes.to_string(),
        }
    }

    pub(super) fn apply(self, cfg: &mut Config, raw: &str) -> String {
        let label = self.label();
        let def = Config::default();
        match self {
            Self::BashTimeoutSecs => apply_num(
                label,
                raw,
                &mut cfg.tools.bash.timeout_secs,
                def.tools.bash.timeout_secs,
            ),
            Self::BashOutputBytes => apply_num(
                label,
                raw,
                &mut cfg.tools.bash.output_bytes,
                def.tools.bash.output_bytes,
            ),
            Self::BashBackgroundJobs => apply_num(
                label,
                raw,
                &mut cfg.tools.bash.background_jobs,
                def.tools.bash.background_jobs,
            ),
            Self::ReadWindow => apply_num(
                label,
                raw,
                &mut cfg.tools.read.window,
                def.tools.read.window,
            ),
            Self::ReadMaxFileBytes => apply_num(
                label,
                raw,
                &mut cfg.tools.read.max_file_bytes,
                def.tools.read.max_file_bytes,
            ),
            Self::OutlineMaxFileBytes => apply_num(
                label,
                raw,
                &mut cfg.tools.outline.max_file_bytes,
                def.tools.outline.max_file_bytes,
            ),
            Self::WebResults => apply_num(
                label,
                raw,
                &mut cfg.tools.web.results,
                def.tools.web.results,
            ),
            Self::WebTimeoutSecs => apply_num(
                label,
                raw,
                &mut cfg.tools.web.timeout_secs,
                def.tools.web.timeout_secs,
            ),
            Self::WebFetchBytes => apply_num(
                label,
                raw,
                &mut cfg.tools.web.fetch_bytes,
                def.tools.web.fetch_bytes,
            ),
            Self::LimitMaxToolCalls => apply_num(
                label,
                raw,
                &mut cfg.limits.max_tool_calls_per_turn,
                def.limits.max_tool_calls_per_turn,
            ),
            Self::LimitMaxTokens => apply_num(
                label,
                raw,
                &mut cfg.limits.max_tokens_per_turn,
                def.limits.max_tokens_per_turn,
            ),
            Self::LimitMaxParallelTools => apply_num(
                label,
                raw,
                &mut cfg.limits.max_parallel_tools,
                def.limits.max_parallel_tools,
            ),
            Self::PlanMaxSteps => {
                apply_num(label, raw, &mut cfg.plan.max_steps, def.plan.max_steps)
            }
            Self::PlanNudgeAfter => {
                apply_num(label, raw, &mut cfg.plan.nudge_after, def.plan.nudge_after)
            }
            Self::MemoryLoadBudgetRatio => apply_num(
                label,
                raw,
                &mut cfg.memory.load_budget_ratio,
                def.memory.load_budget_ratio,
            ),
            Self::MemoryMaxTokens => apply_num(
                label,
                raw,
                &mut cfg.memory.max_tokens,
                def.memory.max_tokens,
            ),
            Self::MemoryMaxProposals => apply_num(
                label,
                raw,
                &mut cfg.memory.max_writes_per_turn,
                def.memory.max_writes_per_turn,
            ),
            Self::CompactionThreshold => apply_num(
                label,
                raw,
                &mut cfg.compaction.threshold,
                def.compaction.threshold,
            ),
            Self::CompactionKeepTurns => apply_num(
                label,
                raw,
                &mut cfg.compaction.keep_turns,
                def.compaction.keep_turns,
            ),
            Self::CompactionAnchorRatio => apply_num(
                label,
                raw,
                &mut cfg.compaction.anchor_ratio,
                def.compaction.anchor_ratio,
            ),
            Self::UndoKeepPerSession => apply_num(
                label,
                raw,
                &mut cfg.undo.keep_per_session,
                def.undo.keep_per_session,
            ),
            Self::UndoBlobGraceSecs => apply_num(
                label,
                raw,
                &mut cfg.undo.blob_grace_secs,
                def.undo.blob_grace_secs,
            ),
            Self::UndoShadowMaxBytes => apply_num(
                label,
                raw,
                &mut cfg.undo.shadow_max_bytes,
                def.undo.shadow_max_bytes,
            ),
        }
    }
}

impl App {
    pub(super) fn cur_menu(&self) -> Option<&Menu> {
        self.menu_stack.last()
    }

    pub(super) fn open_menu(&mut self, menu: Menu) {
        self.menu_stack.push(menu);
        self.menu_sel = 0;
        self.menu_open_mouse = self.last_mouse;
        crate::tui::event_log::log(
            "HOVER",
            format!(
                "open depth={} sel={} open_mouse={:?}",
                self.menu_stack.len(),
                self.menu_sel,
                self.menu_open_mouse,
            ),
        );
        self.form_hover_row = None;
        self.form_fields.clear();
        self.form_focus = 0;
        if let Some(Menu::AskUser { questions, .. }) = self.cur_menu().cloned() {
            self.ask_picked = questions
                .iter()
                .map(|q| vec![false; q.options.len()])
                .collect();
            self.ask_custom = vec![String::new(); questions.len()];
            self.ask_focus = 0;
            self.ask_custom_focus = None;
        }
        if matches!(
            self.cur_menu(),
            Some(Menu::Effort { .. })
                | Some(Menu::Context)
                | Some(Menu::ContextView)
                | Some(Menu::Settings)
                | Some(Menu::EditScalar(..))
                | Some(Menu::UndoShadow)
                | Some(Menu::UndoList)
                | Some(Menu::Providers)
                | Some(Menu::EditProvider { .. })
                | Some(Menu::EditProviderField { .. })
                | Some(Menu::ProviderFormat { .. })
                | Some(Menu::Models { .. })
                | Some(Menu::EditModel { .. })
                | Some(Menu::EditModelField { .. })
                | Some(Menu::Sessions)
                | Some(Menu::EditSessionTitle { .. })
                | Some(Menu::ConfirmDelete { .. })
                | Some(Menu::Approval { .. })
                | Some(Menu::Debug)
                | Some(Menu::Plan)
                | Some(Menu::Help)
                | Some(Menu::TestAnim)
                | Some(Menu::TestArt)
                | Some(Menu::TestColors)
                | Some(Menu::TestMd)
                | Some(Menu::Skills)
                | Some(Menu::SkillView { .. })
        ) {
            self.follow = true;
        }
        if matches!(self.cur_menu(), Some(Menu::Settings)) {
            self.config_search = Self::fresh_input(String::new());
            self.config_focus = false;
        }
        if matches!(self.cur_menu(), Some(Menu::Skills)) {
            self.refresh_skills();
            self.skill_search = Self::fresh_input(String::new());
            self.skill_focus = false;
        }
        if matches!(
            self.cur_menu(),
            Some(Menu::ContextView | Menu::TestMd | Menu::SkillView { .. })
        ) {
            self.md_built_w = 0;
        }
        if matches!(self.cur_menu(), Some(Menu::UndoShadow)) {
            let cur = self.cfg.undo.shadow;
            self.menu_sel = crate::config::ShadowStore::ALL
                .iter()
                .position(|s| *s == cur)
                .unwrap_or(0);
        }
        if matches!(self.cur_menu(), Some(Menu::EditProvider { name: None })) {
            self.provider_draft = ProviderDraft::default();
        }
        if let Some(Menu::ProviderFormat { name }) = self.cur_menu().cloned() {
            let fmt = name
                .as_ref()
                .and_then(|n| self.cfg.providers.get(n))
                .map(|pc| pc.format)
                .unwrap_or(self.provider_draft.format);
            self.menu_sel = WireFormat::ALL
                .iter()
                .position(|f| *f == fmt)
                .unwrap_or(0);
        }
        if let Some(Menu::EditModel { key: None, .. }) = self.cur_menu().cloned() {
            self.model_draft = ModelDraft::default();
        }
        let effort_scoped: Option<Option<String>> = match self.cur_menu() {
            Some(Menu::Effort { model }) => Some(model.clone()),
            _ => None,
        };
        if let Some(model) = effort_scoped {
            let want = if model.is_none() && self.parent_is_new_edit_model() {
                self.model_draft.effort
            } else {
                model
                    .as_ref()
                    .and_then(|k| self.cfg.models.get(k))
                    .map(|m| m.effort)
                    .unwrap_or(self.model_cfg.effort)
            };
            self.menu_sel = EffortLevel::SELECTABLE
                .iter()
                .position(|l| *l == want)
                .unwrap_or(0);
            self.effort_blend = None;
            self.effort_blend_sel = None;
        }
        #[cfg(not(test))]
        if matches!(self.cur_menu(), Some(Menu::Sessions)) {
            self.sessions = Session::list_visible_headers(40).unwrap_or_default();
        }
        if matches!(self.cur_menu(), Some(Menu::Sessions)) {
            self.session_search = Self::fresh_input(String::new());
            self.session_focus = false;
        }
        if matches!(self.cur_menu(), Some(Menu::Providers)) {
            self.prov_search = Self::fresh_input(String::new());
            self.prov_focus = false;
        }
        if matches!(self.cur_menu(), Some(Menu::Models { .. })) {
            self.model_search = Self::fresh_input(String::new());
            self.model_focus = false;
            if let Some(Menu::Models { provider }) = self.cur_menu().cloned() {
                self.provider_checks.remove(&provider);
            }
        }
        self.prefill_form();
        self.build_menu_rows();
        self.menu_skip_none(1);
        self.menu_scroll = 0;
        self.surf_top = 0;
        if matches!(self.cur_menu(), Some(Menu::Approval { .. })) {
            self.menu_sel = self
                .menu_rows
                .iter()
                .position(|(_, a)| matches!(a, MenuAction::DecideApproval(ApprovalDecision::Deny)))
                .unwrap_or(0);
            self.approval_opened_at = Some(Instant::now());
        }
        self.dirty = true;
    }

    pub(super) fn open_menu_replace(&mut self, menu: Menu) {
        self.menu_stack.pop();
        self.open_menu(menu);
    }

    pub(super) fn menu_back(&mut self) {
        match self.cur_menu() {
            Some(Menu::AskUser { .. }) => {
                self.ask_answer(String::new());
                return;
            }
            Some(Menu::Approval { .. }) => {
                self.approval_decide(ApprovalDecision::Deny);
                return;
            }
            _ => {}
        }
        let popped = self.menu_stack.pop();
        self.menu_sel = match (&popped, self.cur_menu()) {
            (Some(Menu::EditModelField { field, .. }), Some(Menu::EditModel { .. })) => {
                field.row_index()
            }
            (Some(Menu::Effort { .. }), Some(Menu::EditModel { .. })) => 3,
            (
                Some(Menu::EditProviderField { field, .. }),
                Some(Menu::EditProvider { name }),
            ) => {
                let is_builtin = name
                    .as_ref()
                    .is_some_and(|n| self.cfg.is_builtin_provider(n));
                field.row_index(is_builtin)
            }
            (Some(Menu::ProviderFormat { .. }), Some(Menu::EditProvider { .. })) => 1,
            _ => 0,
        };
        self.form_hover_row = None;
        self.form_fields.clear();
        self.form_focus = 0;
        self.prefill_form();
        self.restore_form_text();
        self.build_menu_rows();
        if matches!(self.cur_menu(), Some(Menu::Settings)) {
            let pos = match popped {
                Some(Menu::EditScalar(s)) => self.menu_rows.iter().position(|(_, a)| {
                    matches!(a, MenuAction::EditScalar(other) if other.label() == s.label())
                }),
                Some(Menu::UndoShadow) => self
                    .menu_rows
                    .iter()
                    .position(|(_, a)| matches!(a, MenuAction::OpenUndoShadow)),
                _ => None,
            };
            if let Some(idx) = pos {
                self.menu_sel = idx;
            }
        }
        if matches!(self.cur_menu(), Some(Menu::Skills)) {
            if let Some(Menu::SkillView { name }) = popped
                && let Some(idx) = self.menu_rows.iter().position(|(_, a)| {
                    matches!(a, MenuAction::ToggleSkill(other) if other.eq_ignore_ascii_case(&name))
                })
            {
                self.menu_sel = idx;
            } else {
                self.menu_skip_none(1);
            }
        }
        self.dirty = true;
    }

    pub(super) fn menu_home(&mut self) {
        self.menu_stack.clear();
        self.menu_sel = 0;
        self.form_hover_row = None;
        self.form_fields.clear();
        self.form_focus = 0;
        self.menu_rows.clear();
        self.dirty = true;
    }

    pub(super) fn menu_wheel(&mut self, delta: i32) {
        if self.is_form_menu() {
            return;
        }
        let n = self.menu_rows.len();
        if n == 0 {
            return;
        }
        let max = n.saturating_sub(self.menu_visible_rows.max(1));
        let next = if delta < 0 {
            self.menu_scroll
                .saturating_sub(delta.unsigned_abs() as usize)
        } else {
            (self.menu_scroll + delta as usize).min(max)
        };
        if next != self.menu_scroll {
            self.menu_scroll = next;
            self.dirty = true;
        }
    }

    fn menu_follow_sel(&mut self) {
        let n = self.menu_len();
        if n == 0 {
            return;
        }
        self.menu_sel = self.menu_sel.min(n - 1);
        if self.menu_sel >= self.menu_rows.len() {
            return;
        }
        let vis = self.menu_visible_rows.max(1);
        if self.menu_sel < self.menu_scroll {
            self.menu_scroll = self.menu_sel;
        } else if self.menu_sel >= self.menu_scroll + vis {
            self.menu_scroll = self.menu_sel + 1 - vis;
        }
    }

    fn menu_skips_none(&self) -> bool {
        !matches!(
            self.cur_menu(),
            Some(Menu::Context)
                | Some(Menu::ContextView)
                | Some(Menu::Plan)
                | Some(Menu::Help)
                | Some(Menu::TestAnim)
                | Some(Menu::TestArt)
                | Some(Menu::TestColors)
                | Some(Menu::TestMd)
        )
    }

    fn menu_skip_none(&mut self, dir: i32) {
        if !self.menu_skips_none() {
            return;
        }
        let n = self.menu_len();
        if n == 0 {
            return;
        }
        let step = if dir < 0 { n - 1 } else { 1 };
        for _ in 0..n {
            if !matches!(
                self.menu_row_at(self.menu_sel).map(|(_, a)| a),
                Some(MenuAction::None)
            ) {
                break;
            }
            self.menu_sel = (self.menu_sel + step) % n;
        }
    }

    pub(super) fn menu_is_dead(&self) -> bool {
        matches!(
            self.cur_menu(),
            Some(
                Menu::Context
                    | Menu::ContextView
                    | Menu::Plan
                    | Menu::Help
                    | Menu::TestAnim
                    | Menu::TestArt
                    | Menu::TestColors
                    | Menu::TestMd
                    | Menu::SkillView { .. }
            )
        )
    }

    pub(super) fn menu_nav(&mut self, dir: i32) {
        if matches!(
            self.cur_menu(),
            Some(
                Menu::ContextView
                    | Menu::Plan
                    | Menu::Help
                    | Menu::TestAnim
                    | Menu::TestArt
                    | Menu::TestColors
                    | Menu::TestMd
                    | Menu::SkillView { .. }
            )
        ) {
            let step = if dir.abs() > 1 { 10 } else { 1 };
            if dir < 0 {
                self.surf_top = self.surf_top.saturating_sub(step);
            } else {
                self.surf_top = self
                    .surf_top
                    .saturating_add(step)
                    .min(self.menu_rows.len().saturating_sub(1));
            }
            self.dirty = true;
            return;
        }
        if self.menu_is_dead() {
            return;
        }
        let is_form = self.is_form_menu();
        if is_form {
            let n = self.form_fields.len();
            if n > 0 {
                if let Some(FormField::Text { ta, .. }) = self.form_fields.get_mut(self.form_focus)
                {
                    ta.cancel_selection();
                }
                self.form_focus = if dir < 0 {
                    (self.form_focus + n - 1) % n
                } else {
                    (self.form_focus + 1) % n
                };
                if dir < 0 {
                    self.form_to_start();
                } else {
                    self.form_to_end();
                }
            }
        } else if dir.abs() > 1 {
            let n = self.menu_len();
            if n > 0 {
                let step = (self.menu_len() as i32 / 2).clamp(1, 10) as usize;
                self.menu_sel = if dir < 0 {
                    self.menu_sel.saturating_sub(step)
                } else {
                    (self.menu_sel + step).min(n - 1)
                };
                self.menu_skip_none(dir);
                self.menu_follow_sel();
            }
        } else {
            let n = self.menu_len();
            if n > 0 {
                self.menu_sel = if dir < 0 {
                    (self.menu_sel + n - 1) % n
                } else {
                    (self.menu_sel + 1) % n
                };
                self.menu_skip_none(dir);
                self.menu_follow_sel();
            }
        }
        self.surf_set_focus(false);
        self.dirty = true;
    }

    pub(super) fn menu_jump(&mut self, to_end: bool) {
        if self.is_form_menu() {
            self.menu_nav(if to_end { 1 } else { -1 });
            return;
        }
        if self.menu_len() > 0 {
            self.menu_sel = if to_end { self.menu_len() - 1 } else { 0 };
            self.menu_skip_none(if to_end { -1 } else { 1 });
            self.menu_follow_sel();
        }
        self.dirty = true;
    }

    pub(super) fn is_form_menu(&self) -> bool {
        matches!(
            self.cur_menu(),
            Some(
                Menu::EditProviderField { .. }
                    | Menu::EditModelField { .. }
                    | Menu::EditSessionTitle { .. }
                    | Menu::AskFree { .. }
                    | Menu::EditScalar(..)
            )
        )
    }

    pub(super) fn menu_hover(&mut self, row: u16) -> Option<usize> {
        if matches!(self.cur_menu(), Some(Menu::Effort { .. })) && !self.effort_hits.is_empty() {
            return Some(self.menu_sel);
        }
        if self.surface_open {
            let hit = self
                .surf_hits
                .iter()
                .find(|(y, _)| *y == row)
                .map(|(_, i)| *i);
            if let Some(i) = hit
                && i != self.menu_sel
            {
                self.menu_sel = i;
                self.dirty = true;
            }
            return hit;
        }
        if self.is_form_menu() {
            return None;
        }
        let r = self.menu_rect;
        let skip = 2 + usize::from(self.menu_table_header.is_some());
        if self.is_form_menu()
            || r.height == 0
            || row.saturating_sub(r.y) < skip as u16
            || row + 1 >= r.bottom()
        {
            return None;
        }
        let body = self.menu_rows.len();
        let foot = self.menu_sticky_footer.len();
        let shown = if self.menu_visible_rows == 0 {
            body.saturating_sub(self.menu_scroll)
        } else {
            self.menu_visible_rows
                .min(body.saturating_sub(self.menu_scroll))
        };
        let rel = (row - r.y) as usize - skip;
        let abs = if rel < shown {
            self.menu_scroll + rel
        } else if foot > 0 && rel == shown {
            return None;
        } else if foot > 0 {
            let f = rel - shown - 1;
            if f >= foot {
                return None;
            }
            body + f
        } else {
            return None;
        };
        if self.menu_skips_none()
            && matches!(
                self.menu_row_at(abs).map(|(_, a)| a),
                Some(MenuAction::None)
            )
        {
            return None;
        }
        if abs != self.menu_sel {
            self.menu_sel = abs;
            self.dirty = true;
        }
        Some(abs)
    }

    pub(super) fn parent_is_new_edit_model(&self) -> bool {
        self.menu_stack
            .len()
            .checked_sub(2)
            .and_then(|i| self.menu_stack.get(i))
            .is_some_and(|m| matches!(m, Menu::EditModel { key: None, .. }))
    }

    pub(super) fn effort_committed(&self) -> crate::config::EffortLevel {
        match self.cur_menu() {
            Some(Menu::Effort { model }) => {
                if model.is_none() && self.parent_is_new_edit_model() {
                    return self.model_draft.effort;
                }
                model
                    .as_ref()
                    .and_then(|k| self.cfg.models.get(k))
                    .map(|m| m.effort)
                    .unwrap_or(self.model_cfg.effort)
            }
            _ => self.model_cfg.effort,
        }
    }

    pub(super) fn effort_index_at(&self, row: u16, col: u16) -> Option<usize> {
        if !matches!(self.cur_menu(), Some(Menu::Effort { .. })) {
            return None;
        }
        self.effort_hits
            .iter()
            .find(|(r, _)| col >= r.x && col < r.right() && row >= r.y && row < r.bottom())
            .map(|(_, idx)| *idx)
    }

    pub(super) fn menu_len(&self) -> usize {
        self.menu_rows.len() + self.menu_sticky_footer.len()
    }

    pub(super) fn menu_row_at(&self, idx: usize) -> Option<&(Line<'static>, MenuAction)> {
        if idx < self.menu_rows.len() {
            self.menu_rows.get(idx)
        } else {
            self.menu_sticky_footer.get(idx - self.menu_rows.len())
        }
    }

    pub(super) fn surface_provider(&self) -> Option<String> {
        match self.cur_menu() {
            Some(Menu::Models { provider }) => Some(provider.clone()),
            Some(Menu::Providers) => match self.menu_row_at(self.menu_sel) {
                Some((_, MenuAction::OpenModels(p))) => Some(p.clone()),
                _ => None,
            },
            _ => None,
        }
    }

    pub(super) fn surface_model(&self) -> Option<String> {
        match self.menu_row_at(self.menu_sel) {
            Some((_, MenuAction::UseModel(k))) => Some(k.clone()),
            _ => None,
        }
    }

    pub(super) fn surf_field(&mut self) -> &mut TextArea<'static> {
        match self.cur_menu() {
            Some(Menu::Models { .. }) => &mut self.model_search,
            Some(Menu::Sessions) => &mut self.session_search,
            Some(Menu::Settings) => &mut self.config_search,
            Some(Menu::Skills) => &mut self.skill_search,
            _ => &mut self.prov_search,
        }
    }

    pub(super) fn surf_focused(&self) -> bool {
        match self.cur_menu() {
            Some(Menu::Models { .. }) => self.model_focus,
            Some(Menu::Sessions) => self.session_focus,
            Some(Menu::Settings) => self.config_focus,
            Some(Menu::Skills) => self.skill_focus,
            _ => self.prov_focus,
        }
    }

    pub(super) fn surf_set_focus(&mut self, on: bool) {
        match self.cur_menu() {
            Some(Menu::Models { .. }) => self.model_focus = on,
            Some(Menu::Sessions) => self.session_focus = on,
            Some(Menu::Settings) => self.config_focus = on,
            Some(Menu::Skills) => self.skill_focus = on,
            _ => self.prov_focus = on,
        }
    }

    pub(super) fn selected_skill_name(&self) -> Option<String> {
        match &self.menu_row_at(self.menu_sel)?.1 {
            MenuAction::ToggleSkill(name) => Some(name.clone()),
            _ => None,
        }
    }

    pub(super) fn surf_search_key(&mut self, k: crossterm::event::KeyEvent) {
        self.surf_field().input(k);
        if self.surf_field().lines().join("").is_empty() {
            self.surf_set_focus(false);
        }
        self.menu_scroll = 0;
        self.surf_top = 0;
        self.build_menu_rows();
        self.dirty = true;
    }

    pub(super) fn selected_session_id(&self) -> Option<String> {
        match &self.menu_row_at(self.menu_sel)?.1 {
            MenuAction::OpenSession(id) => Some(id.clone()),
            _ => None,
        }
    }

    pub(super) fn in_menu_rect(&self, row: u16, col: u16) -> bool {
        let r = self.menu_rect;
        r.width > 0 && col >= r.x && col < r.right() && row >= r.y && row < r.bottom()
    }

    pub(super) fn menu_click(&mut self, row: u16) {
        let Some(sel) = self.menu_hover(row) else {
            return;
        };
        if let Some(Menu::ConfirmDelete { .. }) = self.cur_menu() {
            let Some((_, action)) = self.menu_row_at(sel) else {
                return;
            };
            self.run_action(action.clone());
            return;
        }
        if matches!(self.cur_menu(), Some(Menu::Approval { .. })) {
            if let Some((_, MenuAction::DecideApproval(_))) = self.menu_row_at(sel) {
                self.menu_sel = sel;
                self.dirty = true;
            }
            return;
        }
        self.menu_sel = sel;
        self.menu_activate();
    }

    pub(super) fn run_confirm_action(&mut self) {
        let act = self
            .menu_rows
            .iter()
            .chain(self.menu_sticky_footer.iter())
            .find(|(_, a)| matches!(a, MenuAction::Confirm(_)))
            .map(|(_, a)| a.clone());
        if let Some(a) = act {
            self.run_action(a);
        }
    }

    pub(super) fn menu_activate(&mut self) {
        if self.is_form_menu() {
            self.form_save();
            return;
        }
        if matches!(
            self.cur_menu(),
            Some(Menu::Context)
                | Some(Menu::ContextView)
                | Some(Menu::Plan)
                | Some(Menu::Help)
                | Some(Menu::TestAnim)
                | Some(Menu::TestArt)
                | Some(Menu::TestColors)
                | Some(Menu::TestMd)
                | Some(Menu::SkillView { .. })
        ) {
            self.menu_back();
            return;
        }
        if let Some(Menu::AskUser { .. }) = self.cur_menu() {
            if let Some((_, action)) = self.menu_row_at(self.menu_sel) {
                self.run_action(action.clone());
            }
            return;
        }
        if let Some(Menu::Approval { .. }) = self.cur_menu() {
            if let Some(opened) = self.approval_opened_at
                && opened.elapsed() < std::time::Duration::from_millis(500)
            {
                return;
            }
            if let Some((_, MenuAction::DecideApproval(decision))) = self.menu_row_at(self.menu_sel)
            {
                self.run_action(MenuAction::DecideApproval(*decision));
            }
            return;
        }
        let Some((_, action)) = self.menu_row_at(self.menu_sel) else {
            return;
        };
        self.run_action(action.clone());
    }

    pub(super) fn ask_answer(&mut self, text: String) {
        let id = match self.cur_menu() {
            Some(Menu::AskUser { id, .. }) | Some(Menu::AskFree { id }) => *id,
            _ => return,
        };
        if let Some(agent) = &self.agent {
            let _ = agent.control.try_send(ControlMsg::AskAnswer { id, text });
        }
        self.ask_picked.clear();
        self.close_interaction();
    }

    pub(super) fn approval_select(&mut self, decision: ApprovalDecision) {
        if !matches!(self.cur_menu(), Some(Menu::Approval { .. })) {
            return;
        }
        if let Some(idx) = self
            .menu_rows
            .iter()
            .position(|(_, a)| matches!(a, MenuAction::DecideApproval(d) if *d == decision))
        {
            self.menu_sel = idx;
            self.dirty = true;
        }
    }

    pub(super) fn approval_decide(&mut self, decision: ApprovalDecision) {
        let id = match self.cur_menu() {
            Some(Menu::Approval { id, .. }) => *id,
            _ => return,
        };
        if let Some(agent) = &self.agent {
            let _ = agent
                .control
                .try_send(ControlMsg::ApprovalAnswer { id, decision });
        }
        self.close_interaction();
    }

    pub(super) fn close_interaction(&mut self) {
        self.menu_stack.pop();
        self.menu_sel = 0;
        self.form_fields.clear();
        self.form_focus = 0;
        self.prefill_form();
        self.build_menu_rows();
        self.dirty = true;
    }

    pub(super) fn menu_title(&self) -> String {
        match &self.cur_menu() {
            Some(Menu::Settings) => " Config ".into(),
            Some(Menu::UndoShadow) => " Config · undo shadow ".into(),
            Some(Menu::UndoList) => " Undo / Redo checkpoints ".into(),
            Some(Menu::Providers) => " Providers ".into(),
            Some(Menu::Models { provider }) => format!(" Models · {provider} "),
            Some(Menu::EditProvider { name }) => match name {
                Some(n) => format!(" Edit provider: {n} "),
                None => " New provider ".into(),
            },
            Some(Menu::EditProviderField { name, field }) => match name {
                Some(n) => format!(" Provider · {n} · {} ", field.label()),
                None => format!(" New provider · {} ", field.label()),
            },
            Some(Menu::ProviderFormat { name }) => match name {
                Some(n) => format!(" Provider · {n} · format "),
                None => " New provider · format ".into(),
            },
            Some(Menu::EditModel { provider, .. }) => {
                format!(" Model · {provider} ")
            }
            Some(Menu::EditModelField {
                provider, field, ..
            }) => {
                format!(" Model · {provider} · {} ", field.label())
            }
            Some(Menu::Sessions) => " Sessions ".into(),
            Some(Menu::Debug) => " Debug ".into(),
            Some(Menu::Help) => " Help ".into(),
            Some(Menu::Controls) => " Controls ".into(),
            Some(Menu::EditScalar(setting)) => format!(" Config · {} ", setting.label()),
            Some(Menu::EditSessionTitle { .. }) => " Rename session ".into(),
            Some(Menu::ConfirmDelete { .. }) => " Confirm ".into(),
            Some(Menu::Effort { model }) => match model {
                Some(key) => format!(" Effort · {key} "),
                None => " Effort ".into(),
            },
            Some(Menu::Context) => " Context ".into(),
            Some(Menu::ContextView) => " Context · view ".into(),
            Some(Menu::TestAnim) => " Anim ".into(),
            Some(Menu::TestArt) => " Art ".into(),
            Some(Menu::TestColors) => " Colors ".into(),
            Some(Menu::TestMd) => " Md ".into(),
            Some(Menu::AskUser { .. }) => " Ask ".into(),
            Some(Menu::Approval { .. }) => " Confirm command ".into(),
            Some(Menu::AskFree { .. }) => " Answer ".into(),
            Some(Menu::Plan) => match self.session_plan() {
                Some(plan) => {
                    let status = match plan.status {
                        plan::PlanStatus::Active => "active",
                        plan::PlanStatus::Completed => "completed",
                        plan::PlanStatus::Abandoned => "abandoned",
                    };
                    let c = plan.counts();
                    format!(" Plan · {status} ({}/{} done) ", c.done, plan.steps.len())
                }
                None => " Plan ".into(),
            },
            Some(Menu::Skills) => {
                let all = self.cached_skills();
                let active_count = all
                    .iter()
                    .filter(|s| {
                        s.always
                            || self
                                .active_skills
                                .iter()
                                .any(|a| a.name.eq_ignore_ascii_case(&s.name))
                    })
                    .count();
                if active_count > 0 {
                    format!(" Skills · {active_count} active / {} available ", all.len())
                } else {
                    format!(" Skills ({}) ", all.len())
                }
            }            Some(Menu::SkillView { name }) => format!(" Skill · {name} "),
            None => String::new(),
        }
    }
}

pub(super) fn table_budget(menu_rect_w: u16) -> usize {
    if menu_rect_w == 0 {
        return 72;
    }
    (menu_rect_w as usize).saturating_sub(6).max(30)
}
