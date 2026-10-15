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
    "/constraints",
    "/context",
    "/debug",
    "/effort",
    "/exit",
    "/export",
    "/goal",
    "/help",
    "/init",
    "/mode",
    "/models",
    "/new",
    "/plan",
    "/providers",
    "/sessions",
    "/skill",
    "/skills",
    "/test",
    "/undo",
    "/why",
];

/// Second-level completions: (command, subcommands), in the same order as
/// the dispatch arms. Commands absent here keep level-1-only behavior
/// (the popup hides after the first space).
pub(super) const SUBCOMMANDS: &[(&str, &[&str])] = &[
    (
        "/plan",
        &["history", "limit", "complete", "abandon", "delete"],
    ),
    ("/undo", &["step"]),
    ("/test", &["anim", "approval", "art", "churn", "colors", "md"]),
    ("/providers", &["add", "update"]),
    ("/constraints", &["add", "remove"]),
    ("/mode", &["plan", "act"]),
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
    pub provider: String,
    pub key: String,
    pub id: String,
    pub context: u64,
    pub effort: EffortLevel,
}

impl Default for ModelDraft {
    fn default() -> Self {
        Self {
            provider: String::new(),
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
#[allow(clippy::large_enum_variant)] // popup state is rebuilt per frame; form payloads are small in practice
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
    /// picker for `[compaction].summary`
    CompactionSummary,
    /// picker for `[undo].shadow`
    UndoShadow,
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
    /// Opened from the model form it writes back into that model;
    /// from anywhere else it acts on the session model.
    Effort {
        model: Option<String>,
    },
    /// /context: the context window's composition — one noise block per part
    Context,
    /// /test anim: every single-character frame set, spinning, for picking the
    /// live status marker
    TestAnim,
    /// /test art: numbered static logo variants for picking the empty-state mark
    TestArt,
    /// /test colors: every Theme color in one list, duplicates visible
    TestColors,
    /// /test md: the markdown showcase, rendered live
    TestMd,
    /// the model asked the user structured questions (ask_user)
    /// legacy modal path; live asks render inline in the chat instead
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
    /// free-text answer for an open ask_user (single-field form) — legacy, now inline
    #[allow(dead_code)]
    AskFree {
        id: u64,
    },
    /// the active plan's visible steps, opened with Ctrl+T
    Todo,
    /// full plan overview, opened with /plan
    Plan,
}

#[derive(Clone)]
pub(super) enum MenuAction {
    None,
    Back,
    OpenModels(String),
    OpenEffort,
    /// The search row of a surface: activating it (Enter or a click) hands the
    /// caret to the field. The row is reachable by the arrows like any other.
    FocusSearch,
    EditProvider(String),
    OpenProviderField(ProviderField),
    OpenProviderFormat,
    SetProviderFormat(WireFormat),
    DeleteProvider(String),
    /// check that the provider answers with the configured credentials
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
    ToggleExperimentalTest,
    TogglePerfLog,
    ToggleMode,
    OpenSessions,
    Confirm(Box<MenuAction>),
    DeletePlan,
    SetEffort(EffortLevel),
    /// ask_user: select one option (q, idx)
    AskSelect {
        q: usize,
        idx: usize,
    },
    /// ask_user multi: toggle an option by index (q, idx)
    AskToggle {
        q: usize,
        idx: usize,
    },
    /// ask_user: confirm all questions
    AskConfirm,
    /// ask_user: focus custom input for question q
    AskCustom {
        q: usize,
    },
    /// ask_user: switch focus to next/prev question
    #[allow(dead_code)]
    AskNext,
    #[allow(dead_code)]
    AskPrev,
    /// approval: commit the selected option (select with 1/2/3, click or arrows)
    DecideApproval(crate::agent::loop_task::ApprovalDecision),
    #[allow(dead_code)]
    OpenControls,
    CycleCompactionSummary,
    OpenCompactionSummary,
    SetCompactionSummary(crate::config::CompactionSummary),
    CycleUndoShadow,
    OpenUndoShadow,
    SetUndoShadow(crate::config::ShadowStore),
    ToggleSkillsAutoLoad,
    EditScalar(ScalarSetting),
}

/// One numeric host setting editable from /settings. Enums (effort,
/// compaction summary, undo shadow) are cycle rows, not scalars.
#[derive(Clone, Copy)]
pub(super) enum ScalarSetting {
    PlanBudgetRatio,
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
            Self::PlanBudgetRatio => "budget ratio",
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
            Self::PlanBudgetRatio => cfg.plan.budget_ratio.to_string(),
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
            Self::PlanBudgetRatio => apply_num(
                label,
                raw,
                &mut cfg.plan.budget_ratio,
                def.plan.budget_ratio,
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
        // A surface is the tail of the history: it only exists at the end of
        // it, so opening one brings the view back down to it.
        if matches!(
            self.cur_menu(),
            Some(Menu::Effort { .. })
                | Some(Menu::Context)
                | Some(Menu::Settings)
                | Some(Menu::EditScalar(..))
                | Some(Menu::CompactionSummary)
                | Some(Menu::UndoShadow)
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
        ) {
            self.follow = true;
        }
        if matches!(self.cur_menu(), Some(Menu::Settings)) {
            self.config_search = Self::fresh_input(String::new());
            self.config_focus = false;
        }
        if matches!(self.cur_menu(), Some(Menu::CompactionSummary)) {
            let cur = self.cfg.compaction.summary;
            self.menu_sel = crate::config::CompactionSummary::ALL
                .iter()
                .position(|s| s.as_str() == cur.as_str())
                .unwrap_or(0);
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
        if let Some(Menu::EditModel {
            provider,
            key: None,
        }) = self.cur_menu().cloned()
        {
            self.model_draft = ModelDraft {
                provider,
                ..ModelDraft::default()
            };
        }
        // effort slider opens on the target level, not on `off`
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
            // a fresh open is a first sighting: it must land on the level's own
            // color, not sweep from wherever an earlier popup left the slider
            self.effort_blend = None;
            self.effort_blend_sel = None;
        }
        // unit tests inject the cache directly and never hit the disk
        #[cfg(not(test))]
        if matches!(self.cur_menu(), Some(Menu::Sessions)) {
            // The menu needs metadata only; defer loading full message histories
            // until the user actually opens a session.
            self.sessions = Session::list_visible_headers(40).unwrap_or_default();
        }
        if matches!(self.cur_menu(), Some(Menu::Sessions)) {
            self.session_search = Self::fresh_input(String::new());
            self.session_focus = false;
        }
        if matches!(self.cur_menu(), Some(Menu::Providers)) {
            // every visit starts clean: a stale query would hide providers the
            // user came to look at, and the caret waits for a click
            self.prov_search = Self::fresh_input(String::new());
            self.prov_focus = false;
        }
        if matches!(self.cur_menu(), Some(Menu::Models { .. })) {
            self.model_search = Self::fresh_input(String::new());
            self.model_focus = false;
            // the probe's answer belongs to the visit that asked for it: a
            // stale `refused` standing in the hint column reads as a claim
            // about right now
            if let Some(Menu::Models { provider }) = self.cur_menu().cloned() {
                self.provider_checks.remove(&provider);
            }
        }
        self.prefill_form();
        // rows are ready immediately, not on the next frame
        self.build_menu_rows();
        // a menu that opens on an info row (section divider) starts on the
        // first actionable row instead of painting a dead band
        self.menu_skip_none(1);
        // a fresh menu opens at the top: the skip above may have pulled the
        // view after the selection for a window that does not exist yet
        self.menu_scroll = 0;
        self.surf_top = 0;
        if matches!(self.cur_menu(), Some(Menu::Approval { .. })) {
            // deny is preselected, and the clock starts for the Enter guard
            self.menu_sel = self
                .menu_rows
                .iter()
                .position(|(_, a)| matches!(a, MenuAction::DecideApproval(ApprovalDecision::Deny)))
                .unwrap_or(0);
            self.approval_opened_at = Some(Instant::now());
        }
        self.dirty = true;
    }

    /// replace the top of the stack (used after save/confirm flows)
    pub(super) fn open_menu_replace(&mut self, menu: Menu) {
        self.menu_stack.pop();
        self.open_menu(menu);
    }

    pub(super) fn menu_back(&mut self) {
        // interactions: esc cancels/blanks an ask, denies an approval
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
        // a form below the popped one needs its fields back
        self.prefill_form();
        // a submenu detour (form → Effort menu) must not eat unsaved
        // typing: prefill rebuilds from config, the stash restores drafts
        self.restore_form_text();
        self.build_menu_rows();
        if matches!(self.cur_menu(), Some(Menu::Settings)) {
            let pos = match popped {
                Some(Menu::EditScalar(s)) => self.menu_rows.iter().position(|(_, a)| {
                    matches!(a, MenuAction::EditScalar(other) if other.label() == s.label())
                }),
                Some(Menu::CompactionSummary) => self
                    .menu_rows
                    .iter()
                    .position(|(_, a)| matches!(a, MenuAction::OpenCompactionSummary)),
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

    /// Wheel over a list menu: scrolls the view only, the selection
    /// stays put — it may freely leave the visible window. Forms keep
    /// the wheel inert.
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

    /// Pull the view after the selection (keyboard nav): the selection
    /// must stay visible. The wheel never calls this — it scrolls free.
    /// Footer rows are always visible, so a selection there never moves
    /// the body scroll.
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

    /// info rows (MenuAction::None: section dividers, the catalog status
    /// line) never take the band — step past them in `dir`, giving up back
    /// at the start when nothing is actionable. View-following stays with
    /// the caller (nav/page/jump pull, open resets to top). Galleries are
    /// all-None content rows, never info: skipping is disabled there.
    fn menu_skips_none(&self) -> bool {
        !matches!(
            self.cur_menu(),
            Some(Menu::Context)
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

    /// A read-only menu: no selection band, no cursor movement — every row is
    /// a painted fact, nothing in it is a button. Esc still closes it.
    pub(super) fn menu_is_dead(&self) -> bool {
        matches!(self.cur_menu(), Some(Menu::Context))
    }

    pub(super) fn menu_nav(&mut self, dir: i32) {
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
                // start at the sensible end of the newly focused field
                if dir < 0 {
                    self.form_to_start();
                } else {
                    self.form_to_end();
                }
            }
        } else if dir.abs() > 1 {
            // page jump
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
        // the arrows walk the list, so the search field gives the caret back as
        // soon as the cursor moves
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
        // effort slider has its own column-aware hit-testing and hover
        // previews nothing — but only while the slider card is actually
        // drawn (wide terminal). The narrow fallback list hovers by row
        // like every other menu.
        if matches!(self.cur_menu(), Some(Menu::Effort { .. })) && !self.effort_hits.is_empty() {
            return Some(self.menu_sel);
        }
        // A list painted as a surface answers hover from its hit map: the block
        // scrolls with the history, so its rows no longer line up with the
        // popup's fixed title + air offset. Like the rest of this function, the
        // hit moves the selection — callers read the side effect, not the value.
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
        // list rows start after title + air row (+ frozen table header):
        // clicks on chrome select nothing. The sticky footer (with its
        // separator rule above it) maps after the visible body window.
        // Forms keep their own mapping.
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
        // undrawn menus (tests, first frame) have no window yet: every
        // body row is hoverable, like before the sticky footer existed
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
            return None; // separator rule
        } else if foot > 0 {
            let f = rel - shown - 1;
            if f >= foot {
                return None;
            }
            body + f
        } else {
            return None;
        };
        // info rows take no band and no click: hovering them changes nothing
        // (galleries exempt: their None rows are the content)
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

    /// Effort slider hit-test: label column or dot column → SELECTABLE
    /// index. Pure lookup, no side effects: hovering previews nothing, only
    /// a click (or arrows + Enter) changes the selection.
    /// The level the open effort picker commits to: the scoped model's own,
    /// or the session model's when the picker is unscoped. The bottom surface
    /// shows it next to its title so "what will Enter write" is readable
    /// without leaving the surface.
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

    /// unified nav index space: scrollable body rows, then sticky footer
    /// rows. Selection and activation run on this; scrolling only on body.
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

    /// The provider a surface is about: the one a Models list shows, or the one
    /// the cursor stands on in the Providers list. The chords read the same
    /// fact the rows carry, so a row and its key cannot disagree.
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

    /// The model the cursor stands on in a Models surface.
    pub(super) fn surface_model(&self) -> Option<String> {
        match self.menu_row_at(self.menu_sel) {
            Some((_, MenuAction::UseModel(k))) => Some(k.clone()),
            _ => None,
        }
    }

    /// The search field of whichever surface owns one. Providers and Models
    /// keep separate queries: the provider name you filtered to is not a model
    /// filter, and carrying it downstairs would hide the list you came for.
    pub(super) fn surf_field(&mut self) -> &mut TextArea<'static> {
        match self.cur_menu() {
            Some(Menu::Models { .. }) => &mut self.model_search,
            Some(Menu::Sessions) => &mut self.session_search,
            Some(Menu::Settings) => &mut self.config_search,
            _ => &mut self.prov_search,
        }
    }

    pub(super) fn surf_focused(&self) -> bool {
        match self.cur_menu() {
            Some(Menu::Models { .. }) => self.model_focus,
            Some(Menu::Sessions) => self.session_focus,
            Some(Menu::Settings) => self.config_focus,
            _ => self.prov_focus,
        }
    }

    pub(super) fn surf_set_focus(&mut self, on: bool) {
        match self.cur_menu() {
            Some(Menu::Models { .. }) => self.model_focus = on,
            Some(Menu::Sessions) => self.session_focus = on,
            Some(Menu::Settings) => self.config_focus = on,
            _ => self.prov_focus = on,
        }
    }

    /// Edit the surface's search field and re-filter the list. The selection
    /// stays on the field's own row — typing there must not throw the selector
    /// onto another row — while the window under it starts over. Erasing the
    /// last character gives the caret back to the list: an empty field with a
    /// cursor standing in it is a claim that someone is about to type.
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

    /// id of the session row currently highlighted in the sessions menu
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
        // confirmation prompts: honor the exact row clicked (label/cancel/confirm)
        if let Some(Menu::ConfirmDelete { .. }) = self.cur_menu() {
            let Some((_, action)) = self.menu_row_at(sel) else {
                return;
            };
            self.run_action(action.clone());
            return;
        }
        // approval: a click selects the option, it never commits — commit
        // is Enter only, so a stray click cannot approve a command
        if matches!(self.cur_menu(), Some(Menu::Approval { .. })) {
            if let Some((_, MenuAction::DecideApproval(_))) = self.menu_row_at(sel) {
                self.menu_sel = sel;
                self.dirty = true;
            }
            return;
        }
        // the click commits the row it landed on, not the row the cursor
        // happened to rest on: a press without a preceding hover event must
        // still open what was clicked
        self.menu_sel = sel;
        self.menu_activate();
    }

    /// run the confirm action of the open delete-confirmation prompt
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
        // galleries are showcases: any commit just closes them
        if matches!(
            self.cur_menu(),
            Some(Menu::Context)
                | Some(Menu::TestAnim)
                | Some(Menu::TestArt)
                | Some(Menu::TestColors)
                | Some(Menu::TestMd)
        ) {
            self.menu_back();
            return;
        }
        // ask_user: enter submits the selected option (or free text)
        if let Some(Menu::AskUser { .. }) = self.cur_menu() {
            if let Some((_, action)) = self.menu_row_at(self.menu_sel) {
                self.run_action(action.clone());
            }
            return;
        }
        // approval: enter commits the SELECTED option (deny is preselected).
        // Enter inside the grace window after the dialog opened is ignored:
        // it is the focus-steal path that once approved a destructive
        // command while the user was switching windows.
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

    /// ask_user: send the chosen answer back to the agent and close
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

    /// approval: move the selection to the given option without committing.
    /// Committing is Enter only (plus the open grace window), so neither a
    /// stray keypress nor a click can approve a command by itself.
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

    /// approval: send the user's decision to the agent and close
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

    /// pop an ask/approval interaction without re-triggering its esc handler
    fn close_interaction(&mut self) {
        self.menu_stack.pop();
        self.menu_sel = 0;
        self.form_fields.clear();
        self.form_focus = 0;
        self.prefill_form();
        self.build_menu_rows();
        self.dirty = true;
    }

    pub(super) fn run_action(&mut self, action: MenuAction) {
        match action {
            MenuAction::None => {}
            MenuAction::Back => self.menu_back(),
            MenuAction::DecideApproval(decision) => self.approval_decide(decision),
            MenuAction::OpenEffort => self.open_menu(Menu::Effort { model: None }),
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
            MenuAction::SetProviderFormat(format) => {
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
            MenuAction::OpenControls => self.open_menu(Menu::Controls),
            MenuAction::CycleCompactionSummary => {
                let all = crate::config::CompactionSummary::ALL;
                let cur = all
                    .iter()
                    .position(|s| s.as_str() == self.cfg.compaction.summary.as_str())
                    .unwrap_or(0);
                self.cfg.compaction.summary = all[(cur + 1) % all.len()];
                self.cfg.save().ok();
                self.status(
                    &format!(
                        "compaction summary = {}",
                        self.cfg.compaction.summary.as_str()
                    ),
                    StatusKind::Ok,
                );
                self.build_menu_rows();
            }
            MenuAction::OpenCompactionSummary => self.open_menu(Menu::CompactionSummary),
            MenuAction::SetCompactionSummary(summary) => {
                self.cfg.compaction.summary = summary;
                self.cfg.save().ok();
                self.status(
                    &format!("compaction summary = {}", summary.as_str()),
                    StatusKind::Ok,
                );
                self.menu_back();
            }
            MenuAction::CycleUndoShadow => {
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
            MenuAction::ToggleSkillsAutoLoad => {
                self.cfg.skills.auto_load = !self.cfg.skills.auto_load;
                self.cfg.save().ok();
                let on = self.cfg.skills.auto_load;
                self.status(&format!("skills auto-load: {}", on_off(on)), StatusKind::Ok);
                self.build_menu_rows();
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
                    // stored unwrapped; build_menu_rows adds the single Confirm layer
                    action: MenuAction::DeleteProvider(p),
                });
            }
            MenuAction::CheckProvider(name) => {
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
                // A thread, not the tick: the probe waits on the network.
                // It builds its own single-thread runtime so it works no
                // matter which context the menu runs in.
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
            MenuAction::OpenSession(id) => {
                if id == self.session.id.to_string() {
                    self.menu_home();
                    self.status("already in this session", StatusKind::Info);
                } else if self.streaming {
                    self.show_busy_status();
                } else {
                    match Session::load(&id) {
                        Ok(s) => self.apply_session(s),
                        Err(e) => self.status(&format!("load session: {e:#}"), StatusKind::Err),
                    }
                }
            }
            MenuAction::RenameSession(id) => {
                self.open_menu(Menu::EditSessionTitle { id });
            }
            MenuAction::PinSession(id) => {
                // headers hold menu state; the durable file is updated
                // through a full load/toggle/save round-trip
                let mut pinned = false;
                if let Some(s) = self.sessions.iter_mut().find(|s| s.id.to_string() == id) {
                    s.pinned = !s.pinned;
                    pinned = s.pinned;
                }
                if let Ok(mut s) = Session::load(&id) {
                    s.pinned = pinned;
                    let _ = s.save();
                }
                let state = if pinned { "pinned" } else { "unpinned" };
                self.status(&format!("session {state}"), StatusKind::Ok);
                if self.session.id.to_string() == id {
                    // keep the in-memory copy consistent with the file
                    self.session.pinned = pinned;
                }
                // re-sort and rebuild while staying in the menu
                SessionHeader::sort_sessions(&mut self.sessions);
                self.build_menu_rows();
            }
            MenuAction::ToggleTypewriter => {
                self.cfg.ui.typewriter = !self.cfg.ui.typewriter;
                self.cfg.save().ok();
                let on = self.cfg.ui.typewriter;
                self.status(&format!("typewriter: {}", on_off(on)), StatusKind::Ok);
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
                // per-frame transcript timings for lag hunting: one line per
                // drawn frame plus tool markers, into a fresh temp file
                let on = !self.perf.enabled();
                let renders = self.test_renders;
                let wraps = crate::tui::markdown::wrap_tagged_calls();
                match self.perf.set_enabled(on, renders, wraps) {
                    Ok(msg) => self.status(&msg, StatusKind::Ok),
                    Err(msg) => self.status(&msg, StatusKind::Err),
                }
                self.build_menu_rows();
            }
            MenuAction::ToggleMode => {
                let next = self.mode.toggle();
                self.set_mode(next);
                self.status(&format!("mode: {}", self.mode.label()), StatusKind::Info);
            }
            MenuAction::OpenSessions => {
                self.open_menu(Menu::Sessions);
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
            MenuAction::Confirm(inner) => {
                let inner = *inner;
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
                    // close the prompt first so the outcome toast is
                    // visible on the bare chat, not hidden behind a menu
                    self.menu_home();
                    let root = self.project_root.clone();
                    match self.deletable_plan_id() {
                        Some(id) => {
                            let plan_path =
                                crate::plan::plans_dir(&root).join(format!("{id}.json"));
                            // Never claim success the filesystem didn't confirm:
                            // on Windows a locked file (editor, AV, second
                            // instance) makes remove_file fail while the plan
                            // is still on disk.
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
                                    // #171: resolution is session-strict — another
                                    // session's active plan never surfaces here,
                                    // so the session is simply plan-less now.
                                    let sid = self.session.id.to_string();
                                    let note = match crate::plan::open_active_for_session(
                                        &root,
                                        Some(&sid),
                                    )
                                    .ok()
                                    .flatten()
                                    {
                                        Some(next) => {
                                            let goal: String =
                                                next.goal.text.chars().take(60).collect();
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
                                Err(e) => self
                                    .status(&format!("plan delete failed: {e:#}"), StatusKind::Err),
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
                self.cfg.save().ok();
                match &inner {
                    // provider gone: land on a fresh providers list
                    MenuAction::DeleteProvider(_) => {
                        self.menu_home();
                        self.open_menu(Menu::Providers);
                    }
                    // model gone: back to its provider's model list
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
                    // session gone: back to the sessions list
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
            MenuAction::DeletePlan => {
                self.run_action(MenuAction::Confirm(Box::new(MenuAction::DeletePlan)));
            }
            MenuAction::SetEffort(level) => {
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
                // back one level (not home): from a form the fields rebuild
                // from the saved config, so the effort row shows the pick
                // with no lost text edits
                self.menu_back();
                let plan = crate::providers::effort::plan(level, self.effort_support_for(&target));
                let kind = if plan.is_honoured() {
                    StatusKind::Ok
                } else {
                    StatusKind::Warn
                };
                self.status(&plan.label(), kind);
            }
            MenuAction::AskSelect { q, idx } => {
                // Single-choice: pick this option, clear others in same question, focus stays
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
            MenuAction::AskConfirm => {
                // Collect answers for all questions
                let text = match self.cur_menu() {
                    Some(Menu::AskUser { questions, .. }) => {
                        let mut parts = Vec::new();
                        let single_no_header =
                            questions.len() == 1 && questions[0].header.is_empty();
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
                            if answer.is_empty() {
                                // No selection, try to use first option if single, or empty
                                answer = String::new();
                            }
                            let header = if single_no_header {
                                "".to_string()
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

    pub(super) fn menu_title(&self) -> String {
        match &self.cur_menu() {
            Some(Menu::Settings) => " Config ".into(),
            Some(Menu::CompactionSummary) => " Config · compaction summary ".into(),
            Some(Menu::UndoShadow) => " Config · undo shadow ".into(),
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
            Some(Menu::TestAnim) => " Anim ".into(),
            Some(Menu::TestArt) => " Art ".into(),
            Some(Menu::TestColors) => " Colors ".into(),
            Some(Menu::TestMd) => " Md ".into(),
            Some(Menu::AskUser { .. }) => " Ask ".into(),
            Some(Menu::Approval { .. }) => " Confirm command ".into(),
            Some(Menu::AskFree { .. }) => " Answer ".into(),
            Some(Menu::Todo) => " To-do ".into(),
            Some(Menu::Plan) => " Plan ".into(),
            None => String::new(),
        }
    }

    pub(super) fn build_menu_rows(&mut self) {
        self.menu_rows.clear();
        self.menu_sticky_footer.clear();
        self.menu_table_header = None;
        self.menu_footer_text = None;
        let Some(menu) = self.cur_menu().cloned() else {
            return;
        };
        let row = |l: Line<'static>, a: MenuAction| (l, a);
        match menu {
            Menu::Settings => {
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
                        "plan budget ratio",
                        ScalarSetting::PlanBudgetRatio.current(&self.cfg),
                        MenuAction::EditScalar(ScalarSetting::PlanBudgetRatio),
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
                        "compaction summary",
                        self.cfg.compaction.summary.as_str().to_string(),
                        MenuAction::OpenCompactionSummary,
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
                            Span::styled(
                                format!("{label:<28}"),
                                Style::new().fg(Theme::FG()),
                            ),
                            Span::styled(val, Theme::dim()),
                        ]),
                        action,
                    ));
                }
                self.menu_sel = sel_before;
            }
            Menu::CompactionSummary => {
                let current = self.cfg.compaction.summary.as_str();
                for s in crate::config::CompactionSummary::ALL {
                    let style = if s.as_str() == current {
                        Theme::brand_ink()
                    } else {
                        Style::new().fg(Theme::FG())
                    };
                    self.menu_rows.push(row(
                        Line::from(vec![Span::styled(s.as_str(), style)]),
                        MenuAction::SetCompactionSummary(s),
                    ));
                }
            }
            Menu::UndoShadow => {
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
            Menu::EditScalar(..) => {
                self.menu_rows.push(row(Line::default(), MenuAction::None));
            }
            Menu::Help => {
                self.menu_footer_text = Some("esc: close".into());
            }
            Menu::Controls => {
                const CONTROLS: &[(&str, &str)] = &[
                    ("enter", "submit · confirm"),
                    ("tab", "plan / act mode"),
                    ("esc", "cancel · back"),
                    ("ctrl+c", "stop · copy"),
                    ("ctrl+d", "quit if empty"),
                    ("ctrl+j", "newline"),
                    ("ctrl+v", "paste"),
                    ("ctrl+s", "sessions"),
                    ("ctrl+t", "todo"),
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
            Menu::Debug => {
                let setting = |l: &str, val: String, a: MenuAction| {
                    row(
                        Line::from(vec![
                            Span::styled(format!(" {l:<18}"), Theme::FG()),
                            Span::styled(val, Theme::dim()),
                        ]),
                        a,
                    )
                };
                let info = |l: &str, v: &str| {
                    row(
                        Line::from(vec![
                            Span::styled(format!(" {l:<18}"), Theme::dim()),
                            Span::styled(v.to_string(), Theme::base()),
                        ]),
                        MenuAction::None,
                    )
                };
                let log_path = crate::config::data_dir()
                    .map(|d| d.join("debug.log").display().to_string())
                    .unwrap_or_else(|_| "debug.log".into());
                let cfg_dir = crate::config::config_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                self.menu_rows.push(setting(
                    "typewriter",
                    on_off(self.cfg.ui.typewriter),
                    MenuAction::ToggleTypewriter,
                ));
                self.menu_rows.push(setting(
                    "http debug log",
                    on_off(self.cfg.ui.http_log),
                    MenuAction::ToggleHttpLog,
                ));
                self.menu_rows.push(setting(
                    "perf frame log",
                    if self.perf.enabled() {
                        self.perf.path().to_string()
                    } else {
                        on_off(false)
                    },
                    MenuAction::TogglePerfLog,
                ));
                self.menu_rows.push(setting(
                    "effort",
                    self.model_cfg.effort.as_str().to_string(),
                    MenuAction::OpenEffort,
                ));
                self.menu_rows.push(setting(
                    "mode",
                    self.mode.label().to_string(),
                    MenuAction::ToggleMode,
                ));
                self.menu_rows.push(setting(
                    "sessions",
                    format!("{} saved", self.sessions.len()),
                    MenuAction::OpenSessions,
                ));
                self.menu_rows.push(info("model", &self.model_cfg.id));
                self.menu_rows
                    .push(info("provider", &self.model_cfg.provider));
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
                                    .then(|| "set".to_string())
                            })
                    })
                    .unwrap_or_else(|| "none".into());
                self.menu_rows.push(info("api key", &key_state));
                self.menu_rows.push(info("log file", &log_path));
                self.menu_rows.push(info("config", &cfg_dir));
                self.menu_footer_text = Some("enter: toggle value · esc: done".into());
            }
            Menu::Sessions => {
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
                let pinned: Vec<&SessionHeader> =
                    visible.iter().filter(|s| s.pinned).copied().collect();
                let (mine, foreign): (Vec<&&SessionHeader>, Vec<&&SessionHeader>) =
                    visible.iter().partition(|s| {
                        s.pinned
                            || crate::session::Session::project_is_here(
                                &s.project,
                                &self.project_root,
                            )
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
            Menu::EditSessionTitle { .. } => {
                self.menu_rows.push(row(Line::default(), MenuAction::None));
            }
            Menu::Providers => {
                // A surface row list, not a table: the search field, then the
                // provider names alone. Model counts and key state left with
                // the columns — a provider that needs saying something says it
                // itself.
                // the search field is a row of this list: the arrows reach it,
                // Enter or a click gives it the caret. The painter draws the
                // glyph, the query and the cursor there, not this line.
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled("Search", Theme::dim())]),
                    MenuAction::FocusSearch,
                ));
                // the provider the session's model comes from wears the brand
                // ink: one row in the list says "this is what you are on"
                let applied = self.model_cfg.provider.clone();
                // the search field filters by name, case-insensitively; an
                // empty field lists everything
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
            Menu::EditProvider { name } => {
                let is_builtin = name
                    .as_ref()
                    .is_some_and(|n| self.cfg.is_builtin_provider(n));
                let (name_val, fmt_val, url_val, key_val, env_val) =
                    match name.as_ref().and_then(|n| self.cfg.providers.get(n)) {
                        Some(pc) => (
                            name.clone().unwrap_or_default(),
                            pc.format.as_str().to_string(),
                            pc.base_url.clone(),
                            super::forms::FormField::mask_value(
                                pc.api_key.as_deref().unwrap_or(""),
                            ),
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
            Menu::EditProviderField { .. } => {
                self.menu_rows.push(row(Line::default(), MenuAction::None));
            }
            Menu::ProviderFormat { name } => {
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
            Menu::Models { provider } => {
                // A surface row list, not a table: the search field, then the
                // model names. The column headers are gone; what a model has to
                // say about itself rides its own row, in gray parentheses.
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled("Search", Theme::dim())]),
                    MenuAction::FocusSearch,
                ));
                let current = self.session.model_key.clone();
                let q = self.model_search.lines().join("").to_lowercase();
                for (k, m) in &self.cfg.models {
                    if m.provider != *provider || !m.status.visible_in_picker() {
                        continue;
                    }
                    if !q.is_empty() && !k.to_lowercase().contains(&q) {
                        continue;
                    }
                    // the level this model would actually run at. `off` is the
                    // case most rows sit in, so it stays silent and the models
                    // that think are the rows that read longer
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
                    // the model the session runs on wears the brand ink: one row
                    // says "this is what you are on", and the name keeps its own
                    // first cell instead of spending it on a marker
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
            Menu::EditModel { key, .. } => {
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
            Menu::EditModelField { .. } => {
                self.menu_rows.push(row(Line::default(), MenuAction::None));
            }
            Menu::ConfirmDelete { label, action } => {
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
            Menu::Effort { model } => {
                // The menu has room for the whole truth: what each level does
                // on the target model, not just its name.
                let target = model.unwrap_or_else(|| self.session.model_key.clone());
                let target_effort = self
                    .cfg
                    .models
                    .get(&target)
                    .map(|m| m.effort)
                    .unwrap_or(self.model_cfg.effort);
                let support = self.effort_support_for(&target);
                let plans: Vec<(EffortLevel, crate::providers::effort::Plan)> =
                    EffortLevel::SELECTABLE
                        .iter()
                        .map(|lvl| (*lvl, crate::providers::effort::plan(*lvl, support)))
                        .collect();
                for (lvl, plan) in plans {
                    let current = lvl == target_effort;
                    let note = match plan.status {
                        crate::providers::effort::Status::Applied => String::new(),
                        crate::providers::effort::Status::Clamped { to } => {
                            format!("  → sent as {to}")
                        }
                        crate::providers::effort::Status::Ignored { why } => {
                            format!("  → ignored by model ({why})")
                        }
                    };
                    let name_style = if plan.is_honoured() {
                        Theme::base()
                    } else {
                        Theme::dim()
                    };
                    // current level marks the lead column like session rows
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
            Menu::Context => {
                // One continuous scale: a part's width is its share of the
                // window, laid largest first, and the tail left over is free.
                // Size reads as width and only as width — the texture experiment
                // made one part look like two.
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
                // The fills are the brand ramp cut into as many stops as there
                // are parts: the loudest end leads the bar, the plum end
                // trails, so the scale is one gradient and every part is a
                // different slice of it. Colors ride the rank, not the name —
                // the legend below carries the names, and it is sorted too.
                let parts: Vec<(&str, u64, ratatui::style::Color)> = ranked
                    .iter()
                    .enumerate()
                    .map(|(i, (name, tokens))| {
                        let (r, g, b) = crate::tui::art::coral_at(i, ranked.len());
                        (*name, *tokens, ratatui::style::Color::Rgb(r, g, b))
                    })
                    .collect();

                // header: the truth about the window
                let used = session.context_tokens_used();
                let limit = session.context_limit.max(1);
                self.menu_rows.push(row(
                    Line::from(vec![
                        Span::styled(session.model_key.clone(), Theme::FG()),
                        Span::styled(
                            format!(
                                " · used {} ({:.1}%)",
                                fmt_k(used),
                                session.context_percent()
                            ),
                            Theme::dim(),
                        ),
                    ]),
                    MenuAction::None,
                ));
                self.menu_rows
                    .push(row(super::view::blank(), MenuAction::None));

                // one proportional scale, no per-part labels: the legend below
                // carries the names, the bar carries the sizes
                let width = (self.menu_rect.width as usize)
                    .saturating_sub(4)
                    .clamp(20, 120);
                let mut widths: Vec<usize> = parts
                    .iter()
                    .map(|(_, tokens, _)| ((*tokens as f64 / limit as f64) * width as f64) as usize)
                    .collect();
                // a part the legend names stays findable in the bar
                for (w, (_, tokens, _)) in widths.iter_mut().zip(&parts) {
                    if *tokens > 0 {
                        *w = (*w).max(1);
                    }
                }
                // rounding — or a window pushed past its limit — must not
                // overflow the row: give back from the widest first
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
                    // a solid band: the color is the background, the glyph is
                    // just a space — shade glyphs read as halftone noise
                    meter.push(Span::styled(" ".repeat(*cells), Style::new().bg(*color)));
                }
                if free_cells > 0 {
                    meter.push(Span::styled(
                        " ".repeat(free_cells),
                        // one step lighter than the panel: visible, still quiet
                        Style::new().bg(Theme::INPUT_BG()),
                    ));
                }
                self.menu_rows
                    .push(row(Line::from(meter), MenuAction::None));
                self.menu_rows
                    .push(row(super::view::blank(), MenuAction::None));

                // legend: the dot wears the block's color. Percentages are
                // shares of the window, normalized to the provider's real
                // usage: the parts are estimates that may undershoot or
                // overshoot `prompt_tokens`, so parts plus "unaccounted" are
                // scaled to sum exactly to the used percent, and free completes
                // the scale to 100.
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
                // what the parts do not cover: the tokenizer, reasoning replay,
                // message envelopes. Shown as its own row, never spread over
                // the others to make the percentages reach 100.
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
                // #209 L14: the compaction budget measures history alone, on
                // purpose — gating on the fixed prefix fires a useless
                // compaction every turn (`after == before`, loop_compact.rs:17-21).
                // The fixed share is deliberately not printed here: the owner
                // asked for the panel to stay about the user's own content.
            }
            Menu::TestAnim => {
                // One row per frame set: the live frame in the brand's loud end,
                // the name, then the whole cycle — a pick should not depend on
                // remembering glyphs that only appear once every 50 ms. Rebuilt
                // on every animation tick while this gallery is open.
                const STRIP: usize = 24;
                // Half the frame rate of the tool spinner: at 20 FPS the short
                // cycles (2..4 frames) strobe instead of turning, and nobody can
                // read a glyph they are picking.
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
            Menu::TestArt => {
                // static art blocks: one numbered label row per variant,
                // then its lines. All dead rows (viewing only) — the pick
                // comes back as a variant number.
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
            Menu::TestColors => {
                // one row per palette entry: name plus a live sample in the
                // style itself. All dead rows, like the other galleries.
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
            Menu::TestMd => {
                // the showcase, rendered once with the live highlighter.
                // Static rows afterwards (no streaming here to re-render).
                for line in crate::tui::markdown::render(
                    crate::tui::markdown::SHOWCASE,
                    self.menu_rect.width.max(20),
                    &self.hl,
                ) {
                    self.menu_rows.push(row(line, MenuAction::None));
                }
            }
            Menu::AskUser { questions, .. } => {
                // Render each question with its options. The focused question is highlighted,
                // and its options are selectable. Custom is inline per question.
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
                    // Question text — selectable, not an action
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
                        let checked = if q.multiple {
                            let picked = self
                                .ask_picked
                                .get(q_idx)
                                .and_then(|v| v.get(o_idx).copied())
                                .unwrap_or(false);
                            if picked { " [x] " } else { " [ ] " }
                        } else {
                            let picked = self
                                .ask_picked
                                .get(q_idx)
                                .and_then(|v| v.get(o_idx).copied())
                                .unwrap_or(false);
                            if picked && !q.multiple {
                                " ● "
                            } else {
                                " ○ "
                            }
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
                        let custom_text =
                            self.ask_custom.get(q_idx).map(|s| s.as_str()).unwrap_or("");
                        let is_custom_focused = self.ask_custom_focus == Some(q_idx);
                        if is_custom_focused {
                            // Inline editor for custom answer
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
                    // Separator between questions, except after last
                    if q_idx + 1 < questions.len() {
                        self.menu_rows.push(row(
                            Line::from(vec![Span::styled(" ──".to_string(), Theme::dim())]),
                            MenuAction::None,
                        ));
                    }
                }
                // Global confirm row
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
            Menu::Approval {
                command, reason, ..
            } => {
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
            Menu::AskFree { .. } => {}
            Menu::Todo => {
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(
                        " agent to-do list",
                        Theme::accent_bold(),
                    )]),
                    MenuAction::None,
                ));
                if self.todos.is_empty() {
                    self.menu_rows.push(row(
                        Line::from(vec![Span::styled(
                            "  (no active plan steps yet)",
                            Theme::dim(),
                        )]),
                        MenuAction::None,
                    ));
                } else {
                    for (i, item) in self.todos.iter().enumerate() {
                        self.menu_rows.push(row(
                            Line::from(vec![
                                Span::styled(format!("  {}. ", i + 1), Theme::marker()),
                                Span::styled(item.clone(), Theme::base()),
                            ]),
                            MenuAction::None,
                        ));
                    }
                }
                self.menu_footer_text = Some("esc: close · ctrl+t: toggle".into());
            }
            Menu::Plan => {
                let Some(plan) = self.session_plan() else {
                    self.menu_rows.push(row(
                        Line::from(vec![Span::styled("  no active plan", Theme::dim())]),
                        MenuAction::None,
                    ));
                    self.menu_footer_text = Some("esc: close".into());
                    // fall through to sel clamp
                    self.menu_sel = self.menu_sel.min(self.menu_len().saturating_sub(1));
                    return;
                };
                let status = match plan.status {
                    plan::PlanStatus::Active => "active",
                    plan::PlanStatus::Completed => "completed",
                    plan::PlanStatus::Abandoned => "abandoned",
                    plan::PlanStatus::Blocked => "blocked",
                };
                self.menu_rows.extend(plan_rows(
                    &plan,
                    format!("  plan {}", plan.id),
                    format!(" · {status}"),
                ));
                self.menu_footer_text = Some("up/down: scroll · esc: close".into());
            }
        }
        if self.menu_sel >= self.menu_len() {
            self.menu_sel = self.menu_len().saturating_sub(1);
        }
    }
}

/// Render the session's plan as /plan menu rows.
fn plan_rows(
    plan: &crate::plan::Plan,
    heading_main: String,
    heading_suffix: String,
) -> Vec<(Line<'static>, MenuAction)> {
    let row = |l: Line<'static>, a: MenuAction| (l, a);
    let mut rows: Vec<(Line<'static>, MenuAction)> = Vec::new();
    // Wrap width for plan text: menu inner is w-4, w is 78 max
    // so 68 is safe for wide, still reasonable for narrow (will
    // be truncated to w, but far better than cutting at 78).
    const WRAP: usize = 68;
    let push_wrapped = |rows: &mut Vec<(Line<'static>, MenuAction)>,
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
    let c = plan.counts();
    rows.push(row(
        Line::from(vec![
            Span::styled(heading_main, Theme::accent_bold()),
            Span::styled(heading_suffix, Theme::dim()),
        ]),
        MenuAction::None,
    ));
    // goal: may be long, wrap it
    let goal_prefix = format!("  goal [{} criteria]: ", plan.criteria.len());
    push_wrapped(
        &mut rows,
        &goal_prefix,
        &plan.goal.text,
        Theme::dim(),
        Theme::base(),
    );
    if !plan.constraints.is_empty() {
        push_wrapped(
            &mut rows,
            "  constraints: ",
            &plan.constraints.join(" · "),
            Theme::dim(),
            Theme::dim(),
        );
    }
    if !plan.criteria.is_empty() {
        rows.push(row(
            Line::from(vec![Span::styled("  criteria:", Theme::dim())]),
            MenuAction::None,
        ));
        // done-criteria are the agent's own plain-text notes; the host runs
        // nothing and certifies nothing, so they render without status colors.
        for note in &plan.criteria {
            push_wrapped(&mut rows, "    · ", note, Theme::dim(), Theme::base());
        }
    }
    rows.push(row(
        Line::from(vec![Span::styled(
            format!(
                "  steps: {} done · {} in progress · {} blocked · {} pending · {} cancelled",
                c.done, c.in_progress, c.blocked, c.pending, c.cancelled
            ),
            Theme::base(),
        )]),
        MenuAction::None,
    ));
    for s in &plan.steps {
        let (marker, style) = match s.status {
            plan::StepStatus::Done => ("[x]", Theme::ok()),
            plan::StepStatus::InProgress => ("[>]", Theme::accent()),
            plan::StepStatus::Blocked => ("[!]", Theme::err()),
            plan::StepStatus::Cancelled => ("[-]", Theme::dim()),
            plan::StepStatus::Pending | plan::StepStatus::Reopened => ("[ ]", Theme::dim()),
        };
        let prefix = format!("  {marker} {} ", s.id);
        let mut title = s.title.clone();
        if s.stale_goal == Some(true) {
            title.push_str("  [stale goal]");
        }
        push_wrapped(&mut rows, &prefix, &title, style, style);
        if let Some(reason) = &s.reason {
            push_wrapped(
                &mut rows,
                "      reason: ",
                reason,
                Theme::dim(),
                Theme::dim(),
            );
        }
        if let Some(summary) = &s.summary {
            push_wrapped(&mut rows, "      ", summary, Theme::dim(), Theme::dim());
        }
    }
    rows
}

/// One session row in the Sessions surface: the title in house foreground
/// (brand ink for the active session) and the time, model and context size in
/// gray parentheses, matching the Models surface.
fn session_row(s: &SessionHeader, is_current: bool) -> (Line<'static>, MenuAction) {
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

/// Section label for a session's activity day: Today, Yesterday, or the
/// calendar date. Rows arrive newest-first, so labels change rarely.
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

/// Day-section header style: muted brand ink like H2 (bold), same truecolor
/// fallback to the indexed house accent.
fn day_section_style() -> Style {
    crate::tui::art::h2_style()
}

/// One grid cell: truncated/padded, left- or right-aligned.
fn tcell(text: &str, w: usize, right: bool, style: impl Into<Style>) -> Span<'static> {
    let t = super::view::truncate_display_width(text, w);
    let fill = " ".repeat(w.saturating_sub(super::view::cols(&t)));
    Span::styled(
        if right {
            format!("{fill}{t}")
        } else {
            format!("{t}{fill}")
        },
        style,
    )
}

/// Frozen column header: same grid as the rows, exact-fit to the budget
/// (a narrow header must not outgrow its columns).
fn table_header(cells: Vec<Span<'static>>, budget: usize) -> Line<'static> {
    fit_line_width(table_line(cells), budget)
}
/// One table row: lead space + 2-column gaps, every cell grid-aligned.
/// Headers use the same builder with dim labels, so columns can never drift.
fn table_line(cells: Vec<Span<'static>>) -> Line<'static> {
    let mut spans = vec![Span::styled(" ".to_string(), Theme::base())];
    for (i, c) in cells.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ".to_string(), Theme::dim()));
        }
        spans.push(c);
    }
    Line::from(spans)
}

/// Content width for table menus: the card minus chrome, floored so
/// narrow terminals shrink columns instead of clipping rows. No ceiling:
/// the elastic column of every table absorbs the slack, so wide cards
/// fill edge to edge instead of leaving a dead right margin. An undrawn
/// menu (zero rect) builds at full width — the draw-time re-resolve in
/// draw_menu shrinks it once the real card is known.
pub(super) fn table_budget(menu_rect_w: u16) -> usize {
    if menu_rect_w == 0 {
        return 72;
    }
    (menu_rect_w as usize).saturating_sub(6).max(30)
}

/// cut or pad a styled line to exactly `width` display columns, keeping
/// span styles on the surviving fragments
pub(super) fn fit_line_width(line: Line<'static>, width: usize) -> Line<'static> {
    let mut out = Vec::new();
    let mut used = 0usize;
    for s in line.spans {
        let w = unicode_width::UnicodeWidthStr::width(s.content.as_ref());
        if used + w <= width {
            used += w;
            out.push(s);
        } else {
            let mut kept = String::new();
            let mut cw = 0usize;
            for ch in s.content.chars() {
                let chw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                if used + cw + chw > width {
                    break;
                }
                kept.push(ch);
                cw += chw;
            }
            used += cw;
            out.push(Span::styled(kept, s.style));
            break;
        }
    }
    if used < width {
        out.push(Span::styled(" ".repeat(width - used), Style::new()));
    }
    Line::from(out)
}
