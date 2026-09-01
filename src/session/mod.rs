use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::providers::{Message, Role, Usage};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: Uuid,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub model_key: String,
    pub context_limit: u64,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub usage: Usage,
    /// tokens estimated from message length when the provider sent no usage
    #[serde(default)]
    pub estimated_tokens: u64,
    /// prompt size of the latest provider request (current context occupancy)
    ///; separate from cumulative usage used for billing/statistics
    #[serde(default)]
    pub context_tokens: u64,
    /// time of the latest user/agent message; drives ordering in the sessions menu
    #[serde(default)]
    pub last_message_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub pinned: bool,
    /// set when this session was created by /fork
    #[serde(default)]
    pub forked_from_id: Option<String>,
    /// title snapshot of the parent, survives parent deletion
    #[serde(default)]
    pub forked_from_title: Option<String>,
    /// (sha, label) undo journal: one entry per mutating agent action
    #[serde(default)]
    pub checkpoints: Vec<(String, String)>,
    /// last to-do list written via the todowrite tool, restored on resume
    #[serde(default)]
    pub todos: Vec<String>,
}

impl Session {
    pub fn new(model_key: String, context_limit: u64) -> Self {
        Self {
            id: Uuid::new_v4(),
            title: "new session".into(),
            created_at: Utc::now(),
            model_key,
            context_limit,
            messages: Vec::new(),
            usage: Usage::default(),
            estimated_tokens: 0,
            context_tokens: 0,
            last_message_at: None,
            pinned: false,
            forked_from_id: None,
            forked_from_title: None,
            checkpoints: Vec::new(),
            todos: Vec::new(),
        }
    }

    /// create a fork copying messages up to `last_idx` inclusive
    pub fn fork_upto(&self, last_idx: usize) -> Self {
        let mut f = Session::new(self.model_key.clone(), self.context_limit);
        f.title = self.title.clone();
        f.messages = self.messages[..=(last_idx.min(self.messages.len() - 1))].to_vec();
        f.estimated_tokens = f
            .messages
            .iter()
            .map(|m| m.content.len() as u64)
            .sum::<u64>()
            .div_ceil(4);
        // provider usage describes prompts of the whole original conversation;
        // carry it over only when everything is copied
        if f.messages.len() == self.messages.len() {
            f.usage = self.usage;
        }
        f.forked_from_id = Some(self.id.to_string());
        f.forked_from_title = Some(self.title.clone());
        f
    }

    /// pinned sessions first, then by latest activity
    pub fn sort_sessions(v: &mut [Self]) {
        v.sort_by(|a, b| {
            b.pinned
                .cmp(&a.pinned)
                .then_with(|| b.last_activity().cmp(&a.last_activity()))
        });
    }

    /// last activity time, falling back to creation time
    pub fn last_activity(&self) -> DateTime<Utc> {
        self.last_message_at.unwrap_or(self.created_at)
    }

    pub fn sessions_dir() -> Result<std::path::PathBuf> {
        let dir = crate::config::data_dir()?.join("sessions");
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    pub fn push(&mut self, role: Role, content: impl Into<String>) {
        let content = content.into();
        if self.messages.is_empty() && role == Role::User {
            self.title = title_from(&content);
        }
        if role != Role::System {
            self.estimated_tokens += (content.len() as u64).div_ceil(4);
            self.last_message_at = Some(Utc::now());
        }
        self.messages.push(Message::new(role, content));
    }

    pub fn add_usage(&mut self, u: &Usage) {
        self.usage.prompt_tokens += u.prompt_tokens;
        self.usage.completion_tokens += u.completion_tokens;
        if let Some(c) = u.cached_tokens {
            *self.usage.cached_tokens.get_or_insert(0) += c;
        }
        // Provider prompt_tokens describes the current request context. Do not
        // use the cumulative billing counter as the live context meter.
        self.context_tokens = u.prompt_tokens;
    }

    /// tokens occupying the latest provider request context
    pub fn context_tokens_used(&self) -> u64 {
        if self.context_tokens > 0 {
            self.context_tokens
        } else if self.usage.prompt_tokens == 0 {
            self.estimated_tokens
        } else {
            0
        }
    }

    /// cumulative usage, useful for billing/statistics
    pub fn used_tokens(&self) -> u64 {
        let reported = self.usage.prompt_tokens + self.usage.completion_tokens;
        if reported > 0 {
            reported
        } else {
            self.estimated_tokens
        }
    }

    pub fn context_percent(&self) -> f64 {
        if self.context_limit == 0 {
            return 0.0;
        }
        ((self.context_tokens_used() as f64 / self.context_limit as f64) * 100.0).clamp(0.0, 100.0)
    }

    pub fn save(&self) -> Result<()> {
        // unit tests must never write real session files
        #[cfg(test)]
        return Ok(());
        #[allow(unreachable_code)]
        {
            let path = Self::sessions_dir()?.join(format!("{}.json", self.id));
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
            std::fs::rename(&tmp, &path).context("saving session")?;
            Ok(())
        }
    }

    pub fn load(id: &str) -> Result<Self> {
        let path = Self::resolve_path(id)?;
        let raw = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&raw)?)
    }

    /// find a session file by full uuid or unique prefix
    fn resolve_path(id: &str) -> Result<std::path::PathBuf> {
        let dir = Self::sessions_dir()?;
        let mut found = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let name = entry?.file_name().into_string().unwrap_or_default();
            if name.starts_with(id) && name.ends_with(".json") {
                found.push(name);
            }
        }
        anyhow::ensure!(
            found.len() == 1,
            "expected exactly one session matching {id:?}, found {}",
            found.len()
        );
        Ok(dir.join(&found[0]))
    }

    /// all saved sessions, newest activity first, pinned on top
    #[cfg_attr(test, allow(dead_code))]
    pub fn list() -> Result<Vec<Self>> {
        let dir = Self::sessions_dir()?;
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match std::fs::read_to_string(&path)
                .ok()
                .and_then(|raw| serde_json::from_str::<Self>(&raw).ok())
            {
                Some(s) => out.push(s),
                // skip corrupt files instead of breaking the whole menu
                None => continue,
            }
        }
        Self::sort_sessions(&mut out);
        Ok(out)
    }

    pub fn delete(id: &str) -> Result<()> {
        // unit tests must never remove real session files
        #[cfg(test)]
        {
            let _ = id;
            return Ok(());
        }
        #[allow(unreachable_code)]
        {
            let path = Self::resolve_path(id)?;
            std::fs::remove_file(path).context("deleting session")?;
            Ok(())
        }
    }
}

fn title_from(s: &str) -> String {
    let first_line = s.lines().next().unwrap_or("new session");
    let t = first_line.trim();
    if t.chars().count() <= 40 {
        t.to_string()
    } else {
        format!("{}…", t.chars().take(40).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_from_first_line_truncated() {
        assert_eq!(title_from("fix the bug\nmore"), "fix the bug");
        let long = "x".repeat(60);
        assert_eq!(title_from(&long).chars().count(), 41); // 40 + ellipsis
    }

    #[test]
    fn context_percent_clamped() {
        let mut s = Session::new("m".into(), 1000);
        s.push(Role::User, "hello");
        assert!(s.context_percent() < 1.0);
        s.add_usage(&Usage {
            prompt_tokens: 500,
            completion_tokens: 600,
            cached_tokens: None,
        });
        assert_eq!(s.context_tokens_used(), 500);
        assert_eq!(s.context_percent(), 50.0);
        assert_eq!(s.used_tokens(), 1100);

        // A later request reports its own prompt size; the context meter must
        // follow it instead of accumulating every previous prompt.
        s.add_usage(&Usage {
            prompt_tokens: 36_267,
            completion_tokens: 10,
            cached_tokens: None,
        });
        assert_eq!(s.context_tokens_used(), 36_267);
        assert_eq!(s.context_percent(), 100.0);
    }

    #[test]
    fn fork_copies_prefix_and_marks_origin() {
        let mut s = Session::new("m".into(), 1000);
        s.push(Role::User, "one");
        s.push(Role::Assistant, "two");
        s.push(Role::User, "three");
        s.usage.prompt_tokens = 900;

        // partial fork: no provider usage, estimated tokens from copied text
        let f = s.fork_upto(1);
        assert_eq!(f.messages.len(), 2);
        assert_eq!(f.messages[0].content, "one");
        assert_eq!(f.usage.prompt_tokens, 0);
        assert!(f.estimated_tokens > 0);
        assert_eq!(f.forked_from_id.as_deref(), Some(s.id.to_string().as_str()));
        assert_eq!(f.forked_from_title.as_deref(), Some(s.title.as_str()));
        assert!(!f.pinned);

        // full fork carries usage over
        let full = s.fork_upto(2);
        assert_eq!(full.messages.len(), 3);
        assert_eq!(full.usage.prompt_tokens, 900);
    }

    #[test]
    fn pinned_sessions_sort_first_then_activity() {
        let old = Session::new("m".into(), 10);
        let mut mid = Session::new("m".into(), 10);
        mid.last_message_at = Some(Utc::now());
        let mut pinned_old = Session::new("m".into(), 10);
        pinned_old.pinned = true;
        let mut v = vec![mid.clone(), old.clone(), pinned_old.clone()];
        Session::sort_sessions(&mut v);
        assert_eq!(v[0].id, pinned_old.id, "pinned first");
        assert_eq!(v[1].id, mid.id, "then latest activity");
        assert_eq!(v[2].id, old.id);
    }
}
