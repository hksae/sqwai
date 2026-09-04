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
    /// Conversation transcript: user / assistant / tool turns only.
    ///
    /// The system prompt is **never** stored here. It is rebuilt for every
    /// request from the current project state and passed to the provider
    /// separately, so a prompt edit or an AGENTS.md change cannot resurrect a
    /// stale copy from disk.
    pub messages: Vec<Message>,
    /// Cumulative totals over the whole session — billing and statistics only.
    /// `prompt_tokens` here is the sum of every request's prompt and is
    /// therefore *not* a context meter; use `last_usage` for that.
    #[serde(default)]
    pub usage: Usage,
    /// tokens estimated from message length when the provider sent no usage
    #[serde(default)]
    pub estimated_tokens: u64,
    /// Snapshot of the most recent provider request. `prompt_tokens` is the
    /// live context occupancy — it replaces, never accumulates.
    #[serde(default)]
    pub last_usage: Option<Usage>,
    /// compaction summary of the messages dropped so far (opencode-style)
    #[serde(default)]
    pub summary: Option<String>,
    /// true once a provider actually reported cached tokens back; prompt
    /// caching is only real when it is observed, not when it is assumed
    #[serde(default)]
    pub cache_confirmed: bool,
    /// time of the latest user/agent message; drives ordering in the sessions menu
    #[serde(default)]
    pub last_message_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub pinned: bool,
    /// set when this session was created by /fork
    #[serde(default)]
    pub forked_from_id: Option<String>,
    /// plan copied for this fork, if the parent had an active plan
    #[serde(default)]
    pub plan_id: Option<String>,
    /// title snapshot of the parent, survives parent deletion
    #[serde(default)]
    pub forked_from_title: Option<String>,
    /// last provider-native response id
    #[serde(default)]
    pub last_response_id: Option<String>,
    /// model the stored response id belongs to; a continuation reference is
    /// only valid for the model that produced it
    #[serde(default)]
    pub last_response_model: Option<String>,
    /// (sha, label) undo journal: one entry per mutating agent action
    #[serde(default)]
    pub checkpoints: Vec<(String, String)>,
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
            last_usage: None,
            summary: None,
            cache_confirmed: false,
            last_message_at: None,
            pinned: false,
            forked_from_id: None,
            plan_id: None,
            forked_from_title: None,
            last_response_id: None,
            last_response_model: None,
            checkpoints: Vec::new(),
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
            f.last_usage = self.last_usage;
        }
        f.summary = self.summary.clone();
        f.cache_confirmed = self.cache_confirmed;
        f.forked_from_id = Some(self.id.to_string());
        f.plan_id = self.plan_id.clone();
        f.forked_from_title = Some(self.title.clone());
        // a continuation reference belongs to the parent conversation
        f.last_response_id = None;
        f.last_response_model = None;
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

    /// Append a conversation turn.
    ///
    /// System turns are refused on purpose: the system block is rebuilt per
    /// request and belongs to no session. Anything that needs to reach the
    /// model as instructions goes through `AgentInput::system`.
    pub fn push(&mut self, role: Role, content: impl Into<String>) {
        if role == Role::System {
            return;
        }
        let content = content.into();
        if self.messages.is_empty() && role == Role::User {
            self.title = title_from(&content);
        }
        self.estimated_tokens += (content.len() as u64).div_ceil(4);
        self.last_message_at = Some(Utc::now());
        self.messages.push(Message::new(role, content));
    }

    /// Record usage reported for **one** request.
    ///
    /// Two counters move independently:
    /// - `usage` accumulates (billing / statistics);
    /// - `last_usage` is *replaced*, never summed, and describes the size of
    ///   the current request — that is the only live context meter we have.
    pub fn add_usage(&mut self, u: &Usage) {
        if u.is_empty() {
            return;
        }
        self.usage.prompt_tokens += u.prompt_tokens;
        self.usage.completion_tokens += u.completion_tokens;
        if let Some(c) = u.cached_tokens {
            *self.usage.cached_tokens.get_or_insert(0) += c;
            // Caching is only real once the provider reports it back. A
            // documented cache key we never see a hit for stays unverified.
            if c > 0 {
                self.cache_confirmed = true;
            }
        }
        // Some providers emit a second usage event for output with zero input:
        // keep the prompt size of the request that actually reported one.
        if u.prompt_tokens > 0 {
            self.last_usage = Some(*u);
        } else if let Some(prev) = self.last_usage.as_mut() {
            prev.completion_tokens += u.completion_tokens;
        }
    }

    /// Tokens occupying the latest provider request — the live context meter.
    pub fn context_tokens_used(&self) -> u64 {
        match self.last_usage {
            Some(u) if u.prompt_tokens > 0 => u.prompt_tokens,
            _ => self.estimated_tokens,
        }
    }

    /// Cumulative tokens over the whole session: billing and statistics only.
    pub fn cumulative_tokens(&self) -> u64 {
        let reported = self.usage.total();
        if reported > 0 {
            reported
        } else {
            self.estimated_tokens
        }
    }

    /// Recompute the fallback estimate after the transcript was replaced
    /// wholesale (compaction, fork, agent outcome).
    pub fn refresh_estimate(&mut self) {
        self.estimated_tokens = self
            .messages
            .iter()
            .map(|m| (m.content.len() as u64).div_ceil(4))
            .sum();
    }

    /// true when a continuation reference may be reused for this model
    pub fn response_id_for(&self, model_key: &str) -> Option<String> {
        match &self.last_response_model {
            Some(m) if m == model_key => self.last_response_id.clone(),
            _ => None,
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
        let mut s: Self = serde_json::from_str(&raw)?;
        s.strip_system_messages();
        Ok(s)
    }

    /// Drop system messages persisted by older builds: the system block is
    /// rebuilt per request and must never come back from disk.
    pub fn strip_system_messages(&mut self) {
        let before = self.messages.len();
        self.messages.retain(|m| m.role != Role::System);
        if self.messages.len() != before {
            self.refresh_estimate();
        }
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

    /// Load only the first visible window of saved sessions. The directory is
    /// ordered by file modification time before deserializing, so opening the
    /// menu does not parse the complete conversation history of every session.
    #[allow(dead_code)]
    pub fn list_visible(limit: usize) -> Result<Vec<Self>> {
        let dir = Self::sessions_dir()?;
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                let modified = entry.metadata().and_then(|m| m.modified()).ok();
                entries.push((modified, path));
            }
        }
        entries.sort_by(|a, b| b.0.cmp(&a.0));
        let mut out = Vec::new();
        for (_, path) in entries.into_iter().take(limit.max(1)) {
            if let Some(mut session) = std::fs::read_to_string(&path)
                .ok()
                .and_then(|raw| serde_json::from_str::<Self>(&raw).ok())
            {
                session.strip_system_messages();
                out.push(session);
            }
        }
        Self::sort_sessions(&mut out);
        Ok(out)
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
                Some(mut s) => {
                    s.strip_system_messages();
                    out.push(s);
                }
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
        assert_eq!(s.cumulative_tokens(), 1100);

        // A later request reports its own prompt size; the context meter must
        // follow it instead of accumulating every previous prompt.
        s.add_usage(&Usage {
            prompt_tokens: 36_267,
            completion_tokens: 10,
            cached_tokens: None,
        });
        assert_eq!(s.context_tokens_used(), 36_267);
        assert_eq!(s.context_percent(), 100.0);
        // cumulative keeps growing on its own track
        assert_eq!(s.cumulative_tokens(), 1100 + 36_267 + 10);
    }

    #[test]
    fn cumulative_usage_never_mixes_into_the_context_meter() {
        let mut s = Session::new("m".into(), 100_000);
        for _ in 0..5 {
            s.add_usage(&Usage {
                prompt_tokens: 1_000,
                completion_tokens: 100,
                cached_tokens: None,
            });
        }
        // five identical requests: the context holds one request, not five
        assert_eq!(s.context_tokens_used(), 1_000);
        assert_eq!(s.cumulative_tokens(), 5_500);
    }

    #[test]
    fn output_only_usage_event_does_not_reset_the_prompt_size() {
        let mut s = Session::new("m".into(), 100_000);
        s.add_usage(&Usage {
            prompt_tokens: 2_000,
            completion_tokens: 0,
            cached_tokens: None,
        });
        // anthropic-style second event: output only, zero input
        s.add_usage(&Usage {
            prompt_tokens: 0,
            completion_tokens: 250,
            cached_tokens: None,
        });
        assert_eq!(s.context_tokens_used(), 2_000);
        assert_eq!(s.last_usage.unwrap().completion_tokens, 250);
    }

    #[test]
    fn system_messages_are_never_stored() {
        let mut s = Session::new("m".into(), 1000);
        s.push(Role::System, "you are an agent");
        s.push(Role::User, "hi");
        assert_eq!(s.messages.len(), 1);
        assert_eq!(s.messages[0].role, Role::User);

        // legacy files are cleaned up on load
        let mut legacy = Session::new("m".into(), 1000);
        legacy
            .messages
            .push(Message::new(Role::System, "stale system prompt"));
        legacy.messages.push(Message::new(Role::User, "hi"));
        legacy.strip_system_messages();
        assert!(legacy.messages.iter().all(|m| m.role != Role::System));
    }

    #[test]
    fn cache_is_confirmed_only_when_reported() {
        let mut s = Session::new("m".into(), 1000);
        s.add_usage(&Usage {
            prompt_tokens: 10,
            completion_tokens: 1,
            cached_tokens: Some(0),
        });
        assert!(!s.cache_confirmed, "a zero-cache report proves nothing");

        s.add_usage(&Usage {
            prompt_tokens: 10,
            completion_tokens: 1,
            cached_tokens: Some(7),
        });
        assert!(s.cache_confirmed);
        assert_eq!(s.usage.cached_tokens, Some(7));
    }

    #[test]
    fn response_id_is_scoped_to_the_model_that_produced_it() {
        let mut s = Session::new("m".into(), 1000);
        s.last_response_id = Some("resp_1".into());
        s.last_response_model = Some("local/qwen".into());
        assert_eq!(s.response_id_for("local/qwen").as_deref(), Some("resp_1"));
        assert_eq!(s.response_id_for("anthropic/sonnet"), None);
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
