/// System prompt for the agent.
///
/// Layers, in order:
/// 1. built-in text from `src/prompts/system.md` (embedded at compile time),
///    overridable by `system.md` in the config directory without recompiling;
/// 2. project instructions from `AGENTS.md` in the working directory;
/// 3. live environment block (OS, git, tree, toolchains) gathered per request.
pub mod env;

pub fn system_prompt() -> String {
    system_prompt_for(true)
}

pub fn system_prompt_for(with_tools: bool) -> String {
    let builtin = if with_tools {
        builtin_prompt()
    } else {
        concise_prompt()
    };
    let agents = if with_tools { project_agents() } else { None };
    let mut prompt = compose(&builtin, agents.as_deref());
    if with_tools {
        prompt.push_str("\n\n");
        prompt.push_str(&env::environment_block());
        if let Ok(root) = std::env::current_dir()
            && let Some(plan) = crate::plan::load(&root)
        {
            prompt.push_str("\n\n<durable_plan>\n");
            prompt.push_str(&plan);
            prompt.push_str("</durable_plan>");
        }
    }
    prompt
}

fn concise_prompt() -> String {
    "You are an AI coding agent hosted inside the sqwai CLI application. Reply in the user's language. For trivial requests, answer directly and briefly. Do not claim to use tools or inspect files unless the request requires it.".into()
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
}
