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
                    self.status("unknown command /test", StatusKind::Warn);
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
                        command: "git reset --hard HEAD~1".into(),
                        reason: "destructive git command".into(),
                    });
                } else if rest.split_whitespace().nth(1) == Some("churn") {
                    self.start_test_churn();
                } else {
                    self.status(
                        "/test takes: anim, approval, art, ask, churn, colors, md, plan",
                        StatusKind::Warn,
                    );
                }
            }
            "/debug" => self.open_menu(Menu::Debug),
            "/skill" => {
                let query = rest.split_whitespace().nth(1);
                let root = self.project_root.clone();
                let loaded = crate::prompts::skills::load_matching(&self.cfg.skills, &root, query);
                if loaded.is_empty() {
                    self.status("skill not found", StatusKind::Warn);
                } else {
                    self.active_skills = loaded.clone();
                    self.stable_parts = self.stable_parts();
                    self.context_bootstrap_pending = true;
                    self.status(
                        &format!(
                            "loaded skill(s): {}",
                            loaded
                                .iter()
                                .map(|s| s.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                        StatusKind::Ok,
                    );
                }
            }

            "/skills" => {
                self.active_skills.clear();
                self.stable_parts = self.stable_parts();
                self.context_bootstrap_pending = true;
                self.status("automatic Skills loading restored", StatusKind::Ok);
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
            "/goal" => self.goal_command(rest),
            "/constraints" => self.constraints_command(rest),
            "/mode" => self.mode_command(rest),
            "/new" => {
                self.start_new_session();
            }
            "/sessions" => self.open_menu(Menu::Sessions),
            "/providers" => {
                let arg = rest.split_whitespace().nth(1);
                if arg == Some("update") {
                    self.start_builtin_update(true);
                } else if arg == Some("add") {
                    self.open_menu(Menu::EditProvider { name: None });
                } else {
                    self.open_menu(Menu::Providers);
                }
            }
            "/models" => self.open_menu(Menu::Models {
                provider: self.model_cfg.provider.clone(),
            }),
            "/exit" => {
                // §2.5 runs retention on `/new` and `/exit`; the current
                // session's own chain is kept so a resumed session still has
                // its undo history.
                self.run_undo_maintenance();
                self.quit = true;
            }
            "/compact" => self.start_compaction(),
            "/context" => self.open_menu(Menu::Context),
            "/context-view" => self.open_menu(Menu::ContextView),
            "/effort" => self.open_menu(Menu::Effort { model: None }),
            "/undo" => {
                if self.streaming {
                    self.show_busy_status();
                } else {
                    let mut args = rest.split_whitespace().skip(1);
                    match (args.next(), args.next()) {
                        (None, _) => self.undo(1),
                        // `/undo step 3` — revert one plan step through the
                        // layer-1 pre-images (§2.5), not the last checkpoint
                        (Some("step"), Some(id)) => self.undo_step(id),
                        (Some("step"), None) => self
                            .status("/undo step needs a step id: /undo step 3", StatusKind::Warn),
                        (Some(arg), _) => match arg.parse::<usize>() {
                            Ok(n) => self.undo(n),
                            // Never guess: this used to parse as `/undo 1` and
                            // revert the last checkpoint, reported as success.
                            Err(_) => self.status(
                                &format!(
                                    "/undo takes a count or a step: /undo, /undo 3, \
                                     /undo step 3. Cannot undo {arg:?}."
                                ),
                                StatusKind::Warn,
                            ),
                        },
                    }
                }
            }
            "/export" => {
                if rest.split_whitespace().nth(1).is_some() {
                    self.status("/export takes no arguments", StatusKind::Warn);
                } else {
                    self.export_session();
                }
            }
            "/why" => {
                let question = rest.strip_prefix("why").unwrap_or("").trim();
                if question.is_empty() {
                    self.status(
                        "/why needs a question: /why why did the tests fail",
                        StatusKind::Warn,
                    );
                } else if self.streaming {
                    self.show_busy_status();
                } else if self.why_rx.is_some() {
                    self.status("a why-answer is already running", StatusKind::Warn);
                } else {
                    self.start_why(question.to_string());
                }
            }
            other if COMMANDS.contains(&other) => {
                self.status(&format!("{other}: not implemented yet"), StatusKind::Warn)
            }
            "" => {}
            other => self.status(&format!("unknown command {other}"), StatusKind::Warn),
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

    /// The linked plan if it is still workable, i.e. active. A linked
    /// completed/abandoned plan is read-only history (§2.1.1, defect A):
    /// mutations must refuse it explicitly instead of silently rewriting a
    /// finished plan the agent itself can no longer touch.
    pub(super) fn workable_plan(&self) -> Result<plan::Plan, String> {
        match self.session_plan() {
            Some(plan) if plan.status == plan::PlanStatus::Active => Ok(plan),
            Some(plan) => Err(format!(
                "plan is {} (read-only, see /plan history); create a new plan to continue",
                match plan.status {
                    plan::PlanStatus::Completed => "completed",
                    plan::PlanStatus::Abandoned => "abandoned",
                    plan::PlanStatus::Blocked => "blocked",
                    plan::PlanStatus::Active => "active",
                }
            )),
            None => Err("no active plan".to_string()),
        }
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
        let root = self.project_root.clone();
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
            Some("history") => {
                let plans = plan::list(&root);
                if plans.is_empty() {
                    "no plan history".to_string()
                } else {
                    plans
                        .iter()
                        .filter(|p| p.status != plan::PlanStatus::Active)
                        .map(|p| format!("{} · {:?} · {}", p.id, p.status, p.goal.text))
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            }
            Some("limit") => {
                format!(
                    "plan step limit is [plan].max_steps in the config (currently {}); \
                     a runtime override is not implemented yet",
                    self.cfg.plan.max_steps
                )
            }
            Some("complete") => match self.workable_plan() {
                Ok(mut active) => {
                    match plan::apply(
                        &mut active,
                        plan::Op::Complete,
                        &plan::Limits::default(),
                        None,
                    ) {
                        Ok(plan::Applied::Completed) => {
                            let sid = self.session.id.to_string();
                            match plan::commit(
                                &root,
                                &sid,
                                &mut active,
                                "complete",
                                "user",
                                true,
                                serde_json::json!({}),
                            ) {
                                Ok(_) => "plan completed".to_string(),
                                Err(e) => format!("plan write failed: {e:#}"),
                            }
                        }
                        Ok(_) => "plan complete did not change its status".to_string(),
                        Err(e) => format!("plan complete rejected [{}]: {}", e.code, e.reason),
                    }
                }
                Err(message) => message,
            },
            Some("abandon") => {
                let reason = args
                    .get(1..)
                    .map(|v| v.join(" "))
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if reason.is_empty() {
                    "usage: /plan abandon <reason>".to_string()
                } else {
                    match self.workable_plan() {
                        Ok(mut active) => {
                            plan::abandon(&mut active);
                            let sid = self.session.id.to_string();
                            let commit_args =
                                serde_json::json!({"op": "abandon", "reason": reason});
                            match plan::commit(
                                &root,
                                &sid,
                                &mut active,
                                "abandon",
                                "user",
                                true,
                                commit_args,
                            ) {
                                Ok(_) => format!("plan abandoned: {reason}"),
                                Err(e) => format!("plan write failed: {e:#}"),
                            }
                        }
                        Err(message) => message,
                    }
                }
            }
            Some("waive") | Some("confirm") => {
                "waive/confirm are retired: acceptance is now plain done-criteria notes the \
                 agent keeps for itself — there is nothing for the host to certify"
                    .to_string()
            }
            Some(other) => format!("unknown /plan action '{other}'"),
        };
        self.status(&result, StatusKind::Info);
    }

    pub(super) fn goal_command(&mut self, rest: &str) {
        let root = self.project_root.clone();
        let text = rest
            .split_once(' ')
            .map(|(_, value)| value.trim())
            .unwrap_or_default();
        if text.is_empty() {
            self.status("usage: /goal <text>", StatusKind::Warn);
            return;
        }
        match self.workable_plan() {
            Ok(mut active) => {
                plan::set_goal(
                    &mut active,
                    text.to_string(),
                    "user",
                    Some("user: /goal".to_string()),
                );
                let args = serde_json::json!({
                    "text": text,
                    "source": "user",
                    "reason": "user: /goal",
                });
                let sid = self.session.id.to_string();
                match plan::commit(&root, &sid, &mut active, "set_goal", "user", true, args) {
                    Ok(_) => self.status("goal updated; pending steps are stale", StatusKind::Ok),
                    Err(e) => self.status(&format!("goal update failed: {e:#}"), StatusKind::Err),
                }
            }
            Err(message) => self.status(&message, StatusKind::Warn),
        }
    }

    pub(super) fn constraints_command(&mut self, rest: &str) {
        let root = self.project_root.clone();
        let mut parts = rest.splitn(3, ' ');
        let _ = parts.next();
        let action = parts.next().unwrap_or_default();
        let text = parts.next().unwrap_or_default().trim();
        if text.is_empty() || !matches!(action, "add" | "remove") {
            self.status("usage: /constraints add|remove <text>", StatusKind::Warn);
            return;
        }
        match self.workable_plan() {
            Ok(mut active) => {
                if action == "add" {
                    active.constraints.push(text.to_string());
                } else if let Some(index) = active.constraints.iter().position(|c| c == text) {
                    active.constraints.remove(index);
                }
                active.revision += 1;
                let args = serde_json::json!({"action": action, "text": text});
                let sid = self.session.id.to_string();
                match plan::commit(&root, &sid, &mut active, "constraints", "user", true, args) {
                    Ok(_) => self.status("constraints updated", StatusKind::Ok),
                    Err(e) => self.status(
                        &format!("constraints update failed: {e:#}"),
                        StatusKind::Err,
                    ),
                }
            }
            Err(message) => self.status(&message, StatusKind::Warn),
        }
    }

    pub(super) fn mode_command(&mut self, rest: &str) {
        match rest.split_whitespace().nth(1) {
            Some("plan") => {
                self.set_mode(Mode::Plan);
                self.status("mode: PLAN", StatusKind::Info);
            }
            Some("act") => {
                self.set_mode(Mode::Act);
                self.status("mode: ACT", StatusKind::Info);
            }
            _ => self.status("usage: /mode plan|act", StatusKind::Warn),
        }
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
            4096,
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
