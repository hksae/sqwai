//! AB `/export`: session dumps for bug reports and handoffs (one command,
//! two files). Pure functions — the TUI handler writes the files.
//!
//! Markdown is for humans (paste into an issue); JSON is for machines
//! (journal tail, plan, transcript). Secrets are screened, sizes capped,
//! paths stay relative. Nothing here touches the network or the model.

use std::path::Path;

/// Chars kept per transcript message; the rest becomes a marker, not a cut.
const MESSAGE_CHARS: usize = 4000;
/// Journal records in the JSON tail: recent context, not archaeology.
const JOURNAL_TAIL: usize = 500;

pub struct Export {
    pub markdown: String,
    pub json: serde_json::Value,
}

fn screen(s: &str) -> String {
    crate::agent::secrets::screen(s).text
}

fn cap(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let taken: String = s.chars().take(max).collect();
    format!("{taken}\n…(truncated, {max} chars kept)")
}

fn role_label(role: &crate::providers::Role) -> &'static str {
    match role {
        crate::providers::Role::User => "User",
        crate::providers::Role::Assistant => "Assistant",
        crate::providers::Role::Tool => "Tool",
        crate::providers::Role::System => "System",
    }
}

/// Build both dumps from the live transcript plus host state.
/// `model` is the session's model id for the header.
pub fn export_session(
    root: &Path,
    session: &str,
    model: &str,
    messages: &[crate::providers::Message],
) -> Export {
    let plan = crate::plan::open_active_for_session(root, Some(session))
        .ok()
        .flatten();
    let plan_render = plan.as_ref().map(crate::plan::render);
    let records = crate::agent::journal::Journal::records_for(root, session).unwrap_or_default();
    let tail: Vec<serde_json::Value> = records
        .iter()
        .rev()
        .take(JOURNAL_TAIL)
        .rev()
        .filter_map(|r| serde_json::to_value(r).ok())
        .collect();

    let mut markdown = format!(
        "# sqwai export — session {session}\n\n\
         - exported: {}\n\
         - sqwai: {}\n\
         - model: {model}\n\
         - os: {}\n\n",
        chrono::Local::now().to_rfc3339(),
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
    );
    markdown.push_str("## Plan\n\n");
    markdown.push_str(plan_render.as_deref().unwrap_or("none"));
    markdown.push_str("\n\n## Transcript\n");
    let mut json_messages = Vec::new();
    for message in messages {
        let content = cap(&screen(&message.content), MESSAGE_CHARS);
        markdown.push_str(&format!(
            "\n### {}\n\n{content}\n",
            role_label(&message.role)
        ));
        if !message.tool_calls.is_empty() {
            let calls: Vec<String> = message
                .tool_calls
                .iter()
                .map(|c| format!("{} ({})", c.name, c.id))
                .collect();
            markdown.push_str(&format!("\n[calls: {}]\n", calls.join(", ")));
        }
        json_messages.push(serde_json::json!({
            "role": role_label(&message.role),
            "content": content,
            "tool_call_id": message.tool_call_id,
            "calls": message.tool_calls.iter().map(|c| &c.name).collect::<Vec<_>>(),
        }));
    }
    let json = serde_json::json!({
        "version": 1,
        "session": session,
        "model": model,
        "sqwai": env!("CARGO_PKG_VERSION"),
        "os": std::env::consts::OS,
        "plan": plan.as_ref().and_then(|p| serde_json::to_value(p).ok()),
        "messages": json_messages,
        "journal_tail": tail,
    });
    Export { markdown, json }
}

/// Destination pair under `.sqwai/exports/`, created on write.
pub fn export_paths(root: &Path, session: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let dir = root.join(".sqwai").join("exports");
    let stem = dir.join(format!("export-{session}-{stamp}"));
    (stem.with_extension("md"), stem.with_extension("json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn messages() -> Vec<crate::providers::Message> {
        vec![
            crate::providers::Message::new(crate::providers::Role::User, "do the thing"),
            crate::providers::Message::new(
                crate::providers::Role::Assistant,
                "using token ghp_abcdefghijklmnopqrstuvw1234567890 done",
            ),
        ]
    }

    #[test]
    fn export_screens_caps_and_structures() {
        let dir = std::env::temp_dir().join(format!("sqwai-export-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut journal =
            crate::agent::journal::Journal::open(&dir, "sess").expect("journal opens");
        journal
            .append(
                "tool_result",
                serde_json::json!({"tool": "bash", "ok": true}),
            )
            .unwrap();
        let out = export_session(&dir, "sess", "m", &messages());
        // secret screened everywhere
        assert!(
            !out.markdown
                .contains("ghp_abcdefghijklmnopqrstuvw1234567890"),
            "{}",
            out.markdown
        );
        assert!(
            !out.json
                .to_string()
                .contains("ghp_abcdefghijklmnopqrstuvw1234567890")
        );
        // structure: header, plan section, transcript roles
        assert!(out.markdown.contains("# sqwai export — session sess"));
        assert!(out.markdown.contains("## Transcript"));
        assert!(out.markdown.contains("### User"));
        assert!(out.markdown.contains("## Plan"));
        assert_eq!(out.json["session"], serde_json::json!("sess"));
        assert_eq!(out.json["journal_tail"].as_array().unwrap().len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn export_truncates_long_messages_with_marker() {
        let dir = std::env::temp_dir().join(format!("sqwai-export-long-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let big = "x".repeat(MESSAGE_CHARS + 100);
        let msgs = vec![crate::providers::Message::new(
            crate::providers::Role::Tool,
            big,
        )];
        let out = export_session(&dir, "sess", "m", &msgs);
        assert!(
            out.markdown.contains("truncated"),
            "{}",
            &out.markdown[out.markdown.len() - 200..]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn export_paths_pair_md_and_json() {
        let (md, json) = export_paths(std::path::Path::new("/r"), "abc");
        assert_eq!(md.extension().unwrap(), "md");
        assert_eq!(json.extension().unwrap(), "json");
        assert!(md.parent().unwrap().ends_with(".sqwai/exports"));
        assert_eq!(md.with_extension(""), json.with_extension(""));
    }
}
