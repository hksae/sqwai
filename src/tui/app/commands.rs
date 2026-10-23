//! Slash commands: what the user types at the prompt, and the plan, goal,
//! constraint and mode commands it reaches. Kept apart from the app state they
//! mutate: this is the parser and the policy, not the UI.

#![allow(unused_imports)]
use super::*;

impl App {
    pub(super) fn apply_command_insert(&mut self, cmd: &str) {
        let text = self.input_text();
        let rest = text.split_once(' ').map(|(_, r)| r.to_string());
        let new_text = match rest {
            Some(r) => format!("{cmd} {r}"),
            None => format!("{cmd} "),
        };
        self.input = Self::fresh_input(new_text);
        self.popup_walk = None;
        self.popup_top = 0;
        self.hover = None;
        self.dirty = true;
    }

    /// Level-2 insert: replace only the subcommand word, keep already-typed
    /// arguments (`/plan wai` → `/plan waive `, `/plan waive 2` stays intact).
    pub(super) fn apply_subcommand_insert(&mut self, cmd: &str, sub: &str) {
        let text = self.input_text();
        let tail = text.strip_prefix(cmd).unwrap_or("").trim_start_matches(' ');
        let rest = tail.split_once(' ').map(|(_, r)| r.to_string());
        let new_text = match rest {
            Some(r) => format!("{cmd} {sub} {r}"),
            None => format!("{cmd} {sub} "),
        };
        self.input = Self::fresh_input(new_text);
        self.popup_walk = None;
        self.popup_top = 0;
        self.hover = None;
        self.dirty = true;
    }

    pub(super) fn command(&mut self, rest: &str) {
        let name = format!("/{rest}")
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string();
        match name.as_str() {
            "/config" | "/settings" => self.open_menu(Menu::Settings),
            "/test" => {
                if !self.experimental_test() {
                    self.status(
                        "invalid command /test <experimental flag disabled>",
                        StatusKind::Warn,
                    );
                } else if rest.split_whitespace().nth(1) == Some("art") {
                    self.open_menu(Menu::TestArt);
                } else if rest.split_whitespace().nth(1) == Some("anim") {
                    self.open_menu(Menu::TestAnim);
                } else if rest.split_whitespace().nth(1) == Some("colors") {
                    self.open_menu(Menu::TestColors);
                } else if rest.split_whitespace().nth(1) == Some("md") {
                    self.open_menu(Menu::TestMd);
                } else if rest.split_whitespace().nth(1) == Some("plan") {
                    self.test_plan = Some(Self::sample_test_plan());
                    self.open_menu(Menu::Plan);
                } else if rest.split_whitespace().nth(1) == Some("ask") {
                    self.start_test_ask();
                } else if rest.split_whitespace().nth(1) == Some("approval") {
                    self.open_menu(Menu::Approval {
                        id: 0,
                        subject: "git reset --hard HEAD~1".into(),
                        reason: "destructive git command".into(),
                        grant: "git reset *".into(),
                    });
                } else if rest.split_whitespace().nth(1) == Some("full_access") {
                    self.open_menu(Menu::ConfirmFullAccess);
                } else if rest.split_whitespace().nth(1) == Some("churn") {
                    self.start_test_churn();
                } else {
                    self.status(
                        "invalid command /test <unknown test name>",
                        StatusKind::Warn,
                    );
                }
            }
            "/debug" => self.open_menu(Menu::Debug),
            "/skill" => {
                let Some(query) = rest.split_whitespace().nth(1) else {
                    self.refresh_skills();
                    self.open_menu(Menu::Skills);
                    return;
                };
                if matches!(query.to_ascii_lowercase().as_str(), "clear" | "off" | "reset") {
                    self.run_action(MenuAction::ClearPinnedSkills);
                    return;
                }
                self.refresh_skills();
                let loaded =
                    crate::prompts::skills::filter_by_query(self.cached_skills(), query);
                if loaded.is_empty() {
                    self.status("skill not found", StatusKind::Warn);
                } else {
                    let mut names = Vec::new();
                    for sk in loaded {
                        names.push(sk.name.clone());
                        if !self
                            .active_skills
                            .iter()
                            .any(|s| s.name.eq_ignore_ascii_case(&sk.name))
                        {
                            self.active_skills.push(sk);
                        }
                    }
                    self.context_bootstrap_pending = true;
                    self.sync_active_skills_to_session();
                    self.status(
                        &format!("loaded skill(s): {}", names.join(", ")),
                        StatusKind::Ok,
                    );
                }
            }

            "/skills" => {
                if matches!(
                    rest.split_whitespace().nth(1).map(|s| s.to_ascii_lowercase()).as_deref(),
                    Some("clear" | "off" | "reset")
                ) {
                    self.run_action(MenuAction::ClearPinnedSkills);
                } else {
                    self.refresh_skills();
                    self.open_menu(Menu::Skills);
                }
            }

            "/help" => self.open_menu(Menu::Help),
            "/init" => {
                let init_root = self.project_root.clone();
                if init_root.join("AGENTS.md").exists() {
                    self.status("AGENTS.md already exists", StatusKind::Warn);
                } else {
                    match std::fs::write(
                        init_root.join("AGENTS.md"),
                        crate::prompts::AGENTS_TEMPLATE,
                    ) {
                        Ok(()) => self.status(
                            "AGENTS.md created — it is sent to the model with every request",
                            StatusKind::Ok,
                        ),
                        Err(e) => self.status(&format!("init: {e}"), StatusKind::Err),
                    }
                }
                // sqwai-only rules live apart so other tools never see them
                if init_root.join("SQWAI.md").exists() {
                    self.status("SQWAI.md already exists", StatusKind::Warn);
                } else {
                    match std::fs::write(init_root.join("SQWAI.md"), crate::prompts::SQWAI_TEMPLATE)
                    {
                        Ok(()) => self.status(
                            "SQWAI.md created — this agent's own rules, highest priority",
                            StatusKind::Ok,
                        ),
                        Err(e) => self.status(&format!("init: {e}"), StatusKind::Err),
                    }
                }
                // seed named verify commands (repo probing + MEMORY.md) so
                // `cmd: $name` in plans resolves; hand-written names win.
                let root = self.project_root.clone();
                let (added, already) = crate::config::Config::seed_verify_commands(&root);
                if !added.is_empty() || already > 0 {
                    let list = added
                        .iter()
                        .map(|(n, c)| format!("{n} = {c}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let msg = if list.is_empty() {
                        format!("verify commands: {already} already set — .sqwai/config.toml")
                    } else {
                        format!(
                            "verify commands: +{} [{list}] ({already} already set) — .sqwai/config.toml",
                            added.len()
                        )
                    };
                    self.status(&msg, StatusKind::Ok);
                }
            }
            "/plan" => self.plan_command(rest),
            "/new" => {
                self.start_new_session();
            }
            "/sessions" => self.open_menu(Menu::Sessions),
            "/providers" => self.open_menu(Menu::Providers),
            "/models" => self.open_menu(Menu::Models {
                provider: self.model_cfg.provider.clone(),
            }),
            "/exit" => {
                // runs retention on `/new` and `/exit`; the current
                // session's own chain is kept so a resumed session still has
                // its undo history.
                self.run_undo_maintenance();
                self.quit = true;
            }
            "/compact" => {
                let focus: Vec<&str> = rest.split_whitespace().skip(1).collect();
                self.start_compaction_with_focus(&focus.join(" "));
            }
            "/context" => self.open_menu(Menu::Context),
            "/context-view" => self.open_menu(Menu::ContextView),
            "/effort" => self.open_menu(Menu::Effort { model: None }),
            "/undo" => {
                if self.streaming {
                    self.show_busy_status();
                } else {
                    let mut parts = rest.split_whitespace().skip(1);
                    match parts.next() {
                        None => self.undo(1),
                        Some("turn") => {
                            if parts.next().is_some() {
                                self.status(
                                    "invalid command /undo turn <takes no arguments>",
                                    StatusKind::Warn,
                                );
                            } else {
                                self.undo_turn();
                            }
                        }
                        Some("list") => {
                            if parts.next().is_some() {
                                self.status(
                                    "invalid command /undo list <takes no arguments>",
                                    StatusKind::Warn,
                                );
                            } else if self.session.checkpoints.is_empty()
                                && self.session.redo_stack.is_empty()
                            {
                                self.status("nothing to undo", StatusKind::Info);
                            } else {
                                self.open_menu(Menu::UndoList);
                            }
                        }
                        Some("step") => match parts.next() {
                            Some(step) if parts.next().is_none() => self.undo_step(step),
                            _ => self.status(
                                "invalid command /undo step <missing step id>",
                                StatusKind::Warn,
                            ),
                        },
                        Some(arg) => match arg.parse::<usize>() {
                            Ok(n) if parts.next().is_none() => self.undo(n),
                            // Never guess: this used to parse as `/undo 1` and
                            // revert the last checkpoint, reported as success.
                            _ => self.status(
                                "invalid command /undo <expected a number>",
                                StatusKind::Warn,
                            ),
                        },
                    }
                }
            }
            "/redo" => {
                if self.streaming {
                    self.show_busy_status();
                } else {
                    let mut parts = rest.split_whitespace().skip(1);
                    match parts.next() {
                        None => self.redo(1),
                        Some(arg) => match arg.parse::<usize>() {
                            Ok(n) if parts.next().is_none() => self.redo(n),
                            _ => self.status(
                                "invalid command /redo <expected a number>",
                                StatusKind::Warn,
                            ),
                        },
                    }
                }
            }
            "/export" => {
                if rest.split_whitespace().nth(1).is_some() {
                    self.status(
                        "invalid command /export <takes no arguments>",
                        StatusKind::Warn,
                    );
                } else {
                    self.export_session();
                }
            }
            other if COMMANDS.contains(&other) => {
                self.status(&format!("invalid command {other}"), StatusKind::Warn)
            }
            "" => {}
            other => self.status(&format!("invalid command {other}"), StatusKind::Warn),
        }
        self.dirty = true;
    }

    /// The plan this session works on: its linked plan while the file is
    /// still readable on disk, otherwise the session's own active plan —
    /// never another session's (#171). Every TUI read or mutation of
    /// "the plan" goes through here so two sessions never operate on each
    /// other's plans.
    pub(super) fn session_plan(&self) -> Option<plan::Plan> {
        if let Some(p) = &self.test_plan {
            return Some(p.clone());
        }
        let root = &self.project_root;
        if let Some(id) = &self.session.plan_id
            && let Some(plan) = plan::read_plan_file(root, id)
        {
            return Some(plan);
        }
        plan::open_active_for_session(root, Some(&self.session.id.to_string()))
            .ok()
            .flatten()
    }

    /// Plan id `/plan delete` would remove: the session's own plan while its
    /// file is still on disk, otherwise the most recent active plan.
    /// One shared resolver so the command gate and the confirmed action can
    /// never disagree about what is being deleted.
    pub(super) fn deletable_plan_id(&self) -> Option<String> {
        let root = &self.project_root;
        self.session
            .plan_id
            .clone()
            .filter(|id| plan::plans_dir(root).join(format!("{id}.json")).exists())
            .or_else(|| {
                plan::open_active_for_session(root, Some(&self.session.id.to_string()))
                    .ok()
                    .flatten()
                    .map(|plan| plan.id)
            })
    }

    pub(super) fn plan_command(&mut self, rest: &str) {
        let args: Vec<&str> = rest.split_whitespace().skip(1).collect();
        let result = match args.first().copied() {
            // bare `/plan` opens the overview; there is no `show` alias
            None => {
                self.open_menu(Menu::Plan);
                return;
            }
            Some("delete") => match self.deletable_plan_id() {
                Some(_) => {
                    self.open_menu(Menu::ConfirmDelete {
                        label: "Are you sure? (y/N)".to_string(),
                        action: MenuAction::DeletePlan,
                    });
                    return;
                }
                None => "no active plan".to_string(),
            },
            Some("waive") | Some("confirm") => {
                "invalid command /plan <subcommand is retired>".to_string()
            }
            Some(other) => format!("invalid command /plan {other} <unknown subcommand>"),
        };
        self.status(&result, StatusKind::Info);
    }

    /// Switch ACT/PLAN through one funnel: arms the chip color sweep from
    /// the currently displayed color (see `mode_blend`), so a toggle
    /// mid-sweep redirects the blend instead of breaking it.
    pub(super) fn set_mode(&mut self, mode: Mode) {
        let from = self.mode_chip_rgb(Instant::now());
        self.mode = mode;
        self.mode_blend = Some((from, Instant::now()));
        self.dirty = true;
    }

    /// Switch the approval axis (ask/auto/full) through the same funnel:
    /// sweep from the displayed color. The change is a notice, not history
    /// — the chip is what still says the mode ten minutes later.
    pub(super) fn set_approval_mode(&mut self, mode: crate::config::ApprovalMode) {
        if mode == self.approval_mode {
            return;
        }
        let from = self.approval_chip_rgb(Instant::now());
        let what = format!(
            "approval: {} → {}",
            self.approval_mode.as_str(),
            mode.as_str()
        );
        self.approval_mode = mode;
        self.approval_blend = Some((from, Instant::now()));
        self.status(&what, StatusKind::Info);
        self.dirty = true;
    }

    /// Shift+Tab: cycle ask → auto → full → ask. Entering full access is
    /// gated behind the confirm dialog (a held key must not be able to
    /// grant it) — once per app run: after the first enable the dialog
    /// steps aside so cycling stays fluid, and only a restart brings it
    /// back. Leaving full is always immediate. In PLAN the axis is moot
    /// — mutations are refused outright — so the key is a deliberate no-op
    /// there rather than arming a mode the user cannot see acting.
    pub(super) fn cycle_approval_mode(&mut self) {
        if self.mode != Mode::Act {
            return;
        }
        match self.approval_mode.cycle_next() {
            crate::config::ApprovalMode::Full
                if self.full_access_unlocked =>
            {
                self.set_approval_mode(crate::config::ApprovalMode::Full)
            }
            crate::config::ApprovalMode::Full => self.open_menu(Menu::ConfirmFullAccess),
            next => self.set_approval_mode(next),
        }
    }

    fn sample_test_plan() -> plan::Plan {
        let mut p = plan::create(
            "Migrate TUI menus to the bottom surface".into(),
            vec![
                "keep narrow terminals usable".into(),
                "preserve keyboard and mouse behavior".into(),
            ],
            vec![
                "all menus render as bottom surfaces".into(),
                "cargo test passes cleanly".into(),
            ],
            vec![
                plan::NewStep {
                    title: "Convert Models and Providers to searchable surfaces".into(),
                },
                plan::NewStep {
                    title: "Convert Sessions and /config to two-column surfaces".into(),
                },
                plan::NewStep {
                    title: "Migrate /plan overview to the bottom surface".into(),
                },
                plan::NewStep {
                    title: "Verify legacy modal code is retired".into(),
                },
                plan::NewStep {
                    title: "Keep old floating todo popup".into(),
                },
            ],
            &plan::Limits::default(),
        )
        .unwrap();
        p.steps[0].status = plan::StepStatus::Done;
        p.steps[0].summary = Some("Models and Providers now share draw_list_surface".into());
        p.steps[1].status = plan::StepStatus::Done;
        p.steps[1].summary = Some("Added search row, day headers, and 2-column /config".into());
        p.steps[2].status = plan::StepStatus::InProgress;
        p.steps[3].status = plan::StepStatus::Pending;
        p.steps[4].status = plan::StepStatus::Cancelled;
        p.steps[4].reason = Some("unified into /plan instead".into());
        p
    }

    fn start_test_ask(&mut self) {
        use crate::agent::loop_task::{AskOption, AskQuestion};
        if self.active_ask_id.is_some() {
            self.ask_custom_focus = None;
            self.inline_ask_skip();
        }
        let questions = vec![
            AskQuestion {
                header: "Storage".into(),
                question: "Which backend should we use?".into(),
                options: vec![
                    AskOption {
                        label: "SQLite".into(),
                        description: Some("Single-file local database, zero config".into()),
                        recommended: true,
                    },
                    AskOption {
                        label: "PostgreSQL".into(),
                        description: Some("Full relational server".into()),
                        recommended: false,
                    },
                    AskOption {
                        label: "In-memory".into(),
                        description: Some("Ephemeral store for tests".into()),
                        recommended: false,
                    },
                ],
                multiple: false,
                allow_free: true,
            },
            AskQuestion {
                header: "Features".into(),
                question: "Select all features to enable".into(),
                options: vec![
                    AskOption {
                        label: "Prompt caching".into(),
                        description: Some("Reuse stable system prefix".into()),
                        recommended: true,
                    },
                    AskOption {
                        label: "Shadow git".into(),
                        description: Some("Automatic undo checkpoints".into()),
                        recommended: false,
                    },
                    AskOption {
                        label: "Streaming".into(),
                        description: Some("Incremental token output".into()),
                        recommended: false,
                    },
                ],
                multiple: true,
                allow_free: true,
            },
            AskQuestion {
                header: "Verify".into(),
                question: "Run full test suite after applying changes?".into(),
                options: vec![
                    AskOption {
                        label: "Yes".into(),
                        description: Some("Run cargo test before commit".into()),
                        recommended: true,
                    },
                    AskOption {
                        label: "Fast check only".into(),
                        description: Some("Run cargo check only".into()),
                        recommended: false,
                    },
                ],
                multiple: false,
                allow_free: false,
            },
        ];
        let summary = questions
            .iter()
            .map(|q| q.question.clone())
            .collect::<Vec<_>>()
            .join(" | ");
        let call_id = format!("test-ask-{}", self.segments.len());
        self.handle_tool_start("ask_user".into(), summary, Some(call_id.clone()));
        self.open_ask(&call_id, 0, questions);
    }
}
