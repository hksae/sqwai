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
    "/constraints",
    "/debug",
    "/diary",
    "/exit",
    "/goal",
    "/graph-rebuild",
    "/help",
    "/init",
    "/lsp",
    "/mcp",
    "/mode",
    "/models",
    "/new",
    "/plan",
    "/providers",
    "/sessions",
    "/settings",
    "/skill",
    "/skills",
    "/undo",
];

/// Second-level completions: (command, subcommands), in the same order as
/// the dispatch arms. Commands absent here keep level-1-only behavior
/// (the popup hides after the first space).
pub(super) const SUBCOMMANDS: &[(&str, &[&str])] = &[
    (
        "/plan",
        &[
            "history", "limit", "complete", "abandon", "waive", "confirm", "delete",
        ],
    ),
    ("/undo", &["step"]),
    ("/providers", &["update"]),
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

pub(super) const POPUP_MAX_ROWS: usize = 14;

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

/// Compact $/1M price for menu rows: 2.0 -> "2", 0.15 -> "0.15".
pub(super) fn fmt_price(v: f64) -> String {
    trim_num(v, 4)
}

fn trim_num(v: f64, prec: usize) -> String {
    let s = format!("{v:.prec$}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

#[derive(Clone)]
#[allow(clippy::large_enum_variant)] // popup state is rebuilt per frame; form payloads are small in practice
pub(super) enum Menu {
    /// /settings: top-level settings hub
    Settings,
    /// /settings -> Appearance: organizer for visual settings
    Appearance,
    Mcp,
    Lsp,
    Skills,
    Providers,
    Models {
        provider: String,
    },
    PickModel {
        provider: String,
    },
    Sessions,
    DeleteSessions,
    /// /debug: runtime toggles and diagnostics
    #[allow(dead_code)]
    Debug,
    /// /settings -> Agent: plan, memory, diary and compaction scalars
    Agent,
    /// /settings -> Safety: secrets globs and blocked patterns
    Safety,
    /// /settings -> Undo: checkpoint retention and shadow store
    Undo,
    /// pick the default model for new sessions
    PickDefaultModel,
    /// /help: help sections
    Help,
    /// /help -> controls: key combo reference
    #[allow(dead_code)]
    Controls,
    /// single-field form editing one numeric host setting
    EditScalar(ScalarSetting),
    /// single-field form appending one entry to a string list
    AddListItem(ListSection),
    /// pick an entry of a string list to delete
    DeleteListItems(ListSection),
    /// MCP server add (None) or edit (Some) form
    EditMcpServer {
        index: Option<usize>,
    },
    /// one MCP server: edit / enable-disable / delete
    McpServer {
        index: usize,
    },
    /// pick an MCP server to delete
    DeleteMcpServer,
    /// LSP server add (None) or edit (Some) form
    EditLspServer {
        index: Option<usize>,
    },
    /// one LSP server: edit / enable-disable / delete
    LspServer {
        index: usize,
    },
    /// pick an LSP server to delete
    DeleteLspServer,
    /// single-field form to rename a session
    EditSessionTitle {
        id: String,
    },
    EditProvider {
        name: Option<String>,
    },
    EditModel {
        provider: String,
        key: Option<String>,
    },
    ConfirmDelete {
        label: String,
        action: MenuAction,
    },
    DeleteModelList {
        provider: String,
    },
    Effort,
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
    /// a propose_plan draft preview, read-only (same view as Plan)
    PlanPreview {
        draft: crate::plan::Plan,
    },
    /// all delegated child agents, opened with Ctrl+A
    Subagents,
    /// code graph neighborhood and details, opened with Ctrl+G (§2.4.10)
    GraphView {
        focus_key: String,
        trail: Vec<String>,
        depth: u8,
        search_filter: Option<String>,
    },
}

#[derive(Clone)]
pub(super) enum MenuAction {
    None,
    Back,
    GraphFocus(String),
    GraphBack,
    GraphDepth(i8),
    OpenModels(String),
    OpenAppearance,
    OpenProviders,
    OpenMcp,
    OpenLsp,
    OpenSkills,
    OpenDebug,
    AddProvider,
    EditProvider(String),
    DeleteProvider(String),
    /// check that the provider answers with the configured credentials
    CheckProvider(String),
    AddModel(String),
    EditModel(String, String),
    DeleteModel(String, String),
    PickModelList(String),
    DeleteModelList(String),
    UseModel(String),
    OpenSession(String),
    NewSession,
    RenameSession(String),
    PinSession(String),
    /// kept for future use; deletion now goes through the `d` key
    #[allow(dead_code)]
    DeleteSessionList,
    DeleteSession(String),
    ToggleTypewriter,
    ToggleHttpLog,
    TogglePerfLog,
    ToggleShowCost,
    CycleModelEffort,
    CycleDefaultEffort,
    /// cycle what the current model is declared to do with the slider
    CycleEffortControl,
    /// toggle "this model always reasons, `off` cannot be honoured"
    ToggleEffortAlwaysOn,
    ToggleMode,
    OpenSessions,
    Confirm(Box<MenuAction>),
    DeletePlan,
    UpdateBuiltins,
    SetEffort(EffortLevel),
    OpenSubagent(u64),
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
    OpenAgent,
    OpenSafety,
    OpenUndo,
    OpenPickDefaultModel,
    #[allow(dead_code)]
    OpenControls,
    SetDefaultModel(String),
    CycleDiaryEffort,
    CycleCompactionSummary,
    CycleUndoShadow,
    ToggleSkillsAutoLoad,
    AddMcpServer,
    EditMcpServer(usize),
    OpenMcpServer(usize),
    ToggleMcpServer(usize),
    DeleteMcpServerList,
    DeleteMcpServer(usize),
    AddLspServer,
    EditLspServer(usize),
    OpenLspServer(usize),
    ToggleLspServer(usize),
    DeleteLspServerList,
    DeleteLspServer(usize),
    AddListItem(ListSection),
    DeleteListItems(ListSection),
    DeleteListItem(ListSection, usize),
    EditScalar(ScalarSetting),
}

/// One numeric host setting editable from /settings. Enums (diary effort,
/// compaction summary, undo shadow) are cycle rows, not scalars.
#[derive(Clone, Copy)]
pub(super) enum ScalarSetting {
    PlanBudgetRatio,
    PlanMaxSteps,
    PlanNudgeAfter,
    MemoryLoadBudgetRatio,
    MemoryHeadingDays,
    MemoryMaxTokens,
    MemoryMaxProposals,
    DiaryTokenBudget,
    DiaryTimeoutSecs,
    DiaryBatchSteps,
    DiaryBatchMinutes,
    CompactionThreshold,
    CompactionStageRatio,
    CompactionKeepTurns,
    CompactionAnchorRatio,
    UndoKeepPerSession,
    UndoMaxTreeFiles,
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
            Self::MemoryHeadingDays => "heading days",
            Self::MemoryMaxTokens => "max tokens",
            Self::MemoryMaxProposals => "max proposals",
            Self::DiaryTokenBudget => "token budget",
            Self::DiaryTimeoutSecs => "timeout secs",
            Self::DiaryBatchSteps => "batch steps",
            Self::DiaryBatchMinutes => "batch minutes",
            Self::CompactionThreshold => "threshold",
            Self::CompactionStageRatio => "stage ratio",
            Self::CompactionKeepTurns => "keep turns",
            Self::CompactionAnchorRatio => "anchor ratio",
            Self::UndoKeepPerSession => "keep per session",
            Self::UndoMaxTreeFiles => "max tree files",
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
            Self::MemoryHeadingDays => cfg.memory.heading_days.to_string(),
            Self::MemoryMaxTokens => cfg.memory.max_tokens.to_string(),
            Self::MemoryMaxProposals => cfg.memory.max_proposals_per_turn.to_string(),
            Self::DiaryTokenBudget => cfg.diary.token_budget.to_string(),
            Self::DiaryTimeoutSecs => cfg.diary.timeout_secs.to_string(),
            Self::DiaryBatchSteps => cfg.diary.batch_steps.to_string(),
            Self::DiaryBatchMinutes => cfg.diary.batch_minutes.to_string(),
            Self::CompactionThreshold => cfg.compaction.threshold.to_string(),
            Self::CompactionStageRatio => cfg.compaction.stage_ratio.to_string(),
            Self::CompactionKeepTurns => cfg.compaction.keep_turns.to_string(),
            Self::CompactionAnchorRatio => cfg.compaction.anchor_ratio.to_string(),
            Self::UndoKeepPerSession => cfg.undo.keep_per_session.to_string(),
            Self::UndoMaxTreeFiles => cfg.undo.max_tree_files.to_string(),
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
            Self::MemoryHeadingDays => apply_num(
                label,
                raw,
                &mut cfg.memory.heading_days,
                def.memory.heading_days,
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
                &mut cfg.memory.max_proposals_per_turn,
                def.memory.max_proposals_per_turn,
            ),
            Self::DiaryTokenBudget => apply_num(
                label,
                raw,
                &mut cfg.diary.token_budget,
                def.diary.token_budget,
            ),
            Self::DiaryTimeoutSecs => apply_num(
                label,
                raw,
                &mut cfg.diary.timeout_secs,
                def.diary.timeout_secs,
            ),
            Self::DiaryBatchSteps => apply_num(
                label,
                raw,
                &mut cfg.diary.batch_steps,
                def.diary.batch_steps,
            ),
            Self::DiaryBatchMinutes => apply_num(
                label,
                raw,
                &mut cfg.diary.batch_minutes,
                def.diary.batch_minutes,
            ),
            Self::CompactionThreshold => apply_num(
                label,
                raw,
                &mut cfg.compaction.threshold,
                def.compaction.threshold,
            ),
            Self::CompactionStageRatio => apply_num(
                label,
                raw,
                &mut cfg.compaction.stage_ratio,
                def.compaction.stage_ratio,
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
            Self::UndoMaxTreeFiles => apply_num(
                label,
                raw,
                &mut cfg.undo.max_tree_files,
                def.undo.max_tree_files,
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

/// A string list editable from /settings with add/remove rows.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum ListSection {
    SecretsExclude,
    SafetyBlocked,
    SkillsDirs,
}

impl ListSection {
    pub(super) fn title(self) -> &'static str {
        match self {
            Self::SecretsExclude => "secret globs",
            Self::SafetyBlocked => "blocked patterns",
            Self::SkillsDirs => "skill directories",
        }
    }

    pub(super) fn items(self, cfg: &Config) -> Vec<String> {
        match self {
            Self::SecretsExclude => cfg.secrets.exclude_globs.clone(),
            Self::SafetyBlocked => cfg.safety.blocked_patterns.clone(),
            Self::SkillsDirs => cfg
                .skills
                .dirs
                .iter()
                .map(|p| p.display().to_string())
                .collect(),
        }
    }

    pub(super) fn push(self, cfg: &mut Config, value: String) {
        match self {
            Self::SecretsExclude => cfg.secrets.exclude_globs.push(value),
            Self::SafetyBlocked => cfg.safety.blocked_patterns.push(value),
            Self::SkillsDirs => cfg.skills.dirs.push(value.into()),
        }
    }

    pub(super) fn remove(self, cfg: &mut Config, index: usize) -> bool {
        let len = match self {
            Self::SecretsExclude => cfg.secrets.exclude_globs.len(),
            Self::SafetyBlocked => cfg.safety.blocked_patterns.len(),
            Self::SkillsDirs => cfg.skills.dirs.len(),
        };
        if index >= len {
            return false;
        }
        match self {
            Self::SecretsExclude => {
                cfg.secrets.exclude_globs.remove(index);
            }
            Self::SafetyBlocked => {
                cfg.safety.blocked_patterns.remove(index);
            }
            Self::SkillsDirs => {
                cfg.skills.dirs.remove(index);
            }
        };
        true
    }
}

/// Capitalize the first letter for menu titles ("budget ratio" -> "Budget ratio").
fn cap_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

impl App {
    pub(super) fn cur_menu(&self) -> Option<&Menu> {
        self.menu_stack.last()
    }

    pub(super) fn cur_menu_mut(&mut self) -> Option<&mut Menu> {
        self.menu_stack.last_mut()
    }

    pub(super) fn open_graph_view(&mut self) {
        use crate::agent::graph::GraphStore;
        let plan = self.session_plan();
        let focus = plan
            .as_ref()
            .and_then(|p| {
                p.steps.iter().find(|s| {
                    s.status == crate::plan::StepStatus::InProgress
                        || s.status == crate::plan::StepStatus::Pending
                })
            })
            .and_then(|s| s.refs.first())
            .map(|r| {
                if let Some(sym) = &r.symbol {
                    format!("sym:{}::{sym}", r.path)
                } else {
                    format!("file:{}", r.path)
                }
            })
            .or_else(|| {
                crate::agent::graph::SqliteGraphStore::open(&self.project_root)
                    .ok()
                    .and_then(|s| {
                        s.recall("", 1)
                            .ok()
                            .and_then(|items| items.first().map(|i| i.key.clone()))
                    })
            })
            .unwrap_or_else(|| "file:src/main.rs".to_string());

        self.open_menu(Menu::GraphView {
            focus_key: focus,
            trail: Vec::new(),
            depth: 1,
            search_filter: None,
        });
    }

    pub(super) fn open_menu(&mut self, menu: Menu) {
        self.menu_stack.push(menu);
        self.menu_sel = 0;
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
        // confirmation prompts open with the confirm action highlighted
        if matches!(self.cur_menu(), Some(Menu::ConfirmDelete { .. })) && self.menu_sel == 0 {
            self.menu_sel = 1;
        }
        // effort slider opens on the current level, not on `off`
        if matches!(self.cur_menu(), Some(Menu::Effort)) {
            self.menu_sel = EffortLevel::SELECTABLE
                .iter()
                .position(|l| *l == self.model_cfg.effort)
                .unwrap_or(0);
        }
        // unit tests inject the cache directly and never hit the disk
        #[cfg(not(test))]
        if matches!(self.cur_menu(), Some(Menu::Sessions | Menu::DeleteSessions)) {
            // The menu needs metadata only; defer loading full message histories
            // until the user actually opens a session.
            self.sessions = Session::list_visible_headers(40).unwrap_or_default();
        }
        if matches!(self.cur_menu(), Some(Menu::Sessions)) {
            self.sessions_filter.clear();
        }
        self.prefill_form();
        // rows are ready immediately, not on the next frame
        self.build_menu_rows();
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
        self.menu_stack.pop();
        self.menu_sel = 0;
        self.form_fields.clear();
        self.form_focus = 0;
        // a form below the popped one needs its fields back
        self.prefill_form();
        self.build_menu_rows();
        self.dirty = true;
    }

    pub(super) fn menu_home(&mut self) {
        self.menu_stack.clear();
        self.menu_sel = 0;
        self.form_fields.clear();
        self.form_focus = 0;
        self.menu_rows.clear();
        self.dirty = true;
    }

    pub(super) fn menu_scroll_by(&mut self, delta: i32) {
        if self.is_form_menu() {
            return;
        }
        let visible = self.menu_rect.height.saturating_sub(2) as usize;
        let max_scroll = self.menu_rows.len().saturating_sub(visible.max(1));
        let next = if delta < 0 {
            self.menu_scroll
                .saturating_sub(delta.unsigned_abs() as usize)
        } else {
            self.menu_scroll.saturating_add(delta as usize)
        }
        .min(max_scroll);
        // Scrolling into a clamped edge changes nothing: skip the frame.
        if next != self.menu_scroll {
            self.menu_scroll = next;
            self.dirty = true;
        }
    }

    pub(super) fn menu_nav(&mut self, dir: i32) {
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
            let n = self.menu_rows.len();
            if n > 0 {
                let step = (self.menu_rows.len() as i32 / 2).clamp(1, 10) as usize;
                self.menu_sel = if dir < 0 {
                    self.menu_sel.saturating_sub(step)
                } else {
                    (self.menu_sel + step).min(n - 1)
                };
            }
        } else {
            let n = self.menu_rows.len();
            if n > 0 {
                self.menu_sel = if dir < 0 {
                    (self.menu_sel + n - 1) % n
                } else {
                    (self.menu_sel + 1) % n
                };
            }
        }
        self.dirty = true;
    }

    pub(super) fn menu_jump(&mut self, to_end: bool) {
        if self.is_form_menu() {
            self.menu_nav(if to_end { 1 } else { -1 });
            return;
        }
        if !self.menu_rows.is_empty() {
            self.menu_sel = if to_end { self.menu_rows.len() - 1 } else { 0 };
        }
        self.dirty = true;
    }

    pub(super) fn is_form_menu(&self) -> bool {
        matches!(
            self.cur_menu(),
            Some(
                Menu::EditProvider { .. }
                    | Menu::EditModel { .. }
                    | Menu::EditSessionTitle { .. }
                    | Menu::AskFree { .. }
                    | Menu::EditScalar(..)
                    | Menu::AddListItem(..)
                    | Menu::EditMcpServer { .. }
                    | Menu::EditLspServer { .. }
            )
        )
    }

    pub(super) fn menu_hover(&mut self, row: u16) -> Option<usize> {
        // effort slider has its own column-aware hit-testing and hover
        // previews nothing — but only while the slider card is actually
        // drawn (wide terminal). The narrow fallback list hovers by row
        // like every other menu.
        if matches!(self.cur_menu(), Some(Menu::Effort)) && !self.effort_hits.is_empty() {
            return Some(self.menu_sel);
        }
        let r = self.menu_rect;
        // forms highlight nothing; clicks/hover on the frame (borders) or on
        // rows past the last entry must not select anything — otherwise a
        // click on the top border would run the first menu item (rel == 0)
        if self.is_form_menu() || r.height == 0 || row <= r.y || row + 1 >= r.bottom() {
            return None;
        }
        let abs = self.menu_scroll + (row - r.y - 1) as usize;
        if abs >= self.menu_rows.len() {
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
    pub(super) fn effort_index_at(&self, row: u16, col: u16) -> Option<usize> {
        if !matches!(self.cur_menu(), Some(Menu::Effort)) {
            return None;
        }
        self.effort_hits
            .iter()
            .find(|(r, _)| col >= r.x && col < r.right() && row >= r.y && row < r.bottom())
            .map(|(_, idx)| *idx)
    }

    /// id of the session row currently highlighted in the sessions menu
    pub(super) fn selected_session_id(&self) -> Option<String> {
        match &self.menu_rows.get(self.menu_sel)?.1 {
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
            let Some((_, action)) = self.menu_rows.get(sel) else {
                return;
            };
            self.run_action(action.clone());
            return;
        }
        self.menu_activate();
    }

    /// run the confirm action of the open delete-confirmation prompt
    pub(super) fn run_confirm_action(&mut self) {
        let act = self
            .menu_rows
            .iter()
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
        // confirmation prompts: enter always confirms (esc cancels)
        if let Some(Menu::ConfirmDelete { .. }) = self.cur_menu() {
            self.run_confirm_action();
            return;
        }
        // ask_user: enter submits the selected option (or free text)
        if let Some(Menu::AskUser { .. }) = self.cur_menu() {
            if let Some((_, action)) = self.menu_rows.get(self.menu_sel) {
                self.run_action(action.clone());
            }
            return;
        }
        // approval: enter = run once
        if let Some(Menu::Approval { .. }) = self.cur_menu() {
            self.approval_decide(ApprovalDecision::RunOnce);
            return;
        }
        let Some((_, action)) = self.menu_rows.get(self.menu_sel) else {
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
            MenuAction::GraphFocus(new_key) => {
                if let Some(Menu::GraphView {
                    focus_key,
                    trail,
                    search_filter,
                    ..
                }) = self.cur_menu_mut()
                {
                    trail.push(focus_key.clone());
                    *focus_key = new_key;
                    *search_filter = None;
                }
                self.menu_sel = 0;
                self.menu_scroll = 0;
                self.build_menu_rows();
                self.dirty = true;
            }
            MenuAction::GraphBack => {
                if let Some(Menu::GraphView {
                    focus_key,
                    trail,
                    search_filter,
                    ..
                }) = self.cur_menu_mut()
                {
                    if let Some(prev) = trail.pop() {
                        *focus_key = prev;
                        *search_filter = None;
                    }
                }
                self.menu_sel = 0;
                self.menu_scroll = 0;
                self.build_menu_rows();
                self.dirty = true;
            }
            MenuAction::GraphDepth(delta) => {
                if let Some(Menu::GraphView { depth, .. }) = self.cur_menu_mut() {
                    if delta > 0 && *depth < 3 {
                        *depth += 1;
                    } else if delta < 0 && *depth > 1 {
                        *depth -= 1;
                    }
                }
                self.menu_sel = 0;
                self.menu_scroll = 0;
                self.build_menu_rows();
                self.dirty = true;
            }
            MenuAction::OpenAppearance => self.open_menu(Menu::Appearance),
            MenuAction::OpenProviders => self.open_menu(Menu::Providers),
            MenuAction::OpenMcp => self.open_menu(Menu::Mcp),
            MenuAction::OpenLsp => self.open_menu(Menu::Lsp),
            MenuAction::OpenSkills => self.open_menu(Menu::Skills),
            MenuAction::OpenDebug => self.open_menu(Menu::Debug),
            MenuAction::OpenAgent => self.open_menu(Menu::Agent),
            MenuAction::OpenSafety => self.open_menu(Menu::Safety),
            MenuAction::OpenUndo => self.open_menu(Menu::Undo),
            MenuAction::OpenPickDefaultModel => self.open_menu(Menu::PickDefaultModel),
            MenuAction::OpenControls => self.open_menu(Menu::Controls),
            MenuAction::SetDefaultModel(key) => {
                if !self.cfg.models.contains_key(&key) {
                    self.status(&format!("unknown model '{key}'"), StatusKind::Err);
                    return;
                }
                self.cfg.default_model = key.clone();
                self.cfg.save().ok();
                self.status(
                    &format!("default model = {key} (applies to new sessions)"),
                    StatusKind::Ok,
                );
                self.build_menu_rows();
            }
            MenuAction::CycleDiaryEffort => {
                let cur = EffortLevel::ALL
                    .iter()
                    .position(|l| *l == self.cfg.diary.effort)
                    .unwrap_or(0);
                self.cfg.diary.effort = EffortLevel::ALL[(cur + 1) % EffortLevel::ALL.len()];
                self.cfg.save().ok();
                self.status(
                    &format!("diary effort = {}", self.cfg.diary.effort.as_str()),
                    StatusKind::Ok,
                );
                self.build_menu_rows();
            }
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
            MenuAction::ToggleSkillsAutoLoad => {
                self.cfg.skills.auto_load = !self.cfg.skills.auto_load;
                self.cfg.save().ok();
                let on = self.cfg.skills.auto_load;
                self.status(&format!("skills auto-load: {}", on_off(on)), StatusKind::Ok);
                self.build_menu_rows();
            }
            MenuAction::AddMcpServer => self.open_menu(Menu::EditMcpServer { index: None }),
            MenuAction::EditMcpServer(index) => {
                self.open_menu(Menu::EditMcpServer { index: Some(index) })
            }
            MenuAction::OpenMcpServer(index) => self.open_menu(Menu::McpServer { index }),
            MenuAction::ToggleMcpServer(index) => {
                let Some(server) = self.cfg.mcp.servers.get_mut(index) else {
                    self.status("unknown MCP server", StatusKind::Err);
                    return;
                };
                server.enabled = !server.enabled;
                let state = if server.enabled {
                    "enabled"
                } else {
                    "disabled"
                };
                let name = server.name.clone();
                self.cfg.save().ok();
                self.status(&format!("MCP server '{name}' {state}"), StatusKind::Ok);
                self.build_menu_rows();
            }
            MenuAction::DeleteMcpServerList => self.open_menu(Menu::DeleteMcpServer),
            MenuAction::DeleteMcpServer(index) => {
                let name = self
                    .cfg
                    .mcp
                    .servers
                    .get(index)
                    .map(|s| s.name.clone())
                    .unwrap_or_default();
                self.open_menu(Menu::ConfirmDelete {
                    label: format!("delete MCP server '{name}'?"),
                    action: MenuAction::DeleteMcpServer(index),
                });
            }
            MenuAction::AddLspServer => self.open_menu(Menu::EditLspServer { index: None }),
            MenuAction::EditLspServer(index) => {
                self.open_menu(Menu::EditLspServer { index: Some(index) })
            }
            MenuAction::OpenLspServer(index) => self.open_menu(Menu::LspServer { index }),
            MenuAction::ToggleLspServer(index) => {
                let Some(server) = self.cfg.lsp.servers.get_mut(index) else {
                    self.status("unknown LSP server", StatusKind::Err);
                    return;
                };
                server.enabled = !server.enabled;
                let state = if server.enabled {
                    "enabled"
                } else {
                    "disabled"
                };
                let name = server.name.clone();
                self.cfg.save().ok();
                self.status(&format!("LSP server '{name}' {state}"), StatusKind::Ok);
                self.build_menu_rows();
            }
            MenuAction::DeleteLspServerList => self.open_menu(Menu::DeleteLspServer),
            MenuAction::DeleteLspServer(index) => {
                let name = self
                    .cfg
                    .lsp
                    .servers
                    .get(index)
                    .map(|s| s.name.clone())
                    .unwrap_or_default();
                self.open_menu(Menu::ConfirmDelete {
                    label: format!("delete LSP server '{name}'?"),
                    action: MenuAction::DeleteLspServer(index),
                });
            }
            MenuAction::AddListItem(section) => self.open_menu(Menu::AddListItem(section)),
            MenuAction::DeleteListItems(section) => self.open_menu(Menu::DeleteListItems(section)),
            MenuAction::DeleteListItem(section, index) => {
                let item = section
                    .items(&self.cfg)
                    .get(index)
                    .cloned()
                    .unwrap_or_default();
                self.open_menu(Menu::ConfirmDelete {
                    label: format!("delete {} '{item}'?", section.title()),
                    action: MenuAction::DeleteListItem(section, index),
                });
            }
            MenuAction::EditScalar(setting) => self.open_menu(Menu::EditScalar(setting)),
            MenuAction::OpenModels(p) => self.open_menu(Menu::Models { provider: p }),
            MenuAction::AddProvider => self.open_menu(Menu::EditProvider { name: None }),
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
            MenuAction::EditModel(p, k) => {
                if self.cfg.is_builtin_model(&k) {
                    self.status("built-in model cannot be modified", StatusKind::Warn);
                    return;
                }
                self.open_menu(Menu::EditModel {
                    provider: p,
                    key: Some(k),
                });
            }
            MenuAction::DeleteModel(p, k) => {
                if self.cfg.is_builtin_model(&k) {
                    self.status("built-in model cannot be deleted", StatusKind::Warn);
                    return;
                }
                self.open_menu(Menu::ConfirmDelete {
                    label: format!("delete model '{k}'?"),
                    action: MenuAction::DeleteModel(p, k),
                });
            }
            MenuAction::PickModelList(p) => self.open_menu(Menu::PickModel { provider: p }),
            MenuAction::DeleteModelList(p) => self.open_menu(Menu::DeleteModelList { provider: p }),
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
            MenuAction::NewSession => {
                self.start_new_session();
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
            MenuAction::DeleteSessionList => {
                self.open_menu(Menu::DeleteSessions);
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
            MenuAction::TogglePerfLog => {
                // per-frame transcript timings for lag hunting: one line per
                // drawn frame plus tool markers, into a fresh temp file
                let on = !self.perf.enabled();
                let renders = self.test_renders;
                let wraps = crate::tui::markdown::WRAP_TAGGED_CALLS
                    .load(std::sync::atomic::Ordering::Relaxed);
                match self.perf.set_enabled(on, renders, wraps) {
                    Ok(msg) => self.status(&msg, StatusKind::Ok),
                    Err(msg) => self.status(&msg, StatusKind::Err),
                }
                self.build_menu_rows();
            }
            MenuAction::ToggleShowCost => {
                self.cfg.ui.show_cost = !self.cfg.ui.show_cost;
                self.cfg.save().ok();
                let on = self.cfg.ui.show_cost;
                self.status(&format!("show cost: {}", on_off(on)), StatusKind::Ok);
                self.build_menu_rows();
            }
            MenuAction::CycleModelEffort => {
                let all = EffortLevel::ALL;
                let cur = all
                    .iter()
                    .position(|l| *l == self.model_cfg.effort)
                    .unwrap_or(0);
                let next = all[(cur + 1) % all.len()];
                self.model_cfg.effort = next;
                if let Some(m) = self.cfg.models.get_mut(&self.session.model_key) {
                    m.effort = next;
                }
                self.cfg.save().ok();
                self.build_menu_rows();
            }
            MenuAction::CycleEffortControl => {
                // `None` means "derive from the wire format", and it leads the
                // cycle: the declaration is an override, not a requirement.
                let cycle: Vec<Option<crate::config::EffortControl>> = std::iter::once(None)
                    .chain(crate::config::EffortControl::ALL.into_iter().map(Some))
                    .collect();
                let cur = cycle
                    .iter()
                    .position(|c| *c == self.model_cfg.effort_control)
                    .unwrap_or(0);
                let next = cycle[(cur + 1) % cycle.len()];
                self.model_cfg.effort_control = next;
                if let Some(m) = self.cfg.models.get_mut(&self.session.model_key) {
                    m.effort_control = next;
                }
                self.cfg.save().ok();
                let plan = self.effort_plan();
                self.status(&plan.label(), StatusKind::Ok);
                self.build_menu_rows();
            }
            MenuAction::ToggleEffortAlwaysOn => {
                let next = !self.model_cfg.effort_always_on;
                self.model_cfg.effort_always_on = next;
                if let Some(m) = self.cfg.models.get_mut(&self.session.model_key) {
                    m.effort_always_on = next;
                }
                self.cfg.save().ok();
                let plan = self.effort_plan();
                self.status(&plan.label(), StatusKind::Ok);
                self.build_menu_rows();
            }
            MenuAction::CycleDefaultEffort => {
                let all = EffortLevel::ALL;
                let cur = all
                    .iter()
                    .position(|l| *l == self.cfg.default_effort)
                    .unwrap_or(0);
                self.cfg.default_effort = all[(cur + 1) % all.len()];
                self.cfg.save().ok();
                self.build_menu_rows();
            }
            MenuAction::ToggleMode => {
                self.mode = self.mode.toggle();
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
                        self.status("built-in model cannot be deleted", StatusKind::Warn);
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
                if let MenuAction::DeleteMcpServer(index) = &inner {
                    if *index < self.cfg.mcp.servers.len() {
                        let removed = self.cfg.mcp.servers.remove(*index);
                        self.cfg.save().ok();
                        self.status(
                            &format!("MCP server '{}' deleted", removed.name),
                            StatusKind::Ok,
                        );
                    } else {
                        self.status("unknown MCP server", StatusKind::Err);
                    }
                }
                if let MenuAction::DeleteLspServer(index) = &inner {
                    if *index < self.cfg.lsp.servers.len() {
                        let removed = self.cfg.lsp.servers.remove(*index);
                        self.cfg.save().ok();
                        self.status(
                            &format!("LSP server '{}' deleted", removed.name),
                            StatusKind::Ok,
                        );
                    } else {
                        self.status("unknown LSP server", StatusKind::Err);
                    }
                }
                if let MenuAction::DeleteListItem(section, index) = &inner {
                    if section.remove(&mut self.cfg, *index) {
                        self.cfg.save().ok();
                        self.status(
                            &format!("{} entry deleted", section.title()),
                            StatusKind::Ok,
                        );
                    } else {
                        self.status("unknown entry", StatusKind::Err);
                    }
                }
                if matches!(&inner, MenuAction::DeletePlan) {
                    // close the prompt first: with a menu open status()
                    // writes to menu_status, which nothing renders once the
                    // menu is gone — the outcome must land in chat segments
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
                                    // Defect B: the fallback below can surface
                                    // another session's stale active plan — say
                                    // so explicitly instead of silently
                                    // switching the session onto it.
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
                                            if next.sessions.iter().any(|s| s == &sid) {
                                                format!(
                                                    "plan deleted; now following active plan '{goal}' (see /plan)"
                                                )
                                            } else {
                                                format!(
                                                    "plan deleted; now following active plan '{goal}' — it belongs to another session (see /plan)"
                                                )
                                            }
                                        }
                                        None => "plan deleted; no active plan".to_string(),
                                    };
                                    self.status(&note, StatusKind::Ok);
                                    self.refresh_plan_label();
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
                            self.refresh_plan_label();
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
                        self.open_menu(Menu::Models {
                            provider: p.clone(),
                        });
                    }
                    // session gone: back to the sessions list
                    MenuAction::DeleteSession(_) => {
                        self.menu_stack.pop();
                        self.open_menu(Menu::Sessions);
                    }
                    // server gone: back to a fresh server list
                    MenuAction::DeleteMcpServer(_) => {
                        self.menu_home();
                        self.open_menu(Menu::Mcp);
                    }
                    MenuAction::DeleteLspServer(_) => {
                        self.menu_home();
                        self.open_menu(Menu::Lsp);
                    }
                    // entry gone: back to the delete list (rebuilt) for the next one
                    MenuAction::DeleteListItem(_, _) => {
                        self.menu_back();
                    }
                    _ => {}
                }
            }
            MenuAction::DeletePlan => {
                self.run_action(MenuAction::Confirm(Box::new(MenuAction::DeletePlan)));
            }
            MenuAction::UpdateBuiltins => {
                self.start_builtin_update(true);
            }
            MenuAction::OpenSubagent(id) => {
                self.menu_home();
                self.open_subagent_view(id);
            }
            MenuAction::SetEffort(level) => {
                self.model_cfg.effort = level;
                if let Some(m) = self.cfg.models.get_mut(&self.session.model_key) {
                    m.effort = level;
                }
                self.cfg.save().ok();
                self.menu_home();
                let plan = self.effort_plan();
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
            Some(Menu::Settings) => " Settings ".into(),
            Some(Menu::Appearance) => " Appearance ".into(),
            Some(Menu::Mcp) => " MCP servers ".into(),
            Some(Menu::Lsp) => " LSP servers ".into(),
            Some(Menu::Skills) => " Skills ".into(),
            Some(Menu::Providers) => " Providers ".into(),
            Some(Menu::Models { provider }) => format!(" Models · {provider} "),
            Some(Menu::PickModel { provider }) => format!(" Switch model · {provider} "),
            Some(Menu::DeleteModelList { provider }) => {
                format!(" Delete model · {provider} ")
            }
            Some(Menu::EditProvider { name }) => match name {
                Some(n) => format!(" Edit provider: {n} "),
                None => " New provider ".into(),
            },
            Some(Menu::EditModel { provider, .. }) => {
                format!(" Model · {provider} ")
            }
            Some(Menu::Sessions) => {
                if self.sessions_filter.is_empty() {
                    " Sessions ".into()
                } else {
                    format!(" Sessions · filter '{}' ", self.sessions_filter)
                }
            }
            Some(Menu::DeleteSessions) => " Delete session ".into(),
            Some(Menu::Debug) => " Debug ".into(),
            Some(Menu::Agent) => " Agent ".into(),
            Some(Menu::Safety) => " Safety ".into(),
            Some(Menu::Undo) => " Undo ".into(),
            Some(Menu::PickDefaultModel) => " Default model ".into(),
            Some(Menu::Help) => " Help ".into(),
            Some(Menu::Controls) => " Controls ".into(),
            Some(Menu::EditScalar(setting)) => format!(" Edit {} ", cap_first(setting.label())),
            Some(Menu::AddListItem(section)) => format!(" Add {} ", cap_first(section.title())),
            Some(Menu::DeleteListItems(section)) => {
                format!(" Delete {} ", cap_first(section.title()))
            }
            Some(Menu::EditMcpServer { index }) => match index {
                Some(_) => " Edit MCP server ".into(),
                None => " New MCP server ".into(),
            },
            Some(Menu::McpServer { .. }) => " MCP server ".into(),
            Some(Menu::DeleteMcpServer) => " Delete MCP server ".into(),
            Some(Menu::EditLspServer { index }) => match index {
                Some(_) => " Edit LSP server ".into(),
                None => " New LSP server ".into(),
            },
            Some(Menu::LspServer { .. }) => " LSP server ".into(),
            Some(Menu::DeleteLspServer) => " Delete LSP server ".into(),
            Some(Menu::EditSessionTitle { .. }) => " Rename session ".into(),
            Some(Menu::ConfirmDelete { .. }) => " Confirm ".into(),
            Some(Menu::Effort) => " Effort ".into(),
            Some(Menu::AskUser { .. }) => " Ask ".into(),
            Some(Menu::Approval { .. }) => " Confirm command ".into(),
            Some(Menu::AskFree { .. }) => " Answer ".into(),
            Some(Menu::Todo) => " To-do ".into(),
            Some(Menu::Plan) => " Plan ".into(),
            Some(Menu::PlanPreview { .. }) => " Proposed plan ".into(),
            Some(Menu::Subagents) => " Subagents ".into(),
            Some(Menu::GraphView { .. }) => " Graph ".into(),
            None => String::new(),
        }
    }

    pub(super) fn build_menu_rows(&mut self) {
        self.menu_rows.clear();
        self.menu_footer_text = None;
        let Some(menu) = self.cur_menu().cloned() else {
            return;
        };
        let row = |l: Line<'static>, a: MenuAction| (l, a);
        match menu {
            Menu::Settings => {
                let section = |label: &str, action: MenuAction| {
                    row(
                        Line::from(vec![Span::styled(
                            format!("  {label}"),
                            Theme::FG(),
                        )]),
                        action,
                    )
                };
                self.menu_rows
                    .push(section("Appearance", MenuAction::OpenAppearance));
                self.menu_rows
                    .push(section("Providers", MenuAction::OpenProviders));
                self.menu_rows.push(section("Agent", MenuAction::OpenAgent));
                self.menu_rows
                    .push(section("Safety", MenuAction::OpenSafety));
                self.menu_rows.push(section("Undo", MenuAction::OpenUndo));
                self.menu_rows.push(section("MCP", MenuAction::OpenMcp));
                self.menu_rows.push(section("LSP", MenuAction::OpenLsp));
                self.menu_rows
                    .push(section("Skills", MenuAction::OpenSkills));
                self.menu_rows.push(section("Debug", MenuAction::OpenDebug));
                self.menu_footer_text = Some("enter: open · esc: close".into());
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
                    ("ctrl+b", "subagents"),
                    ("ctrl+p", "providers"),
                    ("ctrl+l", "plan"),
                    ("ctrl+o", "settings"),
                    ("ctrl+e", "effort"),
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
            Menu::Mcp => {
                self.menu_rows
                    .push(row(Line::from("  MCP servers"), MenuAction::None));
                if self.cfg.mcp.servers.is_empty() {
                    self.menu_rows
                        .push(row(Line::from("  no servers configured"), MenuAction::None));
                } else {
                    for (i, server) in self.cfg.mcp.servers.iter().enumerate() {
                        self.menu_rows.push(row(
                            Line::from(format!(
                                "  {:<20} {}",
                                server.name,
                                if server.enabled {
                                    "enabled"
                                } else {
                                    "disabled"
                                }
                            )),
                            MenuAction::OpenMcpServer(i),
                        ));
                    }
                }
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(
                        " + add server".to_string(),
                        Theme::ACCENT_SOFT(),
                    )]),
                    MenuAction::AddMcpServer,
                ));
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(
                        " · delete server".to_string(),
                        Theme::ERR(),
                    )]),
                    MenuAction::DeleteMcpServerList,
                ));
                self.menu_footer_text = Some("enter: open · esc: back".into());
            }
            Menu::McpServer { index } => {
                let name = self
                    .cfg
                    .mcp
                    .servers
                    .get(index)
                    .map(|s| s.name.clone())
                    .unwrap_or_default();
                let enabled = self.cfg.mcp.servers.get(index).is_some_and(|s| s.enabled);
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(" edit".to_string(), Theme::FG())]),
                    MenuAction::EditMcpServer(index),
                ));
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(
                        if enabled { " disable" } else { " enable" }.to_string(),
                        Theme::FG(),
                    )]),
                    MenuAction::ToggleMcpServer(index),
                ));
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(" delete", Theme::ERR())]),
                    MenuAction::DeleteMcpServer(index),
                ));
                self.menu_footer_text = Some(format!(" mcp server: {name} · esc: back "));
            }
            Menu::DeleteMcpServer => {
                for (i, server) in self.cfg.mcp.servers.iter().enumerate() {
                    self.menu_rows.push(row(
                        Line::from(vec![
                            Span::styled(format!(" {}", server.name), Theme::ERR()),
                            Span::styled(
                                format!(
                                    "  {}",
                                    if server.enabled {
                                        "enabled"
                                    } else {
                                        "disabled"
                                    }
                                ),
                                Theme::dim(),
                            ),
                        ]),
                        MenuAction::DeleteMcpServer(i),
                    ));
                }
                self.menu_footer_text = Some("enter: delete · esc: back".into());
            }
            Menu::Lsp => {
                self.menu_rows
                    .push(row(Line::from("  LSP servers"), MenuAction::None));
                if self.cfg.lsp.servers.is_empty() {
                    self.menu_rows
                        .push(row(Line::from("  no servers configured"), MenuAction::None));
                } else {
                    for (i, server) in self.cfg.lsp.servers.iter().enumerate() {
                        self.menu_rows.push(row(
                            Line::from(format!(
                                "  {:<20} {}",
                                server.name,
                                if server.enabled {
                                    "enabled"
                                } else {
                                    "disabled"
                                }
                            )),
                            MenuAction::OpenLspServer(i),
                        ));
                    }
                }
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(
                        " + add server".to_string(),
                        Theme::ACCENT_SOFT(),
                    )]),
                    MenuAction::AddLspServer,
                ));
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(
                        " · delete server".to_string(),
                        Theme::ERR(),
                    )]),
                    MenuAction::DeleteLspServerList,
                ));
                self.menu_footer_text = Some("enter: open · esc: back".into());
            }
            Menu::LspServer { index } => {
                let name = self
                    .cfg
                    .lsp
                    .servers
                    .get(index)
                    .map(|s| s.name.clone())
                    .unwrap_or_default();
                let enabled = self.cfg.lsp.servers.get(index).is_some_and(|s| s.enabled);
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(" edit".to_string(), Theme::FG())]),
                    MenuAction::EditLspServer(index),
                ));
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(
                        if enabled { " disable" } else { " enable" }.to_string(),
                        Theme::FG(),
                    )]),
                    MenuAction::ToggleLspServer(index),
                ));
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(" delete", Theme::ERR())]),
                    MenuAction::DeleteLspServer(index),
                ));
                self.menu_footer_text = Some(format!(" lsp server: {name} · esc: back "));
            }
            Menu::DeleteLspServer => {
                for (i, server) in self.cfg.lsp.servers.iter().enumerate() {
                    self.menu_rows.push(row(
                        Line::from(vec![
                            Span::styled(format!(" {}", server.name), Theme::ERR()),
                            Span::styled(
                                format!(
                                    "  {}",
                                    if server.enabled {
                                        "enabled"
                                    } else {
                                        "disabled"
                                    }
                                ),
                                Theme::dim(),
                            ),
                        ]),
                        MenuAction::DeleteLspServer(i),
                    ));
                }
                self.menu_footer_text = Some("enter: delete · esc: back".into());
            }
            Menu::Skills => {
                self.menu_rows.push(row(
                    Line::from(vec![
                        Span::styled(format!("  {:<18}", "auto-load"), Theme::FG()),
                        Span::styled(on_off(self.cfg.skills.auto_load), Theme::dim()),
                    ]),
                    MenuAction::ToggleSkillsAutoLoad,
                ));
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(
                        "  Directories".to_string(),
                        Theme::dim(),
                    )]),
                    MenuAction::None,
                ));
                if self.cfg.skills.dirs.is_empty() {
                    self.menu_rows
                        .push(row(Line::from("  (none)"), MenuAction::None));
                }
                for dir in &self.cfg.skills.dirs {
                    self.menu_rows.push(row(
                        Line::from(format!("  {}", dir.display())),
                        MenuAction::None,
                    ));
                }
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(
                        " + add directory".to_string(),
                        Theme::ACCENT_SOFT(),
                    )]),
                    MenuAction::AddListItem(ListSection::SkillsDirs),
                ));
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(
                        " · delete directory".to_string(),
                        Theme::ERR(),
                    )]),
                    MenuAction::DeleteListItems(ListSection::SkillsDirs),
                ));
                let root = std::env::current_dir().unwrap_or_default();
                let loaded = crate::prompts::skills::load(&self.cfg.skills, &root);
                self.menu_rows
                    .push(row(Line::from("  Loaded skills"), MenuAction::None));
                if loaded.is_empty() {
                    self.menu_rows
                        .push(row(Line::from("  no skills found"), MenuAction::None));
                } else {
                    for skill in loaded {
                        self.menu_rows.push(row(
                            Line::from(format!("  {}", skill.name)),
                            MenuAction::None,
                        ));
                    }
                }
                self.menu_footer_text = Some("/skill to activate · esc: back".into());
            }

            Menu::Appearance => {
                let setting = |label: &str, detail: &str, action: MenuAction| {
                    row(
                        Line::from(vec![
                            Span::styled(format!("  {label:<16}"), Theme::FG()),
                            Span::styled(detail.to_string(), Theme::dim()),
                        ]),
                        action,
                    )
                };
                self.menu_rows.push(setting(
                    "Typewriter",
                    on_off(self.cfg.ui.typewriter).as_str(),
                    MenuAction::ToggleTypewriter,
                ));
                self.menu_rows.push(setting(
                    "Show cost",
                    on_off(self.cfg.ui.show_cost).as_str(),
                    MenuAction::ToggleShowCost,
                ));
                self.menu_footer_text = Some("enter: open/toggle · esc: back".into());
            }
            Menu::Agent => {
                let header = |t: &str| {
                    row(
                        Line::from(vec![Span::styled(format!("  {t}"), Theme::dim())]),
                        MenuAction::None,
                    )
                };
                let scalar = |label: &str, val: String, s: ScalarSetting| {
                    row(
                        Line::from(vec![
                            Span::styled(format!("  {label:<18}"), Theme::FG()),
                            Span::styled(val, Theme::dim()),
                        ]),
                        MenuAction::EditScalar(s),
                    )
                };
                self.menu_rows.push(header("plan"));
                self.menu_rows.push(scalar(
                    "budget ratio",
                    ScalarSetting::PlanBudgetRatio.current(&self.cfg),
                    ScalarSetting::PlanBudgetRatio,
                ));
                self.menu_rows.push(scalar(
                    "max steps",
                    ScalarSetting::PlanMaxSteps.current(&self.cfg),
                    ScalarSetting::PlanMaxSteps,
                ));
                self.menu_rows.push(scalar(
                    "nudge after",
                    ScalarSetting::PlanNudgeAfter.current(&self.cfg),
                    ScalarSetting::PlanNudgeAfter,
                ));
                self.menu_rows.push(header("memory"));
                self.menu_rows.push(scalar(
                    "load budget ratio",
                    ScalarSetting::MemoryLoadBudgetRatio.current(&self.cfg),
                    ScalarSetting::MemoryLoadBudgetRatio,
                ));
                self.menu_rows.push(scalar(
                    "heading days",
                    ScalarSetting::MemoryHeadingDays.current(&self.cfg),
                    ScalarSetting::MemoryHeadingDays,
                ));
                self.menu_rows.push(scalar(
                    "max tokens",
                    ScalarSetting::MemoryMaxTokens.current(&self.cfg),
                    ScalarSetting::MemoryMaxTokens,
                ));
                self.menu_rows.push(scalar(
                    "max proposals",
                    ScalarSetting::MemoryMaxProposals.current(&self.cfg),
                    ScalarSetting::MemoryMaxProposals,
                ));
                self.menu_rows.push(header("diary"));
                self.menu_rows.push(scalar(
                    "token budget",
                    ScalarSetting::DiaryTokenBudget.current(&self.cfg),
                    ScalarSetting::DiaryTokenBudget,
                ));
                self.menu_rows.push(row(
                    Line::from(vec![
                        Span::styled(format!("  {:<18}", "effort"), Theme::FG()),
                        Span::styled(self.cfg.diary.effort.as_str().to_string(), Theme::dim()),
                    ]),
                    MenuAction::CycleDiaryEffort,
                ));
                self.menu_rows.push(scalar(
                    "timeout secs",
                    ScalarSetting::DiaryTimeoutSecs.current(&self.cfg),
                    ScalarSetting::DiaryTimeoutSecs,
                ));
                self.menu_rows.push(scalar(
                    "batch steps",
                    ScalarSetting::DiaryBatchSteps.current(&self.cfg),
                    ScalarSetting::DiaryBatchSteps,
                ));
                self.menu_rows.push(scalar(
                    "batch minutes",
                    ScalarSetting::DiaryBatchMinutes.current(&self.cfg),
                    ScalarSetting::DiaryBatchMinutes,
                ));
                self.menu_rows.push(header("compaction"));
                self.menu_rows.push(scalar(
                    "threshold",
                    ScalarSetting::CompactionThreshold.current(&self.cfg),
                    ScalarSetting::CompactionThreshold,
                ));
                self.menu_rows.push(scalar(
                    "stage ratio",
                    ScalarSetting::CompactionStageRatio.current(&self.cfg),
                    ScalarSetting::CompactionStageRatio,
                ));
                self.menu_rows.push(scalar(
                    "keep turns",
                    ScalarSetting::CompactionKeepTurns.current(&self.cfg),
                    ScalarSetting::CompactionKeepTurns,
                ));
                self.menu_rows.push(scalar(
                    "anchor ratio",
                    ScalarSetting::CompactionAnchorRatio.current(&self.cfg),
                    ScalarSetting::CompactionAnchorRatio,
                ));
                self.menu_rows.push(row(
                    Line::from(vec![
                        Span::styled(format!("  {:<18}", "summary"), Theme::FG()),
                        Span::styled(
                            self.cfg.compaction.summary.as_str().to_string(),
                            Theme::dim(),
                        ),
                    ]),
                    MenuAction::CycleCompactionSummary,
                ));
                self.menu_footer_text = Some("enter: edit/cycle · esc: back".into());
            }
            Menu::Safety => {
                let header = |t: &str| {
                    row(
                        Line::from(vec![Span::styled(format!("  {t}"), Theme::dim())]),
                        MenuAction::None,
                    )
                };
                for section in [ListSection::SecretsExclude, ListSection::SafetyBlocked] {
                    self.menu_rows.push(header(section.title()));
                    let items = section.items(&self.cfg);
                    if items.is_empty() {
                        self.menu_rows
                            .push(row(Line::from("  (empty)"), MenuAction::None));
                    }
                    for item in items {
                        self.menu_rows
                            .push(row(Line::from(format!("  {item}")), MenuAction::None));
                    }
                    self.menu_rows.push(row(
                        Line::from(vec![Span::styled(
                            " + add".to_string(),
                            Theme::ACCENT_SOFT(),
                        )]),
                        MenuAction::AddListItem(section),
                    ));
                    self.menu_rows.push(row(
                        Line::from(vec![Span::styled(" · delete".to_string(), Theme::ERR())]),
                        MenuAction::DeleteListItems(section),
                    ));
                }
                self.menu_footer_text = Some("enter: open · esc: back".into());
            }
            Menu::Undo => {
                let scalar = |label: &str, val: String, s: ScalarSetting| {
                    row(
                        Line::from(vec![
                            Span::styled(format!("  {label:<18}"), Theme::FG()),
                            Span::styled(val, Theme::dim()),
                        ]),
                        MenuAction::EditScalar(s),
                    )
                };
                self.menu_rows.push(scalar(
                    "keep per session",
                    ScalarSetting::UndoKeepPerSession.current(&self.cfg),
                    ScalarSetting::UndoKeepPerSession,
                ));
                self.menu_rows.push(scalar(
                    "max tree files",
                    ScalarSetting::UndoMaxTreeFiles.current(&self.cfg),
                    ScalarSetting::UndoMaxTreeFiles,
                ));
                self.menu_rows.push(scalar(
                    "blob grace secs",
                    ScalarSetting::UndoBlobGraceSecs.current(&self.cfg),
                    ScalarSetting::UndoBlobGraceSecs,
                ));
                self.menu_rows.push(row(
                    Line::from(vec![
                        Span::styled(format!("  {:<18}", "shadow"), Theme::FG()),
                        Span::styled(self.cfg.undo.shadow.as_str().to_string(), Theme::dim()),
                    ]),
                    MenuAction::CycleUndoShadow,
                ));
                self.menu_rows.push(scalar(
                    "shadow max bytes",
                    ScalarSetting::UndoShadowMaxBytes.current(&self.cfg),
                    ScalarSetting::UndoShadowMaxBytes,
                ));
                self.menu_footer_text = Some("enter: edit/cycle · esc: back".into());
            }
            Menu::PickDefaultModel => {
                for (k, m) in &self.cfg.models {
                    let mark = if *k == self.cfg.default_model {
                        " *default"
                    } else {
                        ""
                    };
                    self.menu_rows.push(row(
                        Line::from(vec![
                            Span::styled(format!(" {k}{mark}"), Theme::FG()),
                            Span::styled(format!("  {}", m.provider), Theme::dim()),
                        ]),
                        MenuAction::SetDefaultModel(k.clone()),
                    ));
                }
                self.menu_footer_text = Some("enter: set default · esc: back".into());
            }
            Menu::DeleteListItems(section) => {
                for (i, item) in section.items(&self.cfg).iter().enumerate() {
                    self.menu_rows.push(row(
                        Line::from(vec![
                            Span::styled(format!(" {item}"), Theme::ERR()),
                            Span::styled(format!("  {}", section.title()), Theme::dim()),
                        ]),
                        MenuAction::DeleteListItem(section, i),
                    ));
                }
                self.menu_footer_text = Some("enter: delete · esc: back".into());
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
                    MenuAction::CycleModelEffort,
                ));
                self.menu_rows.push(setting(
                    "effort control",
                    match self.model_cfg.effort_control {
                        Some(c) => c.as_str().to_string(),
                        // show what it resolved to, so "auto" is not a mystery
                        None => format!("auto ({})", self.effort_support().control.as_str()),
                    },
                    MenuAction::CycleEffortControl,
                ));
                self.menu_rows.push(setting(
                    "always reasons",
                    on_off(self.model_cfg.effort_always_on),
                    MenuAction::ToggleEffortAlwaysOn,
                ));
                self.menu_rows.push(setting(
                    "default effort",
                    self.cfg.default_effort.as_str().to_string(),
                    MenuAction::CycleDefaultEffort,
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
                let q = self.sessions_filter.to_lowercase();
                let visible: Vec<&SessionHeader> = self
                    .sessions
                    .iter()
                    .filter(|s| {
                        q.is_empty()
                            || s.title.to_lowercase().contains(&q)
                            || s.model_key.to_lowercase().contains(&q)
                    })
                    .collect();
                if !self.startup {
                    self.menu_rows.push(row(
                        Line::from(vec![Span::styled(" + new session", Theme::ACCENT_SOFT())]),
                        MenuAction::NewSession,
                    ));
                }
                let cur_id = self.session.id.to_string();
                let pinned: Vec<&SessionHeader> =
                    visible.iter().filter(|s| s.pinned).copied().collect();
                if !pinned.is_empty() {
                    let menu_w = if self.menu_rect.width > 0 {
                        self.menu_rect.width
                    } else {
                        78.min(self.cache_w.saturating_sub(4)).max(30)
                    };
                    let frame_w = (menu_w as usize).saturating_sub(4).clamp(24, 72);
                    let head = " pinned ";
                    let mid = {
                        let label = format!(" {head} ");
                        let fill = frame_w.saturating_sub(super::view::cols(&label));
                        format!(
                            "{}{}{}",
                            "─".repeat(fill / 2),
                            label,
                            "─".repeat(fill - fill / 2)
                        )
                    };
                    self.menu_rows.push(row(
                        Line::from(vec![Span::styled(format!("┌{mid}┐"), Theme::rule_color())]),
                        MenuAction::None,
                    ));
                    for s in &pinned {
                        self.menu_rows.push(session_row(
                            s,
                            s.id.to_string() == cur_id,
                            Some(frame_w),
                        ));
                    }
                    self.menu_rows.push(row(
                        Line::from(vec![Span::styled(
                            format!("└{}┘", "─".repeat(frame_w)),
                            Theme::rule_color(),
                        )]),
                        MenuAction::None,
                    ));
                }
                for s in &visible {
                    if s.pinned {
                        continue;
                    }
                    self.menu_rows
                        .push(session_row(s, s.id.to_string() == cur_id, None));
                }
                if visible.is_empty() {
                    let note = if q.is_empty() {
                        " (no saved sessions yet)"
                    } else {
                        " (no matches)"
                    };
                    self.menu_rows.push(row(
                        Line::from(vec![Span::styled(note.to_string(), Theme::dim())]),
                        MenuAction::None,
                    ));
                }
                if self.sessions_filter.is_empty() {
                    self.menu_rows.push(row(
                        Line::from(Span::styled(" p: pin · d: delete", Theme::dim())),
                        MenuAction::None,
                    ));
                }
                self.menu_footer_text = Some(if self.sessions_filter.is_empty() {
                    "enter: open · r: rename · p: pin · d: delete · type to filter".into()
                } else {
                    "type: filter · backspace: erase · esc: clear filter".into()
                });
            }
            Menu::DeleteSessions => {
                for (i, s) in self.sessions.iter().enumerate() {
                    let date = fmt_date(s.last_activity());
                    self.menu_rows.push(row(
                        Line::from(vec![
                            Span::styled(
                                format!(" {}", truncate_chars(&s.title, 40)),
                                Theme::ERR(),
                            ),
                            Span::styled(
                                format!("  {date} · {}", truncate_chars(&s.model_key, 16)),
                                Theme::dim(),
                            ),
                        ]),
                        MenuAction::DeleteSession(self.sessions[i].id.to_string()),
                    ));
                }
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(" esc: back".to_string(), Theme::dim())]),
                    MenuAction::Back,
                ));
            }
            Menu::Providers => {
                self.menu_rows.push(row(
                    Line::from(vec![
                        Span::styled(format!("  {:<16}", "default model"), Theme::FG()),
                        Span::styled(self.cfg.default_model.clone(), Theme::dim()),
                    ]),
                    MenuAction::OpenPickDefaultModel,
                ));
                self.menu_rows.push(row(
                    Line::from(vec![
                        Span::styled(format!("  {:<16}", "default effort"), Theme::FG()),
                        Span::styled(
                            self.cfg.default_effort.as_str().to_string(),
                            Theme::dim(),
                        ),
                    ]),
                    MenuAction::CycleDefaultEffort,
                ));
                for (name, pc) in &self.cfg.providers {
                    let models = self
                        .cfg
                        .models
                        .values()
                        .filter(|m| &m.provider == name)
                        .count();
                    let key_state = if pc.api_key.as_deref().is_some_and(|k| !k.is_empty()) {
                        "key set".to_string()
                    } else if let Some(env) = pc.key_env_name(name) {
                        format!("key ${env}")
                    } else {
                        "no key".to_string()
                    };
                    let is_builtin = self.cfg.is_builtin_provider(name);
                    let badge = if is_builtin { " [builtin]" } else { "" };
                    self.menu_rows.push(row(
                        Line::from(vec![
                            Span::styled(format!(" {name}{badge}"), Theme::FG()),
                            Span::styled(
                                format!("  {} · {models} models · {key_state}", pc.base_url),
                                Theme::dim(),
                            ),
                        ]),
                        MenuAction::OpenModels(name.clone()),
                    ));
                }
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(" + add provider", Theme::ACCENT_SOFT())]),
                    MenuAction::AddProvider,
                ));
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(
                        " · update built-in providers",
                        Theme::FG(),
                    )]),
                    MenuAction::UpdateBuiltins,
                ));
            }
            Menu::Models { provider } => {
                let is_builtin = self.cfg.is_builtin_provider(&provider);
                for (k, m) in &self.cfg.models {
                    if m.provider == provider {
                        let current = k == &self.session.model_key;
                        let mark = if current { " *" } else { "" };
                        let action = if is_builtin {
                            MenuAction::UseModel(k.clone())
                        } else {
                            MenuAction::EditModel(provider.clone(), k.clone())
                        };
                        let price_part = match (m.price_in, m.price_out) {
                            (Some(pi), Some(po)) => {
                                format!(" · ${}/${}", fmt_price(pi), fmt_price(po))
                            }
                            _ => String::new(),
                        };
                        self.menu_rows.push(row(
                            Line::from(vec![
                                Span::styled(format!(" {k}{mark}"), Theme::FG()),
                                Span::styled(
                                    format!(
                                        "  {} · {}{} · {}",
                                        m.id,
                                        fmt_ctx(m.context),
                                        price_part,
                                        // each row reports what that model
                                        // would actually do with its level
                                        crate::providers::effort::plan(
                                            m.effort,
                                            m.effort_support(
                                                self.cfg
                                                    .providers
                                                    .get(&m.provider)
                                                    .map(|p| p.format)
                                                    .unwrap_or(WireFormat::Openai)
                                            )
                                        )
                                        .short_label()
                                    ),
                                    Theme::dim(),
                                ),
                            ]),
                            action,
                        ));
                    }
                }
                if !is_builtin {
                    self.menu_rows.push(row(
                        Line::from(vec![Span::styled(" + add model", Theme::ACCENT_SOFT())]),
                        MenuAction::AddModel(provider.clone()),
                    ));
                }
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(" · switch active model", Theme::FG())]),
                    MenuAction::PickModelList(provider.clone()),
                ));
                // Connection probe: lights green with the detail on success,
                // red with the trimmed reason on failure.
                let (check_text, check_style) = match self.provider_checks.get(provider.as_str()) {
                    None => (" · check connection".to_string(), Theme::FG().into()),
                    Some(ProviderCheck::Checking) => (" · checking…".to_string(), Theme::dim()),
                    Some(ProviderCheck::Ok(detail)) => {
                        (format!(" · connection ok ({detail})"), Theme::ok())
                    }
                    Some(ProviderCheck::Err(reason)) => {
                        (format!(" · connection failed: {reason}"), Theme::err())
                    }
                };
                // keep narrow terminals usable: one line, bounded width
                let shown: String = check_text.chars().take(64).collect();
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(shown, check_style)]),
                    MenuAction::CheckProvider(provider.clone()),
                ));
                let edit_label = if is_builtin {
                    " · set api key"
                } else {
                    " · edit provider"
                };
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(edit_label, Theme::FG())]),
                    MenuAction::EditProvider(provider.clone()),
                ));
                if !is_builtin {
                    self.menu_rows.push(row(
                        Line::from(vec![Span::styled(" · delete model", Theme::ERR())]),
                        MenuAction::DeleteModelList(provider.clone()),
                    ));
                    self.menu_rows.push(row(
                        Line::from(vec![Span::styled(" · delete provider", Theme::ERR())]),
                        MenuAction::DeleteProvider(provider.clone()),
                    ));
                }
            }
            Menu::PickModel { provider } => {
                for (k, m) in &self.cfg.models {
                    if m.provider == provider {
                        let current = k == &self.session.model_key;
                        let mark = if current { " *current" } else { "" };
                        self.menu_rows.push(row(
                            Line::from(vec![
                                Span::styled(format!(" {k}"), Theme::FG()),
                                Span::styled(format!("  {}{mark}", m.id), Theme::dim()),
                            ]),
                            MenuAction::UseModel(k.clone()),
                        ));
                    }
                }
            }
            Menu::DeleteModelList { provider } => {
                for (k, m) in &self.cfg.models {
                    if m.provider == provider {
                        self.menu_rows.push(row(
                            Line::from(vec![
                                Span::styled(format!(" {k}"), Theme::ERR()),
                                Span::styled(format!("  {}", m.id), Theme::dim()),
                            ]),
                            MenuAction::DeleteModel(provider.clone(), k.clone()),
                        ));
                    }
                }
            }
            Menu::ConfirmDelete { label, action } => {
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(format!(" {label}"), Theme::WARN())]),
                    MenuAction::None,
                ));
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(" enter: confirm delete", Theme::ERR())]),
                    MenuAction::Confirm(Box::new(action)),
                ));
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(" esc: cancel", Theme::dim())]),
                    MenuAction::Back,
                ));
            }
            Menu::Effort => {
                // The menu has room for the whole truth: what each level does
                // on *this* model, not just its name.
                let plans: Vec<(EffortLevel, crate::providers::effort::Plan)> =
                    EffortLevel::SELECTABLE
                        .iter()
                        .map(|lvl| (*lvl, self.effort_plan_for(*lvl)))
                        .collect();
                for (lvl, plan) in plans {
                    let current = lvl == self.model_cfg.effort;
                    let mark = if current { " *current" } else { "" };
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
                    self.menu_rows.push(row(
                        Line::from(vec![
                            Span::styled(format!(" {}", lvl.as_str()), name_style),
                            Span::styled(mark.to_string(), Theme::dim()),
                            Span::styled(note, Theme::dim()),
                        ]),
                        MenuAction::SetEffort(lvl),
                    ));
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
                        Line::from(vec![Span::styled(
                            format!(" {}", q.question),
                            Theme::FG(),
                        )]),
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
                            Span::styled(format!("{checked}{}. ", o_idx + 1), Theme::LIGHT_BLUE()),
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
                    Line::from(vec![Span::styled(
                        " The agent wants to run:",
                        Theme::WARN(),
                    )]),
                    MenuAction::None,
                ));
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(format!("  {command}"), Theme::base())]),
                    MenuAction::None,
                ));
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(
                        format!("  reason: {reason}"),
                        Theme::dim(),
                    )]),
                    MenuAction::None,
                ));
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(
                        " enter: run once".to_string(),
                        Theme::ACCENT_SOFT(),
                    )]),
                    MenuAction::None,
                ));
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(
                        " a: always allow this session".to_string(),
                        Theme::ACCENT_SOFT(),
                    )]),
                    MenuAction::None,
                ));
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(" d: deny".to_string(), Theme::ERR())]),
                    MenuAction::None,
                ));
            }
            Menu::EditProvider { .. }
            | Menu::EditModel { .. }
            | Menu::EditSessionTitle { .. }
            | Menu::AskFree { .. }
            | Menu::EditScalar(..)
            | Menu::AddListItem(..)
            | Menu::EditMcpServer { .. }
            | Menu::EditLspServer { .. } => {}
            Menu::Subagents => {
                if self.subagents.is_empty() {
                    self.menu_rows.push(row(
                        Line::from(vec![Span::styled("  no subagents yet", Theme::dim())]),
                        MenuAction::None,
                    ));
                } else {
                    for (id, task, status, _, _) in &self.subagents {
                        let style = match status.as_str() {
                            "completed" => Theme::ok(),
                            "failed" => Theme::err(),
                            _ => Theme::accent(),
                        };
                        self.menu_rows.push(row(
                            Line::from(vec![
                                Span::styled(format!(" subagent-{id:<3}"), style),
                                Span::styled(format!(" {status:<10} "), Theme::dim()),
                                Span::styled(task.clone(), Theme::base()),
                            ]),
                            MenuAction::OpenSubagent(*id),
                        ));
                    }
                }
            }
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
                                Span::styled(format!("  {}. ", i + 1), Theme::LIGHT_BLUE()),
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
                    self.menu_sel = self.menu_sel.min(self.menu_rows.len().saturating_sub(1));
                    return;
                };
                let status = match plan.status {
                    plan::PlanStatus::Active => "active",
                    plan::PlanStatus::Completed => "completed",
                    plan::PlanStatus::Abandoned => "abandoned",
                };
                self.menu_rows.extend(plan_rows(
                    &plan,
                    format!("  plan {}", plan.id),
                    format!(" · {status}"),
                ));
                self.menu_footer_text = Some("up/down: scroll · esc: close".into());
            }
            Menu::PlanPreview { draft } => {
                self.menu_rows.extend(plan_rows(
                    &draft,
                    "  proposed plan".to_string(),
                    " · not stored".to_string(),
                ));
                self.menu_footer_text = Some("up/down: scroll · esc: back".into());
            }
            Menu::GraphView {
                focus_key,
                trail,
                depth,
                search_filter,
            } => {
                use crate::agent::graph::GraphStore;
                let store = crate::agent::graph::SqliteGraphStore::open(&self.project_root).ok();
                let focus_node = store.as_ref().and_then(|s| s.find_node(&focus_key).ok().flatten());
                let kind_badge = focus_node.as_ref().map(|n| node_badge(&n.kind)).unwrap_or("[?]");

                // Header row
                let mut title_spans = vec![
                    Span::styled(format!(" {kind_badge} "), Theme::accent_bold()),
                    Span::styled(focus_key.clone(), Theme::base().add_modifier(ratatui::style::Modifier::BOLD)),
                ];
                if !trail.is_empty() {
                    title_spans.push(Span::styled(format!(" (trail: {})", trail.len()), Theme::dim()));
                }
                title_spans.push(Span::styled(format!(" [depth: {depth}]"), Theme::dim()));
                self.menu_rows.push(row(Line::from(title_spans), MenuAction::None));

                // Back button if trail not empty
                if let Some(prev) = trail.last() {
                    self.menu_rows.push(row(
                        Line::from(vec![
                            Span::styled("  <- Back to ", Theme::accent()),
                            Span::styled(prev.clone(), Theme::dim()),
                        ]),
                        MenuAction::GraphBack,
                    ));
                }

                // Filter row if active
                if let Some(ref filter) = search_filter {
                    self.menu_rows.push(row(
                        Line::from(vec![
                            Span::styled("  Filter: ", Theme::accent()),
                            Span::styled(filter.clone(), Theme::base()),
                        ]),
                        MenuAction::None,
                    ));
                }

                // Query neighbors
                if let Some(store) = store {
                    let proj = store
                        .graph_query(
                            &focus_key,
                            crate::agent::graph::GraphQuery {
                                direction: crate::agent::graph::Direction::Both,
                                preset: Some("all".to_string()),
                                depth,
                                max_nodes: 50,
                                max_edges: 50,
                                limit: 50,
                                relations: Vec::new(),
                                kinds: Vec::new(),
                            },
                        )
                        .unwrap_or_else(|_| crate::agent::graph::GraphProjection {
                            nodes: Vec::new(),
                            edges: Vec::new(),
                            truncated: false,
                            truncated_reason: None,
                        });

                    if proj.edges.is_empty() {
                        self.menu_rows.push(row(
                            Line::from(vec![Span::styled(
                                "  (no connected nodes found)",
                                Theme::dim(),
                            )]),
                            MenuAction::None,
                        ));
                    } else {
                        // Compute shortest-path distance from focus_key in BFS order
                        let mut distances: std::collections::HashMap<String, u8> = std::collections::HashMap::new();
                        distances.insert(focus_key.clone(), 0);
                        let mut queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();
                        queue.push_back(focus_key.clone());

                        while let Some(curr) = queue.pop_front() {
                            let curr_d = *distances.get(&curr).unwrap_or(&0);
                            for edge in &proj.edges {
                                let neighbor = if edge.from == curr {
                                    Some(&edge.to)
                                } else if edge.to == curr {
                                    Some(&edge.from)
                                } else {
                                    None
                                };
                                if let Some(n) = neighbor {
                                    if !distances.contains_key(n) {
                                        distances.insert(n.clone(), curr_d + 1);
                                        queue.push_back(n.clone());
                                    }
                                }
                            }
                        }

                        // Determine directed connection relative to focus_key and aggregate duplicates
                        struct AggregatedNeighbor {
                            dir: &'static str,
                            kind: String,
                            target_key: String,
                            depth: u8,
                            via: Option<String>,
                            count: usize,
                        }

                        let mut aggregated: Vec<AggregatedNeighbor> = Vec::new();

                        for edge in &proj.edges {
                            let (dir, target, depth_num, via) = if edge.from == focus_key {
                                ("->", edge.to.clone(), 1, None)
                            } else if edge.to == focus_key {
                                ("<-", edge.from.clone(), 1, None)
                            } else {
                                let d_from = distances.get(&edge.from).copied().unwrap_or(99);
                                let d_to = distances.get(&edge.to).copied().unwrap_or(99);
                                if d_to > d_from {
                                    ("->", edge.to.clone(), d_to, Some(edge.from.clone()))
                                } else if d_from > d_to {
                                    ("<-", edge.from.clone(), d_from, Some(edge.to.clone()))
                                } else {
                                    // Lateral cross-edge between nodes at equal distance:
                                    // do not misattribute to focus_key
                                    continue;
                                }
                            };

                            if let Some(existing) = aggregated.iter_mut().find(|a| {
                                a.dir == dir
                                    && a.kind == edge.kind
                                    && a.target_key == target
                                    && a.depth == depth_num
                            }) {
                                existing.count += 1;
                            } else {
                                aggregated.push(AggregatedNeighbor {
                                    dir,
                                    kind: edge.kind.clone(),
                                    target_key: target,
                                    depth: depth_num,
                                    via,
                                    count: 1,
                                });
                            }
                        }

                        // Sort entries: direct neighbors first (depth 1), then multi-hop (depth > 1), then by direction/target
                        aggregated.sort_by(|a, b| {
                            a.depth
                                .cmp(&b.depth)
                                .then_with(|| a.dir.cmp(b.dir))
                                .then_with(|| a.target_key.cmp(&b.target_key))
                                .then_with(|| a.kind.cmp(&b.kind))
                        });

                        let mut rendered_count = 0;
                        for agg in aggregated {
                            let other_node = proj.nodes.iter().find(|n| n.stable_key == agg.target_key);
                            let name_or_key = other_node
                                .and_then(|n| n.name.as_deref())
                                .unwrap_or(&agg.target_key);

                            if let Some(ref filter) = search_filter {
                                if !filter.is_empty() {
                                    let fl = filter.to_lowercase();
                                    let matches_key = agg.target_key.to_lowercase().contains(&fl);
                                    let matches_name = name_or_key.to_lowercase().contains(&fl);
                                    if !matches_key && !matches_name {
                                        continue;
                                    }
                                }
                            }

                            let other_badge = other_node
                                .map(|n| node_badge(&n.kind))
                                .unwrap_or("[?]");

                            let prefix = if agg.depth > 1 {
                                format!("  +{} {} ({}) ", agg.depth, agg.dir, agg.kind)
                            } else {
                                format!("  {} ({}) ", agg.dir, agg.kind)
                            };

    let mut spans = vec![
                                Span::styled(prefix, Theme::dim()),
                                Span::styled(format!("{other_badge} "), Theme::accent()),
                                Span::styled(name_or_key.to_string(), Theme::base()),
                                Span::styled(format!(" ({})", agg.target_key), Theme::dim()),
                            ];

                            if let Some(ref via) = agg.via {
                                let via_node = proj.nodes.iter().find(|n| &n.stable_key == via);
                                let via_name = via_node.and_then(|n| n.name.as_deref()).unwrap_or(via);
                                spans.push(Span::styled(format!(" via {via_name}"), Theme::dim()));
                            }

                            if agg.count > 1 {
                                spans.push(Span::styled(format!("  ×{}", agg.count), Theme::accent_bold()));
                            }

                            self.menu_rows.push(row(
                                Line::from(spans),
                                MenuAction::GraphFocus(agg.target_key),
                            ));
                            rendered_count += 1;
                        }

                        if rendered_count == 0 && search_filter.is_some() {
                            self.menu_rows.push(row(
                                Line::from(vec![Span::styled(
                                    "  (no matching connected nodes)",
                                    Theme::dim(),
                                )]),
                                MenuAction::None,
                            ));
                        }
                    }
                }
                self.menu_footer_text = Some("enter: focus · backspace: back · f: search · +/-: depth · esc: close".into());
            }
        }
        if self.menu_sel >= self.menu_rows.len() {
            self.menu_sel = self.menu_rows.len().saturating_sub(1);
        }
    }
}

pub(super) fn node_badge(kind: &crate::agent::graph::NodeKind) -> &'static str {
    use crate::agent::graph::NodeKind;
    match kind {
        NodeKind::File => "[f]",
        NodeKind::Folder => "[dir]",
        NodeKind::Document => "[doc]",
        NodeKind::Section => "[sec]",
        NodeKind::Module => "[mod]",
        NodeKind::Namespace => "[ns]",
        NodeKind::Function => "[fn]",
        NodeKind::Method => "[meth]",
        NodeKind::Class => "[cls]",
        NodeKind::Struct => "[st]",
        NodeKind::Enum => "[e]",
        NodeKind::Interface => "[if]",
        NodeKind::Trait => "[tr]",
        NodeKind::Variable => "[v]",
        NodeKind::Constant => "[c]",
        NodeKind::Type => "[ty]",
        NodeKind::Macro => "[mac]",
        NodeKind::Test => "[test]",
        NodeKind::Memory => "[mem]",
        NodeKind::Decision => "[dec]",
        NodeKind::Commit => "[cmt]",
        NodeKind::Branch => "[br]",
    }
}

/// Render any plan — the active one from disk or an unstored proposal draft —
/// as menu rows. Shared by /plan and the propose_plan preview popup.
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
    push_wrapped(
        &mut rows,
        "  goal: ",
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
    if !plan.acceptance.is_empty() {
        rows.push(row(
            Line::from(vec![Span::styled("  acceptance:", Theme::accent())]),
            MenuAction::None,
        ));
        for (i, a) in plan.acceptance.iter().enumerate() {
            let st = match a.status {
                plan::AcceptanceStatus::Pending => Theme::dim(),
                plan::AcceptanceStatus::Passed => Theme::ok(),
                plan::AcceptanceStatus::Waived => Theme::warn(),
            };
            // stale validation overrides the passed color: the item will
            // not satisfy `complete` until it is re-verified
            let (st, suffix) = match a.validation.status {
                plan::ValidationStatus::Stale => (Theme::err(), " [stale — re-verify]"),
                plan::ValidationStatus::Passed => (st, " [verified]"),
                plan::ValidationStatus::Waived => (st, " [waived]"),
                plan::ValidationStatus::Pending => (st, ""),
            };
            let prefix = format!("    [{i}] {} ", a.status.as_str());
            push_wrapped(
                &mut rows,
                &prefix,
                &format!("{}{suffix}", a.text),
                st,
                Theme::base(),
            );
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
        let prefix = format!("  {marker} {} ({}) ", s.id, s.kind.as_str());
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

fn session_row(
    s: &SessionHeader,
    is_current: bool,
    framed: Option<usize>,
) -> (Line<'static>, MenuAction) {
    const TITLE_MAX: usize = 28;
    const MODEL_MAX: usize = 16;
    const TOK_W: usize = 7;
    // everything except title+model: lead space, gaps, fixed date, gaps, tok
    const FIXED: usize = 1 + 2 + 11 + 2 + 2 + TOK_W;
    let action = MenuAction::OpenSession(s.id.to_string());
    // narrow frames yield title first, then model; tokens (rightmost) are
    // the last thing the exact-fit pass below may touch
    let (title_w, model_w) = match framed {
        None => (TITLE_MAX, MODEL_MAX),
        Some(budget) => {
            let title_w = budget.saturating_sub(FIXED + 4).clamp(4, TITLE_MAX);
            let model_w = budget
                .saturating_sub(title_w + FIXED)
                .clamp(4, MODEL_MAX);
            (title_w, model_w)
        }
    };
    let mark = if is_current { " *" } else { "" };
    let title = fit_cell(&format!(" {}{mark}", s.title), title_w);
    let date = fmt_date(s.last_activity());
    let model = fit_cell(&s.model_key, model_w);
    let tok_raw = super::view::truncate_display_width(&fmt_k(s.context_tokens), TOK_W);
    let tok = format!(
        "{}{tok_raw}",
        " ".repeat(TOK_W.saturating_sub(super::view::cols(&tok_raw)))
    );
    let gap = || Span::styled("  ".to_string(), Theme::dim());
    let spans = vec![
        Span::styled(title, Theme::FG()),
        gap(),
        Span::styled(date, Theme::dim()),
        gap(),
        Span::styled(model, Theme::dim()),
        gap(),
        Span::styled(tok, Theme::dim()),
    ];
    let Some(frame_w) = framed else {
        return (Line::from(spans), action);
    };
    // rails + exact fit: every framed row matches the borders column-wise
    let mut full = vec![Span::styled("│".to_string(), Theme::rule_color())];
    full.extend(spans);
    full.push(Span::styled("│".to_string(), Theme::rule_color()));
    (
        fit_line_width(Line::from(full), frame_w + 2),
        action,
    )
}

/// truncate + pad to exactly `w` display columns (never chars: a CJK title
/// must not shift every column after it)
fn fit_cell(s: &str, w: usize) -> String {
    let t = super::view::truncate_display_width(s, w);
    format!("{t}{}", " ".repeat(w.saturating_sub(super::view::cols(&t))))
}

/// cut or pad a styled line to exactly `width` display columns, keeping
/// span styles on the surviving fragments
fn fit_line_width(line: Line<'static>, width: usize) -> Line<'static> {
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
        out.push(Span::styled(
            " ".repeat(width - used),
            Style::new(),
        ));
    }
    Line::from(out)
}
