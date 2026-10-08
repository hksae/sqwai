/// System prompt for the agent.
///
/// Layers, in order:
/// 1. built-in text from `src/prompts/system.md` (embedded at compile time),
///    overridable by `system.md` in the config directory without recompiling;
/// 2. project instructions from `AGENTS.md` in the working directory;
/// 3. process environment (OS, shell, working directory), captured once;
/// 4. durable user/project memory.
///
/// Session environment is a separate cacheable block built by the TUI on
/// startup and after compaction. There is no per-turn environment block.
///
/// The system block is assembled per request as ordered *parts*: the stable
/// prefix above, then the durable plan, then whatever changes while the agent
/// works (date, git state, project tree). Volatile facts always go last so
/// they cannot invalidate a cached prefix.
pub mod env;
pub mod skills;

const MEMORY_MAX_CHARS: usize = 12_000;
const DIARY_HEADING_DAYS: i64 = 7;

/// Durable user/project memory and recent diary context for the stable prefix.
/// Missing files are normal; all loaded text is screened before it reaches the
/// provider so secrets cannot be promoted into prompt context.
pub fn memory_block(root: &std::path::Path) -> Option<String> {
    let mut sections = Vec::new();
    if let Ok(path) = crate::config::config_dir().map(|dir| dir.join("USER.md"))
        && let Some(text) = read_bounded(&path)
    {
        sections.push(format!("<user_memory>\n{text}\n</user_memory>"));
    }
    let project_memory = root.join(".sqwai").join("memory").join("MEMORY.md");
    if let Some(text) = read_bounded(&project_memory) {
        sections.push(format!("<project_memory>\n{text}\n</project_memory>"));
    }
    let today = crate::agent::diary::today();
    for offset in 0..DIARY_HEADING_DAYS {
        let date = today - chrono::Duration::days(offset);
        let path = crate::agent::diary::diary_path(root, date);
        let Some(raw) = read_bounded(&path) else {
            continue;
        };
        let text = if offset < 2 { raw } else { headings_only(&raw) };
        if !text.trim().is_empty() {
            sections.push(format!("<diary date=\"{date}\">\n{text}\n</diary>"));
        }
    }
    (!sections.is_empty()).then(|| sections.join("\n\n"))
}

fn read_bounded(path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let screened = crate::agent::diary::screen(&text).text;
    Some(truncate_chars(&screened, MEMORY_MAX_CHARS))
}

fn headings_only(text: &str) -> String {
    text.lines()
        .filter(|line| line.starts_with("#"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let clipped: String = text.chars().take(max).collect();
    format!("{clipped}\n[truncated]")
}

/// Stable prefix: identical for every request of a session, safe to cache.
pub fn stable_prefix() -> String {
    stable_prefix_at(&std::env::current_dir().unwrap_or_default())
}

/// Bench assembly: identical layers, rooted at the fixture copy.
/// The model must never see the cargo-inherited cwd here.
/// Used by the bench harness (test-only) — not dead, just cfg-gated.
/// The harness is the only caller by design: production never assembles
/// prompts against a fixture root.
#[allow(dead_code)]
pub fn bench_prefix(root: &std::path::Path, baseline: bool) -> String {
    stable_prefix_inner(root, baseline)
}

fn stable_prefix_at(root: &std::path::Path) -> String {
    // the product always runs the full machinery; only the bench harness
    // passes its own baseline flag, explicitly
    stable_prefix_inner(root, false)
}

fn stable_prefix_inner(root: &std::path::Path, baseline: bool) -> String {
    let mut prompt = compose(
        &builtin_prompt(),
        project_agents_at(root).as_deref(),
        project_sqwai_at(root).as_deref(),
    );
    prompt.push_str("\n\n");
    prompt.push_str(&env::process_block_at(root));
    // The baseline arm of the bench runs without durable memory in the
    // prefix; the product path always includes it (the flag is the harness's
    // own argument, never an environment switch).
    if !baseline && let Some(memory) = memory_block(root) {
        prompt.push_str("\n\n");
        prompt.push_str(&memory);
    }
    prompt
}

/// Per-turn environment context is intentionally empty to preserve history cache.
pub fn runtime_context() -> String {
    env::runtime_context()
}

/// The durable plan goal, when the project has one. Goal and constraints
/// change only when the agent rewrites them, so this belongs to the
/// cacheable prefix. Step state lives separately (see `plan_status_block`):
/// a step that finishes must not re-key the cached prefix.
pub fn plan_block(root: &std::path::Path, session_id: Option<&str>) -> Option<String> {
    let plan = crate::plan::open_active_for_session(root, session_id)
        .ok()
        .flatten()?;
    Some(format!(
        "<durable_plan>\n{}\n</durable_plan>",
        crate::plan::render_goal(&plan)
    ))
}

/// The moving half of the plan: status and steps. Rebuilt per request — a
/// volatile host-block part, never the cached prefix.
///
/// Reference tone on purpose (§18.1.3). This block used to end in "call ops
/// directly with the step ids above … do not `create` — continue it", and a live
/// session obeyed it into a five-iteration loop: with every step already done
/// there was nothing to continue, so "continue it" came out as "проверяю,
/// коммичу и закрываю". Commands belong in system.md, where they are stated
/// once as rules; a block that rides every request describes state.
/// (Kept out of `render_status`, which also paints the user-facing `/plan` panel.)
pub fn plan_status_block(root: &std::path::Path, session_id: Option<&str>) -> Option<String> {
    let plan = crate::plan::open_active_for_session(root, session_id)
        .ok()
        .flatten()?;
    Some(format!(
        "<plan_status>\n{}\nThe state above is current as of this request, and the step ids are listed here.\n</plan_status>",
        crate::plan::render_status(&plan)
    ))
}

fn render_tools(source: &str) -> String {
    source.replace("{{TOOLS}}", &crate::agent::tools::tool_names().join(", "))
}

fn builtin_prompt() -> String {
    let source = if let Ok(dir) = crate::config::config_dir()
        && let Ok(s) = std::fs::read_to_string(dir.join("system.md"))
        && !s.trim().is_empty()
    {
        s
    } else {
        include_str!("system.md").to_string()
    };
    render_tools(&source)
}

/// AGENTS.md of a project, truncated to a sane size. Rooted explicitly:
/// callers pass the project root (bench fixture copies ship their own
/// AGENTS.md; the process cwd would leak the wrong project).
pub fn project_agents_at(root: &std::path::Path) -> Option<String> {
    let s = std::fs::read_to_string(root.join("AGENTS.md")).ok()?;
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    const MAX: usize = 12_000;
    if s.len() <= MAX {
        Some(s.to_string())
    } else {
        let mut cut = MAX;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        Some(format!("{}\n…(truncated)", &s[..cut]))
    }
}

/// combine the base prompt with optional project instructions.
/// SQWAI.md comes last and wins on conflict: it holds this agent's own
/// rules, AGENTS.md the shared project conventions.
pub fn compose(builtin: &str, agents: Option<&str>, sqwai: Option<&str>) -> String {
    let mut out = match agents {
        None => builtin.to_string(),
        Some(a) => format!("{builtin}\n\n# Project instructions (AGENTS.md)\n\n{a}"),
    };
    if let Some(s) = sqwai {
        out.push_str(
            "\n\n# Project instructions (SQWAI.md — this agent's own rules, highest priority on conflict)\n\n",
        );
        out.push_str(s);
    }
    out
}

/// SQWAI.md of a project: instructions for this agent only, other tools
/// ignore the file. Same truncation as AGENTS.md.
pub fn project_sqwai_at(root: &std::path::Path) -> Option<String> {
    let s = std::fs::read_to_string(root.join("SQWAI.md")).ok()?;
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    const MAX: usize = 12_000;
    if s.len() <= MAX {
        Some(s.to_string())
    } else {
        let mut cut = MAX;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        Some(format!("{}\n…(truncated)", &s[..cut]))
    }
}

/// template written by `/init`
pub const AGENTS_TEMPLATE: &str = "# AGENTS.md — instructions for sqwai\n\
\n\
## Build & test\n\
- build: <command>\n\
- test: <command>\n\
\n\
## Conventions\n\
- <language, style, rules>\n\
\n\
## Notes\n\
- <anything the agent must know>\n\
\n\
## Sqwai-only rules\n\
- rules for this agent alone live in SQWAI.md (other tools ignore it)\n";

/// skeleton written by `/init` next to AGENTS.md: this agent's own
/// rules (style, workflow habits). Instructions, not memories — facts
/// and history belong to MEMORY.md/diary, not here.
pub const SQWAI_TEMPLATE: &str = "# SQWAI.md — this agent's own rules\n\
\n\
Read by sqwai only (highest priority on conflict with AGENTS.md).\n\
Put instructions here, not facts — memories live in MEMORY.md/diary.\n\
\n\
## Style\n\
- <how to talk to the user>\n\
\n\
## Workflow\n\
- <habits for this project>\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqwai_md_loads_with_priority_over_agents_md() {
        let dir = std::env::temp_dir().join(format!("sqwai-sqwai-md-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("AGENTS.md"), "shared rule").unwrap();
        std::fs::write(dir.join("SQWAI.md"), "own rule").unwrap();
        let p = super::bench_prefix(&dir, true);
        assert!(p.contains("own rule"), "sqwai rules must load");
        assert!(p.contains("shared rule"), "agents rules must load");
        let agents_at = p.find("AGENTS.md").unwrap();
        let sqwai_at = p.find("SQWAI.md").unwrap();
        assert!(agents_at < sqwai_at, "sqwai section comes last");
        // empty SQWAI.md behaves as absent
        std::fs::write(dir.join("SQWAI.md"), "  \n").unwrap();
        let p = super::bench_prefix(&dir, true);
        assert!(!p.contains("SQWAI.md"), "empty file must not load");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sqwai_template_states_rules_not_memories() {
        assert!(SQWAI_TEMPLATE.contains("MEMORY.md"));
        assert!(SQWAI_TEMPLATE.contains("highest priority"));
        assert!(AGENTS_TEMPLATE.contains("SQWAI.md"));
    }

    #[test]
    fn compose_appends_agents_section() {
        let s = compose("base prompt", Some("rule one"), None);
        assert!(s.starts_with("base prompt"));
        assert!(s.contains("# Project instructions (AGENTS.md)"));
        assert!(s.contains("rule one"));
        assert!(!s.contains("SQWAI.md"));

        let s = compose("base prompt", None, None);
        assert!(!s.contains("AGENTS.md"));

        // sqwai rules come last and win on conflict
        let s = compose("base prompt", Some("shared rule"), Some("own rule"));
        let agents_at = s.find("AGENTS.md").unwrap();
        let sqwai_at = s.find("SQWAI.md").unwrap();
        assert!(agents_at < sqwai_at, "{s:?}");
        assert!(s.contains("highest priority"), "{s:?}");
        assert!(s.contains("own rule"), "{s:?}");
    }

    #[test]
    fn builtin_prompt_is_not_empty() {
        assert!(!builtin_prompt().trim().is_empty());
    }

    /// The prompt layer is the agent's constitution: a word naming a retired
    /// mechanism teaches it to call a tool that does not exist, or to ask for
    /// permission nobody gives. Every cut in the simplification pass left its
    /// text behind once (plan-first, `add_acceptance`, propose_plan) — this is
    /// the guard that was missing then. `verify`/`verification` stay legal:
    /// `[verify] commands` is alive, and "report unverified work as unverified"
    /// is a duty, not a gate.
    #[test]
    fn prompt_layer_names_no_retired_machinery() {
        let retired_tools = [
            "propose_plan",
            "memory_propose",
            "graph_query",
            "resolve_ref",
            "recall",
            "ast_grep",
            "test_impact",
            "think",
        ];
        let retired_words = [
            "acceptance",
            "propose_reset",
            "accept_proposal",
            "decline_proposal",
            "propose_goal_revision",
            "add_acceptance",
            "frozen_input",
            "plan_first",
            "plan_required",
            "SQWAI_BENCH",
            "step.refs",
            "cmd:",
            "manual:",
            "waive",
            "invalid_evidence",
            "no_evidence",
        ];

        let specs = crate::agent::tools::tool_specs(false);
        for name in retired_tools {
            assert!(
                !specs.iter().any(|s| s.name == name),
                "{name} is retired; it must not come back into the registry"
            );
        }

        // the shipped text, not `builtin_prompt()`: a user's own
        // config_dir/system.md override is theirs to write, and a guard must
        // not fail on it
        let mut haystack = include_str!("system.md").to_string();
        for spec in &specs {
            haystack.push_str(&spec.name);
            haystack.push_str(&spec.description);
            haystack.push_str(&serde_json::to_string(&spec.parameters).unwrap_or_default());
        }
        let lowered = haystack.to_ascii_lowercase();
        for word in retired_words {
            assert!(
                !lowered.contains(&word.to_ascii_lowercase()),
                "the prompt layer still names retired machinery {word:?}; reflash the text \
                 that mentions it (system.md, the tool descriptions, or a refusal hint)"
            );
        }
    }

    #[test]
    fn integrity_section_contains_sqwai_blocking_rule() {
        let prompt = builtin_prompt();
        assert!(prompt.contains("Do not modify host-owned plan, journal, memory, or checkpoint state through shell commands or general file tools."));
    }

    #[test]
    fn tool_placeholder_renders_exactly_the_registry() {
        let source = "available: {{TOOLS}}";
        let expected = crate::agent::tools::tool_names().join(", ");
        assert_eq!(render_tools(source), format!("available: {expected}"));
        // the builtin prompt carries no tool list (schemas travel in the
        // request); the placeholder stays for user-overridden system.md files
        assert!(!include_str!("system.md").contains("{{TOOLS}}"));
    }

    #[test]
    fn headings_only_keeps_structure_without_diary_prose() {
        let text = "## 2026-09-04\n### Done\n- hidden detail\n### Open\n- pending";
        assert_eq!(headings_only(text), "## 2026-09-04\n### Done\n### Open");
    }

    /// An empty project contributes no project memory and no diary. The
    /// user-level `USER.md` is real state on a working machine — the agent
    /// writes it with `memory_write` — so this test may not assume it is absent;
    /// it asserts the project halves only.
    #[test]
    fn an_empty_project_contributes_no_project_memory_or_diary() {
        let root = std::env::temp_dir().join(format!("sqwai-prompt-{}", std::process::id()));
        let block = memory_block(&root).unwrap_or_default();
        assert!(
            !block.contains("<project_memory"),
            "an empty project has no project memory: {block}"
        );
        assert!(!block.contains("<diary"), "no diary exists here: {block}");
    }

    #[test]
    fn bench_prefix_roots_at_fixture_not_cwd() {
        let dir = std::env::temp_dir().join(format!("sqwai-bench-prefix-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("AGENTS.md"), "FIXTURE AGENTS MARKER").unwrap();
        let p = bench_prefix(&dir, false);
        // working directory points at the fixture copy...
        assert!(
            p.contains(&dir.display().to_string()),
            "cwd must be the fixture: {p:.200}"
        );
        // ...and so does the project instructions layer
        assert!(p.contains("FIXTURE AGENTS MARKER"), "{p:.200}");
        // the cargo-inherited cwd must not leak in as a working directory
        let cwd = std::env::current_dir().unwrap_or_default();
        if cwd != dir {
            assert!(
                !p.contains(&format!("Working directory: {}", cwd.display())),
                "repo cwd leaked into bench prompt: {p:.200}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stable_prefix_carries_no_volatile_facts() {
        let p = stable_prefix();
        assert!(p.contains("<process_environment>"));
        assert!(
            !p.contains("<session_environment>"),
            "session env lives in B"
        );
        assert!(
            !p.contains("Date:"),
            "the clock must not enter the process prefix"
        );

        // per-turn runtime context is empty to preserve prompt cache
        let v = runtime_context();
        assert!(v.is_empty());
    }

    /// The plan block states facts and gives no orders (§18.1.3). An
    /// imperative here — "continue it" — is what a live session followed into
    /// a loop once every step was already done, because there was nothing left
    /// to continue except closing.
    #[test]
    fn plan_status_block_states_facts_without_commands() {
        use crate::plan;
        let dir = std::env::temp_dir().join(format!("sqwai-plan-note-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut plan = plan::create(
            "prove the checks".to_string(),
            Vec::new(),
            vec!["cmd: exit 0".to_string()],
            vec![plan::NewStep {
                title: "do the work".to_string(),
            }],
            0,
            &plan::Limits::default(),
        )
        .unwrap();
        plan.sessions.push("sess".to_string());
        plan::commit(
            &dir,
            "sess",
            &mut plan,
            "create",
            "model",
            true,
            serde_json::json!({}),
        )
        .unwrap();
        let block = plan_status_block(&dir, Some("sess")).expect("active plan");
        assert!(
            block.contains("current as of this request"),
            "the block must date its own state: {block}"
        );
        for command in [
            "Do not call",
            "call ops directly",
            "continue it",
            "do not `create`",
        ] {
            assert!(
                !block.contains(command),
                "imperative {command:?} left in the plan block"
            );
        }
        // and a closed plan says nothing at all: the absence convention lives
        // in system.md, not in a block that would re-inject plan vocabulary
        // where there is no plan (§18.1.6)
        plan.status = plan::PlanStatus::Completed;
        plan::store(&dir, &plan).unwrap();
        assert!(plan_status_block(&dir, Some("sess")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
