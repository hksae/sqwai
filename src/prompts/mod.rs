/// System prompt for the agent.
///
/// Layers, in order:
/// 1. built-in text from `src/prompts/system.md` (embedded at compile time),
///    overridable by `system.md` in the config directory without recompiling;
/// 2. project instructions from `AGENTS.md` in the working directory;
/// 3. stable environment block (OS, shell, working directory).
///
/// The system block is assembled per request as ordered *parts*: the stable
/// prefix above, then the durable plan, then whatever changes while the agent
/// works (date, git state, project tree). Volatile facts always go last so
/// they cannot invalidate a cached prefix.
pub mod env;

/// Stable prefix: identical for every request of a session, safe to cache.
pub fn stable_prefix() -> String {
    let mut prompt = compose(&builtin_prompt(), project_agents().as_deref());
    prompt.push_str("\n\n");
    prompt.push_str(&env::stable_block());
    prompt
}

/// Minimal prompt for requests that need no tools and no project context.
pub fn concise_prompt() -> String {
    "You are an AI coding agent hosted inside the sqwai CLI application. Reply in the user's language. For trivial requests, answer directly and briefly. Do not claim to use tools or inspect files unless the request requires it.".into()
}

/// Volatile runtime context: re-read once per user turn, never cached.
pub fn runtime_context() -> String {
    env::volatile_block()
}

/// The durable plan, when the project has one. It changes only when the agent
/// rewrites it, so it belongs to the cacheable prefix.
pub fn plan_block(root: &std::path::Path) -> Option<String> {
    let plan = crate::plan::load(root)?;
    Some(format!("<durable_plan>\n{plan}</durable_plan>"))
}

fn builtin_prompt() -> String {
    if let Ok(dir) = crate::config::config_dir()
        && let Ok(s) = std::fs::read_to_string(dir.join("system.md"))
        && !s.trim().is_empty()
    {
        return s;
    }
    include_str!("system.md").to_string()
}

/// AGENTS.md of the current project, truncated to a sane size
pub fn project_agents() -> Option<String> {
    let s = std::fs::read_to_string("AGENTS.md").ok()?;
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

/// combine the base prompt with optional project instructions
pub fn compose(builtin: &str, agents: Option<&str>) -> String {
    match agents {
        None => builtin.to_string(),
        Some(a) => format!("{builtin}\n\n# Project instructions (AGENTS.md)\n\n{a}"),
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
- <anything the agent must know>\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_appends_agents_section() {
        let s = compose("base prompt", Some("rule one"));
        assert!(s.starts_with("base prompt"));
        assert!(s.contains("# Project instructions (AGENTS.md)"));
        assert!(s.contains("rule one"));

        let s = compose("base prompt", None);
        assert!(!s.contains("AGENTS.md"));
    }

    #[test]
    fn builtin_prompt_is_not_empty() {
        assert!(!builtin_prompt().trim().is_empty());
    }

    #[test]
    fn stable_prefix_carries_no_volatile_facts() {
        let p = stable_prefix();
        assert!(p.contains("<environment>"));
        assert!(!p.contains("<runtime_context>"), "git/tree live elsewhere");
        assert!(!p.contains("Date:"), "the clock must not enter the prefix");

        // volatile facts are a separate part, appended after the prefix
        let v = runtime_context();
        assert!(v.contains("<runtime_context>"));
        assert!(v.contains("Date:"));
    }
}
