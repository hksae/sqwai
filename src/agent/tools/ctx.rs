//! Tool execution context: project root, session guards, read tracking.
//!
//! Moved byte-for-byte from `tools/mod.rs`; behavior unchanged.
//!
use crate::plan;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct ToolCtx {
    /// project root; every path must resolve inside it
    pub root: PathBuf,
    /// canonicalized [`Self::root`], computed once at construction so the
    /// jail check in [`Self::resolve`] does not canonicalize the root on
    /// every call. Falls back to [`Self::root`] when canonicalization fails;
    /// every tool op fails downstream in that case anyway.
    root_canon: PathBuf,
    /// secondary project instances can inspect but not mutate project state
    pub read_only: bool,
    /// Session this context belongs to. §2.5 keys the shadow snapshot chain
    /// on it (`refs/sessions/<id>`), so two sessions in one project do not
    /// interleave their checkpoint history — and retention can truncate one
    /// session's chain without touching another's.
    pub session_id: String,
    /// Override for the checkpoint chain only (§2.2.4): a child session's
    /// snapshots land on its parent's chain, so the parent's `/undo` sees
    /// the child's step boundaries and bash mutations. `None` means the own
    /// chain. Job isolation, read-guard and journal identity all stay on
    /// `session_id` — only the shadow chain is shared.
    pub checkpoint_session: Option<String>,
    /// Files read this session and the content hash they had at the time,
    /// keyed by canonical path. §4 calls for the guard to be hash-tracked: a
    /// file changed by `bash` since the last read has to be read again, and
    /// with paths alone the model could edit it blind. The canonical key also
    /// stops `read("src/x.rs")` followed by `edit("./src/x.rs")` from being
    /// refused as unread.
    pub files_read: HashMap<PathBuf, String>,
    /// journal of checkpoints created by this session's mutations
    pub journal: Vec<(String, String)>,
    /// Host limits on the plan, and the model context they are derived from.
    /// The budget used to come from a `context_limit` the model passed in its
    /// own tool arguments (§2.1.2 makes it a host value).
    pub plan_limits: crate::config::PlanConfig,
    /// context window of the model driving this session, in tokens
    pub context_limit: u64,
    /// §3.7 (§7 S): set from the TUI when the user presses Esc during a
    /// running tool. Long-running handlers poll it at their existing
    /// wait/retry points and stop cooperatively — nothing here can reach into
    /// a handler and pull it out mid-syscall, so a handler that never checks
    /// this is a handler Esc cannot interrupt.
    pub cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Where the shadow repository lives, from [undo].shadow configuration.
    pub shadow_store: crate::config::ShadowStore,
    /// Step this session currently holds (§2.2.3). Set on `plan start`,
    /// cleared on `finish`/`block`/`cancel`; the loop mirrors it into the
    /// journal attribution at dispatch. `None` means idle.
    pub current_step: Option<String>,
    /// Immutable spawn context for subagent sessions (§2.2.4). `None` for
    /// the main agent. Mutating tools refuse to run when the inherited
    /// epoch no longer matches the plan.
    pub subagent_step: Option<plan::StepContext>,
    /// Write scope for a writer subagent (`None` = unrestricted): every
    /// file mutation must sit inside these project-relative roots. Taken
    /// from the spawn registry at construction; read-only children and
    /// the main agent never set it.
    pub subagent_write_paths: Option<Vec<String>>,
    /// H1 reflector executor (§12.7): verify-only context. The dispatcher
    /// refuses every tool outside [`REFLECTOR_TOOLS`] plus mutating `bash`,
    /// with an honest message. Unlike `read_only` (a lock held elsewhere)
    /// this is a role: there is nothing to wait for or `--force` past.
    pub reflector: bool,
    /// Hard-blocked command patterns from `[safety].blocked_patterns`.
    /// The bash tool enforces them at dispatch; acceptance runners enforce
    /// the same list through `acceptance_policy_hit`, so a `cmd:` check —
    /// model-typed or project-injected via `cmd: $name` — cannot run what
    /// the user explicitly blocked. Empty in unit tests (no policy).
    pub blocked_patterns: Vec<String>,
}

impl ToolCtx {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_read_only(root, false)
    }

    pub fn with_read_only(root: impl Into<PathBuf>, read_only: bool) -> Self {
        let root = root.into();
        // Canonicalize once: `resolve` compares canonical paths, and the root
        // itself may sit behind a symlink (macOS `/var` -> `/private/var`).
        let root_canon = root.canonicalize().unwrap_or_else(|_| root.clone());
        Self {
            root,
            root_canon,
            read_only,
            session_id: "shared".into(),
            checkpoint_session: None,
            files_read: HashMap::new(),
            journal: Vec::new(),
            plan_limits: crate::config::PlanConfig::default(),
            context_limit: 0,
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            shadow_store: crate::config::ShadowStore::Local,
            current_step: None,
            subagent_step: None,
            subagent_write_paths: None,
            reflector: false,
            blocked_patterns: Vec::new(),
        }
    }

    /// Canonical path of the host-owned state dir (`.sqwai/`). Walks must
    /// skip it explicitly: `hidden(true)` checks attributes on Windows, not
    /// dotfiles, and the comparison has to run on canonical paths because
    /// the root itself may sit behind a symlink (macOS `/var`).
    pub fn host_state_dir(&self) -> PathBuf {
        self.root_canon.join(".sqwai")
    }

    pub fn with_shadow_store(mut self, shadow_store: crate::config::ShadowStore) -> Self {
        self.shadow_store = shadow_store;
        self
    }

    /// Name the session whose checkpoint chain this context appends to.
    pub fn in_session(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = session_id.into();
        self
    }

    /// Whose shadow chain snapshots go to (child → parent). Everything else
    /// keeps using `session_id`.
    pub fn checkpoint_chain(&self) -> &str {
        self.checkpoint_session
            .as_deref()
            .unwrap_or(&self.session_id)
    }

    /// Share a cancellation flag across this context and its clones. The
    /// caller keeps the `Arc` and flips it; every clone taken afterwards
    /// (including the one moved into a `spawn_blocking` closure) observes it.
    pub fn with_cancel(mut self, cancel: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.cancel = cancel;
        self
    }

    pub fn cancel_requested(&self) -> bool {
        self.cancel.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Adopt the host's plan limits and the driving model's context window.
    pub fn with_plan_limits(
        mut self,
        plan_limits: crate::config::PlanConfig,
        context_limit: u64,
    ) -> Self {
        self.plan_limits = plan_limits;
        self.context_limit = context_limit;
        self
    }

    /// Hard-blocked command patterns from `[safety].blocked_patterns`, so
    /// acceptance runners enforce the same list as the bash tool.
    pub fn with_blocked_patterns(mut self, blocked_patterns: Vec<String>) -> Self {
        self.blocked_patterns = blocked_patterns;
        self
    }

    /// resolve a user-supplied path inside the project; rejects escapes and
    /// host-owned state under `.sqwai/` (§2.0)
    pub fn resolve(&self, p: &str) -> Result<PathBuf, String> {
        let joined = if Path::new(p).is_absolute() {
            PathBuf::from(p)
        } else {
            self.root.join(p)
        };
        // canonicalize the deepest existing ancestor to defeat `..` and
        // symlinks, then re-append the part that does not exist yet, so the
        // full target is known even when the file is about to be created
        let mut anc = joined.clone();
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        while !anc.exists() {
            let Some(name) = anc.file_name().map(|n| n.to_os_string()) else {
                break;
            };
            match anc.parent() {
                Some(par) => {
                    tail.push(name);
                    anc = par.to_path_buf();
                }
                None => break,
            }
        }
        let canon = anc
            .canonicalize()
            .map_err(|e| format!("cannot resolve path {}: {e}", joined.display()))?;
        let mut resolved = canon;
        for name in tail.iter().rev() {
            resolved.push(name);
        }
        let Ok(relative) = resolved.strip_prefix(&self.root_canon) else {
            return Err(format!(
                "path '{}' escapes the project directory",
                joined.display()
            ));
        };
        // Plan, journal, memory and graph are reachable only through their own
        // tools. Enforcing it here is what makes "host-written" and
        // "append-only" guarantees rather than requests (§2.0).
        if let Some(denied) = host_owned_denial(relative) {
            return Err(denied);
        }
        // Return `joined`, not `resolved`: the whole display layer
        // (`rel_label`, checkpoint labels, `FileDiff.path`) strips the
        // *uncanonicalized* root, and where the root itself is reached
        // through a symlink (macOS `/var` -> `/private/var`) a canonical
        // path would miss that prefix and leak absolute machine-specific
        // paths into the journal. The security decision above was already
        // made in canonical space, so the spelling returned here only
        // affects labels, never the verdict.
        Ok(joined)
    }

    fn read_key(p: &Path) -> PathBuf {
        p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
    }

    pub(crate) fn mark_read(&mut self, p: &Path) {
        let hash = file_hash(p);
        self.files_read.insert(Self::read_key(p), hash);
    }

    /// Seed the read guard with files whose bytes reached the model through
    /// @-mention injection (same hash `read` records, so an edit afterwards
    /// works without a redundant read — and goes stale the same way when
    /// the file moves underneath).
    pub(crate) fn note_read(&mut self, p: &Path) {
        self.mark_read(p);
    }

    /// Whether the file may be edited: it was read, and it still holds what it
    /// held then.
    pub(crate) fn read_state(&self, p: &Path) -> ReadState {
        match self.files_read.get(&Self::read_key(p)) {
            None => ReadState::Unread,
            Some(seen) if *seen == file_hash(p) => ReadState::Current,
            Some(_) => ReadState::Stale,
        }
    }
}

/// What the read guard knows about a file the model wants to edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadState {
    /// never read in this session
    Unread,
    /// read, and unchanged since
    Current,
    /// read, but something changed it afterwards — `bash`, a formatter, the
    /// user's editor
    Stale,
}

/// Content hash of a file, matching what `file_diff` records. A missing or
/// unreadable file hashes to the empty string, which never equals a recorded
/// hash, so it reads as stale rather than as current.
fn file_hash(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    match std::fs::read(path) {
        Ok(bytes) => {
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            format!("{:x}", hasher.finalize())
        }
        Err(_) => String::new(),
    }
}

/// The agent's own state directory. File tools see only `skills/` and
/// `config.toml` inside it; plan, journal, memory and graph are host-owned and
/// reachable only through their dedicated tools (§2.0).
const STATE_DIR: &str = ".sqwai";

/// Deny message when `relative` points at host-owned state, `None` otherwise.
/// `relative` must already be relative to the canonicalized project root, so a
/// symlink pointing into `.sqwai/` cannot slip past this check.
fn host_owned_denial(relative: &Path) -> Option<String> {
    use std::path::Component;
    let mut components = relative.components();
    match components.next() {
        Some(Component::Normal(first)) if first == STATE_DIR => {}
        _ => return None,
    }
    let allowed = match components.next() {
        Some(Component::Normal(second)) => second == "skills" || second == "config.toml",
        // `.sqwai` itself: listing or writing the directory is not allowed
        _ => false,
    };
    if allowed {
        return None;
    }
    Some(format!(
        "path '{}' is host-owned state: plan, journal, memory and graph are reachable \
         only through their own tools (plan, note, memory_read, memory_propose). \
         Inside {STATE_DIR}/ the file tools may use skills/ and config.toml only.",
        relative.display()
    ))
}

/// Smallest plan budget the host will use, however small the context. A plan
/// that cannot hold its own goal line is worse than an unbudgeted one.
pub(crate) const MIN_PLAN_BUDGET_TOKENS: u64 = 256;
